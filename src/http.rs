//! Minimal HTTPS over WinHTTP: TLS and proxy are handled by Windows, no network
//! stack is embedded in the binary.

use std::ffi::c_void;

use windows::Win32::Networking::WinHttp::*;
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

pub fn get(host: &str, path: &str, headers: &[(&str, &str)]) -> Result<Response, String> {
    request("GET", host, path, headers, &[])
}

pub fn post(host: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, String> {
    request("POST", host, path, headers, body)
}

fn query_number(request: &Handle, info: u32) -> Option<u32> {
    let mut value = 0u32;
    let mut len = size_of::<u32>() as u32;
    unsafe {
        WinHttpQueryHeaders(
            request.0,
            info | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut value as *mut u32 as *mut c_void),
            &mut len,
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
    let block: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    Ok(block.encode_utf16().collect())
}

fn request(method: &str, host: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, String> {
    let header_block = header_block(headers)?;
    let body_len = u32::try_from(body.len()).map_err(|_| "request body too large".to_string())?;
    unsafe {
        let session = Handle::new(WinHttpOpen(
            w!("ai-quota/0.1"),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))?;
        WinHttpSetTimeouts(session.0, 5_000, 8_000, 10_000, 15_000).map_err(|e| e.to_string())?;
        let connect = Handle::new(WinHttpConnect(session.0, &HSTRING::from(host), 443, 0))?;
        let request = Handle::new(WinHttpOpenRequest(
            connect.0,
            &HSTRING::from(method),
            &HSTRING::from(path),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;

        // No redirects: a token must never leave for another host.
        let no_redirects = WINHTTP_DISABLE_REDIRECTS.to_le_bytes();
        WinHttpSetOption(Some(request.0), WINHTTP_OPTION_DISABLE_FEATURE, Some(&no_redirects))
            .map_err(|e| e.message())?;

        let data = if body.is_empty() { None } else { Some(body.as_ptr().cast()) };
        WinHttpSendRequest(request.0, Some(&header_block), data, body_len, body_len, 0).map_err(|e| e.message())?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| e.message())?;

        let status = query_number(&request, WINHTTP_QUERY_STATUS_CODE).ok_or_else(last_error)?;
        let retry_after = query_number(&request, WINHTTP_QUERY_RETRY_AFTER);

        let mut body = Vec::new();
        let mut chunk = vec![0u8; CHUNK];
        loop {
            let mut read = 0u32;
            WinHttpReadData(request.0, chunk.as_mut_ptr().cast(), CHUNK as u32, &mut read).map_err(|e| e.message())?;
            let read = (read as usize).min(CHUNK);
            if read == 0 {
                break;
            }
            if body.len() + read > MAX_BODY {
                return Err(format!("response larger than {} KiB", MAX_BODY / 1024));
            }
            body.extend_from_slice(&chunk[..read]);
        }
        Ok(Response { status, body, retry_after })
    }
}

/// `application/x-www-form-urlencoded` encoding of a value.
pub fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
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
