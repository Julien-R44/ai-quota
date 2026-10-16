//! Small Windows helpers: registry, theme, autostart, memory.

use std::sync::OnceLock;

use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{FARPROC, HMODULE, HWND};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::ProcessStatus::{
    K32EmptyWorkingSet, K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::{PCSTR, w};
use windows_registry::{CURRENT_USER, Type};

const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "AI Quota";

/// Private memory committed by the process, in bytes.
pub fn private_memory() -> usize {
    let mut counters =
        PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    let ok = unsafe {
        K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(), counters.cb)
    };
    if ok.as_bool() { counters.PrivateUsage } else { 0 }
}

fn read_dword(key: &str, value: &str) -> Option<u32> {
    CURRENT_USER.open(key).and_then(|k| k.get_u32(value)).ok()
}

/// App theme (popup).
pub fn apps_dark() -> bool {
    read_dword(PERSONALIZE, "AppsUseLightTheme") == Some(0)
}

/// System theme (taskbar, hence the tray icon).
pub fn taskbar_dark() -> bool {
    read_dword(PERSONALIZE, "SystemUsesLightTheme") != Some(1)
}

pub fn autostart_enabled() -> bool {
    CURRENT_USER
        .open(RUN)
        .and_then(|k| k.get_type(RUN_VALUE))
        .is_ok_and(|t| matches!(t, Type::String | Type::ExpandString))
}

pub fn set_autostart(enabled: bool) {
    if enabled {
        let Ok(exe) = std::env::current_exe() else { return };
        let _ = CURRENT_USER.create(RUN).and_then(|k| k.set_string(RUN_VALUE, format!("\"{}\"", exe.display())));
    } else {
        let _ = CURRENT_USER.options().write().open(RUN).and_then(|k| k.remove_value(RUN_VALUE));
    }
}

/// Real build number: unlike `GetVersionEx`, `RtlGetVersion` ignores the manifest.
fn windows_build() -> u32 {
    let mut info = OSVERSIONINFOW { dwOSVersionInfoSize: size_of::<OSVERSIONINFOW>() as u32, ..Default::default() };
    if unsafe { RtlGetVersion(&mut info) }.is_ok() { info.dwBuildNumber } else { 0 }
}

/// Undocumented uxtheme export, by ordinal. Only resolved on Windows 10 1903 (build 18362) and
/// later, where these ordinals are stable (Explorer, Notepad++ and others rely on them).
fn uxtheme_ordinal(ordinal: usize) -> FARPROC {
    static UXTHEME: OnceLock<usize> = OnceLock::new();
    let module = *UXTHEME.get_or_init(|| {
        if windows_build() < 18362 {
            return 0;
        }
        unsafe { LoadLibraryW(w!("uxtheme.dll")) }.map_or(0, |m| m.0 as usize)
    });
    if module == 0 {
        return None;
    }
    unsafe { GetProcAddress(HMODULE(module as _), PCSTR(ordinal as *const u8)) }
}

/// Dark context menus when Windows is dark.
pub fn enable_dark_menus(hwnd: HWND) {
    unsafe {
        if let Some(f) = uxtheme_ordinal(135) {
            let set_preferred_app_mode: unsafe extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_preferred_app_mode(1); // AllowDark
        }
        if let Some(f) = uxtheme_ordinal(133) {
            let allow_dark_mode_for_window: unsafe extern "system" fn(HWND, bool) -> bool = std::mem::transmute(f);
            allow_dark_mode_for_window(hwnd, true);
        }
    }
    flush_menu_themes();
}

pub fn flush_menu_themes() {
    if let Some(f) = uxtheme_ordinal(136) {
        unsafe {
            let flush_menu_themes: unsafe extern "system" fn() = std::mem::transmute(f);
            flush_menu_themes();
        }
    }
}

/// Gives unused pages back to the system (after the popup closes).
pub fn trim_memory() {
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_windows_build() {
        assert!(super::windows_build() >= 10240);
    }
}
