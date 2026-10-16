//! Minimal HTTPS over `WinHTTP`: TLS and proxy are handled by Windows, no network
//! stack is embedded in the binary.

use std::ffi::c_void;
use std::fmt::Write;

use windows::Win32::Networking::WinHttp::{
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_DISABLE_REDIRECTS, WINHTTP_FLAG_SECURE,
    WINHTTP_OPTION_DISABLE_FEATURE, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_RETRY_AFTER, WINHTTP_QUERY_STATUS_CODE,
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData,
    WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetOption, WinHttpSetTimeouts,
};
use windows::core::{HSTRING, PCWSTR, w};

/// Usage responses are a few KB; anything larger is not what we asked for.
const MAX_BODY: usize = 1024 * 1024;
const CHUNK: usize = 16 * 1024;

struct Handle(*mut c_void);

impl Handle {
    fn new(h: *mut c_void) -> Result<Self, String> {
        if h.is_null() { Err(last_error()) } else { Ok(Handle(h)) }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: Handle is constructed only from successful WinHTTP calls; ownership is
        // unique and it is closed exactly once, after dependent handles have been dropped.
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

fn last_error() -> String {
    format!("{}", windows::core::Error::from_thread().code())
}

pub struct Response {
    pub status: u32,
    pub body: Vec<u8>,
    /// `Retry-After` header, in seconds.
    pub retry_after: Option<u32>,
}

/// Port: what the providers need from the network. [`WinHttp`] is the real adapter; tests plug in
/// a fake one, so the cooldown, cache and token-refresh logic is tested without Internet.
pub trait Http: Sync {
    fn get(&self, host: &str, path: &str, headers: &[(&str, &str)]) -> Result<Response, String>;
    fn post(&self, host: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, String>;
}

pub struct WinHttp;

impl Http for WinHttp {
    fn get(&self, host: &str, path: &str, headers: &[(&str, &str)]) -> Result<Response, String> {
        get(host, path, headers)
    }

    fn post(&self, host: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, String> {
        post(host, path, headers, body)
    }
}

fn get(host: &str, path: &str, headers: &[(&str, &str)]) -> Result<Response, String> {
    request("GET", host, path, headers, &[])
}

fn post(host: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, String> {
    request("POST", host, path, headers, body)
}

fn query_number(request: &Handle, info: u32) -> Option<u32> {
    let mut value = 0u32;
    let mut len = size_of::<u32>() as u32;
    // SAFETY: request is a live synchronous request. value is a writable u32, and len
    // describes that same allocation; the NUMBER flag requests exactly this representation.
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            info | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&raw mut value).cast::<c_void>()),
            &raw mut len,
            std::ptr::null_mut(),
        )
        .ok()?;
    }
    Some(value)
}

/// Header block sent as-is: a CR or LF in a value (e.g. a corrupted token file)
/// would inject extra headers.
fn header_block(headers: &[(&str, &str)]) -> Result<Vec<u16>, String> {
    if headers.iter().any(|(k, v)| k.contains(['\r', '\n']) || v.contains(['\r', '\n'])) {
        return Err("invalid header value".into());
    }
    let mut block = String::new();
    for (key, value) in headers {
        let _ = write!(block, "{key}: {value}\r\n");
    }
    Ok(block.encode_utf16().collect())
}

// Fields drop in declaration order, keeping the parents alive until the request is closed.
struct Request {
    handle: Handle,
    _connection: Handle,
    _session: Handle,
}

impl Request {
    fn open(method: &str, host: &str, path: &str) -> Result<Self, String> {
        // SAFETY: These are synchronous handles, uniquely owned by guards. Each parent stays
        // live while creating its child; HSTRING temporaries live through each call. The option
        // buffer contains exactly one DWORD and WinHttpSetOption copies it synchronously.
        unsafe {
            let session = Handle::new(WinHttpOpen(
                w!("ai-quota/0.1"),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            ))?;
            WinHttpSetTimeouts(session.0, 5_000, 8_000, 10_000, 15_000).map_err(|e| e.to_string())?;
            let connection = Handle::new(WinHttpConnect(session.0, &HSTRING::from(host), 443, 0))?;
            let handle = Handle::new(WinHttpOpenRequest(
                connection.0,
                &HSTRING::from(method),
                &HSTRING::from(path),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ))?;
            // No redirects: a token must never leave for another host.
            let no_redirects = WINHTTP_DISABLE_REDIRECTS.to_le_bytes();
            WinHttpSetOption(Some(handle.0), WINHTTP_OPTION_DISABLE_FEATURE, Some(&no_redirects))
                .map_err(|e| e.message())?;
            Ok(Self { handle, _connection: connection, _session: session })
        }
    }

