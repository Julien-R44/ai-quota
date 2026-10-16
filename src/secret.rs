//! Token Amp : collé depuis le presse-papiers, chiffré avec DPAPI (lié à la
//! session Windows de l'utilisateur) dans `%LOCALAPPDATA%\ai-quota\amp.token`.

use std::path::PathBuf;

use windows::Win32::Foundation::{HGLOBAL, HLOCAL, LocalFree};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::core::w;

const CF_UNICODETEXT: u32 = 13;

fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("ai-quota").join("amp.token"))
}

fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
    CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 }
}

/// Copie le blob alloué par DPAPI puis le libère.
unsafe fn take(out: CRYPT_INTEGER_BLOB) -> Vec<u8> {
    unsafe {
        let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
        let _ = LocalFree(Some(HLOCAL(out.pbData.cast())));
        bytes
    }
}

/// Chiffre avec la clé de la session Windows (DPAPI).
fn protect(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptProtectData(&blob(data), w!("AI Quota - Amp"), None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            .ok()?;
        Some(take(out))
    }
}

fn unprotect(sealed: &[u8]) -> Option<Vec<u8>> {
    let mut out = CRYPT_INTEGER_BLOB::default();
    unsafe {
        CryptUnprotectData(&blob(sealed), None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out).ok()?;
        Some(take(out))
    }
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

/// Texte du presse-papiers, s'il y en a.
pub fn clipboard_text() -> Option<String> {
    unsafe {
        OpenClipboard(None).ok()?;
        let text = (|| {
            let handle = GetClipboardData(CF_UNICODETEXT).ok()?;
            let global = HGLOBAL(handle.0);
            let ptr = GlobalLock(global) as *const u16;
            if ptr.is_null() {
                return None;
            }
            let len = (0..).take_while(|&i| *ptr.add(i) != 0).count();
            let text = String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len));
            let _ = GlobalUnlock(global);
            Some(text)
        })();
        let _ = CloseClipboard();
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dpapi_roundtrip() {
        let sealed = protect(b"sgamp_test_token").unwrap();
        assert!(!sealed.windows(16).any(|w| w == b"sgamp_test_token"));
        assert_eq!(unprotect(&sealed).unwrap(), b"sgamp_test_token");
    }
}
