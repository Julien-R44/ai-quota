//! HTTPS minimal sur WinHTTP : TLS et proxy gérés par Windows, aucune pile
//! réseau embarquée dans le binaire.

use std::ffi::c_void;

use windows::Win32::Networking::WinHttp::*;
use windows::core::{HSTRING, PCWSTR, w};

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
    /// En-tête `Retry-After`, en secondes.
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
    let mut len = 4u32;
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

fn request(method: &str, host: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response, String> {
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

        // Aucune redirection : un token ne doit jamais partir vers un autre hôte.
        let no_redirects = WINHTTP_DISABLE_REDIRECTS.to_le_bytes();
        WinHttpSetOption(Some(request.0), WINHTTP_OPTION_DISABLE_FEATURE, Some(&no_redirects))
            .map_err(|e| e.message())?;

        let header_block: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        let header_block: Vec<u16> = header_block.encode_utf16().collect();
        let (data, len) = if body.is_empty() { (None, 0) } else { (Some(body.as_ptr().cast()), body.len() as u32) };
        WinHttpSendRequest(request.0, Some(&header_block), data, len, len, 0).map_err(|e| e.message())?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| e.message())?;

        let status = query_number(&request, WINHTTP_QUERY_STATUS_CODE).ok_or_else(last_error)?;
        let retry_after = query_number(&request, WINHTTP_QUERY_RETRY_AFTER);

        let mut body = Vec::new();
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(request.0, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut read)
                .map_err(|e| e.message())?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..read as usize]);
        }
        Ok(Response { status, body, retry_after })
    }
}

/// Encodage `application/x-www-form-urlencoded` d'une valeur.
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
    #[ignore = "réseau"]
    fn user_agent_header_replaces_session_agent() {
        let r = super::get("httpbin.org", "/headers", &[("User-Agent", "claude-cli/9.9.9 (external, cli)")]).unwrap();
        let body = String::from_utf8_lossy(&r.body);
        println!("{body}");
        assert!(body.contains("claude-cli/9.9.9 (external, cli)"));
        assert!(!body.contains("ai-quota/0.1"));
    }
}