    fn send(&self, headers: &[u16], body: &[u8]) -> Result<(), String> {
        let len = u32::try_from(body.len()).map_err(|_| "request body too large".to_string())?;
        let data = if body.is_empty() { None } else { Some(body.as_ptr().cast()) };
        // SAFETY: The synchronous request and its parents are live. headers is passed as a
        // counted slice, and data/len refer to the same body allocation. Both buffers stay live
        // through WinHttpReceiveResponse, as required even if authentication resends the request.
        unsafe {
            WinHttpSendRequest(self.handle.0, Some(headers), data, len, len, 0).map_err(|e| e.message())?;
            WinHttpReceiveResponse(self.handle.0, std::ptr::null_mut()).map_err(|e| e.message())
        }
    }

    fn read(&self, buffer: &mut [u8]) -> Result<usize, String> {
        let len = u32::try_from(buffer.len()).map_err(|_| "read buffer too large".to_string())?;
        let mut read = 0;
        // SAFETY: The request and parents are live; buffer is writable for the checked len.
        // Synchronous WinHTTP initializes at most len bytes and writes the count to read.
        unsafe {
            WinHttpReadData(self.handle.0, buffer.as_mut_ptr().cast(), len, &raw mut read).map_err(|e| e.message())?;
        }
        Ok((read as usize).min(buffer.len()))
    }
}

fn request(method: &str, host: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, String> {
    let headers = header_block(headers)?;
    let request = Request::open(method, host, path)?;
    request.send(&headers, body)?;
    let status = query_number(&request.handle, WINHTTP_QUERY_STATUS_CODE).ok_or_else(last_error)?;
    let retry_after = query_number(&request.handle, WINHTTP_QUERY_RETRY_AFTER);

    let mut body = Vec::new();
    let mut chunk = vec![0u8; CHUNK];
    loop {
        let read = request.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        if read > MAX_BODY - body.len() {
            return Err(format!("response larger than {} KiB", MAX_BODY / 1024));
        }
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(Response { status, body, retry_after })
}

/// `application/x-www-form-urlencoded` encoding of a value.
pub fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(char::from(b)),
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn form_encoding() {
        assert_eq!(super::form_encode("sk-ant_ort01.a~b"), "sk-ant_ort01.a~b");
        assert_eq!(super::form_encode("a+b/c=d"), "a%2Bb%2Fc%3Dd");
    }

    #[test]
    fn rejects_header_injection() {
        assert!(super::header_block(&[("Authorization", "Bearer x\r\nX-Evil: 1")]).is_err());
        let block = super::header_block(&[("Accept", "application/json")]).unwrap();
        assert_eq!(String::from_utf16(&block).unwrap(), "Accept: application/json\r\n");
    }

    #[test]
    #[ignore = "network"]
    fn user_agent_header_replaces_session_agent() {
        let r = super::get("httpbin.org", "/headers", &[("User-Agent", "claude-cli/9.9.9 (external, cli)")]).unwrap();
        let body = String::from_utf8_lossy(&r.body);
        println!("{body}");
        assert!(body.contains("claude-cli/9.9.9 (external, cli)"));
        assert!(!body.contains("ai-quota/0.1"));
    }
}
