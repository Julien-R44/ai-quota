//! Amp token: pasted from the clipboard, encrypted with DPAPI (bound to the user's Windows session)
//! in `%LOCALAPPDATA%\ai-quota\amp.token`.

use std::path::PathBuf;

use windows::Win32::Foundation::{HGLOBAL, HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::core::w;

const CF_UNICODETEXT: u32 = 13;

fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("ai-quota").join("amp.token"))
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

fn crypt(data: &[u8], decrypt: bool) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB { cbData: u32::try_from(data.len()).ok()?, pbData: data.as_ptr().cast_mut() };
    let mut out = DpapiOutput(CRYPT_INTEGER_BLOB::default());
    // SAFETY: input points to data with a checked byte length. DPAPI does not modify input,
    // despite the binding's mutable pointer. out is writable and owned by its freeing guard;
    // successful DPAPI calls supply an initialized output allocation of cbData bytes.
    unsafe {
        if decrypt {
            CryptUnprotectData(&raw const input, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &raw mut out.0)
                .ok()?;
        } else {
            CryptProtectData(
                &raw const input,
                w!("AI Quota - Amp"),
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

fn protect(data: &[u8]) -> Option<Vec<u8>> {
    crypt(data, false)
}

fn unprotect(sealed: &[u8]) -> Option<Vec<u8>> {
    crypt(sealed, true)
}

pub fn save_amp_token(token: &str) -> bool {
    let (Some(path), Some(sealed)) = (path(), protect(token.as_bytes())) else { return false };
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
    std::fs::write(path, sealed).is_ok()
}

pub fn load_amp_token() -> Option<String> {
    String::from_utf8(unprotect(&std::fs::read(path()?).ok()?)?).ok()
}

pub fn has_amp_token() -> bool {
    path().is_some_and(|p| p.exists())
}

pub fn clear_amp_token() {
    if let Some(p) = path() {
        let _ = std::fs::remove_file(p);
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

    #[test]
    fn dpapi_roundtrip() {
        let token = b"sgamp_test_token";
        let sealed = protect(token).unwrap();
        assert!(!sealed.windows(token.len()).any(|w| w == token));
        assert_eq!(unprotect(&sealed).unwrap(), token);
    }

    #[test]
    fn rejects_invalid_dpapi_blob() {
        assert!(unprotect(b"not a DPAPI blob").is_none());
    }
}
