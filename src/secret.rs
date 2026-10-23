//! Tokens pasted from the clipboard (Amp, `OpenCode`), encrypted with DPAPI (bound to the user's
//! Windows session) in `%LOCALAPPDATA%\ai-quota\<provider>.token`.

use std::path::PathBuf;

use windows::Win32::Foundation::{HGLOBAL, HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::core::HSTRING;

use crate::model::Provider;

const CF_UNICODETEXT: u32 = 13;

fn path(p: Provider) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("ai-quota");
    Some(dir.join(format!("{}.token", p.short_name().to_lowercase())))
}

struct DpapiOutput(CRYPT_INTEGER_BLOB);

impl Drop for DpapiOutput {
    fn drop(&mut self) {
        // SAFETY: Only DPAPI writes this blob. Its output is LocalAlloc-owned, and this guard
        // frees it exactly once, after the bytes have been copied. LocalFree accepts null.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(self.0.pbData.cast())));
        }
    }
}

fn crypt(data: &[u8], decrypt: bool, p: Provider) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB { cbData: u32::try_from(data.len()).ok()?, pbData: data.as_ptr().cast_mut() };
    let mut out = DpapiOutput(CRYPT_INTEGER_BLOB::default());
    let description = HSTRING::from(format!("AI Quota - {}", p.short_name()));
    // SAFETY: input points to data with a checked byte length. DPAPI does not modify input,
    // despite the binding's mutable pointer. out is writable and owned by its freeing guard;
    // successful DPAPI calls supply an initialized output allocation of cbData bytes. The
    // description stays alive for the call.
    unsafe {
        if decrypt {
            CryptUnprotectData(&raw const input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &raw mut out.0)
                .ok()?;
        } else {
            CryptProtectData(
                &raw const input,
                &description,
                None,
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &raw mut out.0,
            )
            .ok()?;
        }
        if out.0.cbData == 0 {
            Some(Vec::new())
        } else {
            Some(std::slice::from_raw_parts(out.0.pbData, out.0.cbData as usize).to_vec())
        }
    }
}

fn protect(data: &[u8], p: Provider) -> Option<Vec<u8>> {
    crypt(data, false, p)
}

fn unprotect(sealed: &[u8], p: Provider) -> Option<Vec<u8>> {
    crypt(sealed, true, p)
}

pub fn save_token(p: Provider, token: &str) -> bool {
    let (Some(path), Some(sealed)) = (path(p), protect(token.as_bytes(), p)) else { return false };
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
    std::fs::write(path, sealed).is_ok()
}

pub fn load_token(p: Provider) -> Option<String> {
    String::from_utf8(unprotect(&std::fs::read(path(p)?).ok()?, p)?).ok()
}

pub fn has_token(p: Provider) -> bool {
    path(p).is_some_and(|p| p.exists())
}

pub fn clear_token(p: Provider) {
    if let Some(path) = path(p) {
        let _ = std::fs::remove_file(path);
    }
}

pub fn clipboard_text() -> Option<String> {
    // SAFETY: OpenClipboard prevents replacement while its HGLOBAL is borrowed. GlobalLock's
    // pointer is read only through GlobalSize / size_of::<u16>() units, then unlocked before
    // closing the clipboard on every return path. CF_UNICODETEXT uses aligned UTF-16 storage.
    unsafe {
        OpenClipboard(None).ok()?;
        let text = (|| {
            let global = HGLOBAL(GetClipboardData(CF_UNICODETEXT).ok()?.0);
            let ptr = GlobalLock(global).cast::<u16>().cast_const();
            if ptr.is_null() {
                return None;
            }
            // Should be NUL-terminated, but another app may have put anything there: never read
            // past the allocation.
            let units = std::slice::from_raw_parts(ptr, GlobalSize(global) / size_of::<u16>());
            let len = units.iter().position(|&c| c == 0).unwrap_or(units.len());
            let text = String::from_utf16_lossy(&units[..len]);
            let _ = GlobalUnlock(global);
            Some(text)
        })();
        let _ = CloseClipboard();
        text
    }
}

#[cfg(test)]
mod tests {
    use super::{protect, unprotect};
    use crate::model::Provider;

    #[test]
    fn dpapi_roundtrip() {
        let token = b"sgamp_test_token";
        let sealed = protect(token, Provider::Amp).unwrap();
        assert!(!sealed.windows(token.len()).any(|w| w == token));
        assert_eq!(unprotect(&sealed, Provider::Amp).unwrap(), token);
    }

    #[test]
    fn rejects_invalid_dpapi_blob() {
        assert!(unprotect(b"not a DPAPI blob", Provider::Opencode).is_none());
    }
}
