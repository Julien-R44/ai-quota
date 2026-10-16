//! Small Windows helpers: registry, theme, autostart, memory.

use std::sync::OnceLock;

use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Foundation::{FARPROC, HMODULE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::ProcessStatus::{
    K32EmptyWorkingSet, K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::SystemInformation::{GetTickCount64, OSVERSIONINFOW};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, KillTimer, PostMessageW, PostQuitMessage, SetTimer};
use windows::core::{PCSTR, w};
use windows_registry::{CURRENT_USER, Type};

const PERSONALIZE: &str = r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize";
const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "AI Quota";

/// Private memory committed by the process, in bytes.
pub fn private_memory() -> usize {
    let mut counters =
        PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    // SAFETY: The current-process pseudo-handle is valid, and counters is a writable EX struct
    // whose prefix and cb match PROCESS_MEMORY_COUNTERS as required by this API.
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
    // SAFETY: info is writable, initialized, and carries the size of its allocation.
    if unsafe { RtlGetVersion(&raw mut info) }.is_ok() { info.dwBuildNumber } else { 0 }
}

/// Undocumented uxtheme export, by ordinal. Only resolved on Windows 10 1903 (build 18362) and
/// later, where these ordinals are stable (Explorer, Notepad++ and others rely on them).
fn uxtheme_ordinal(ordinal: u16) -> FARPROC {
    static UXTHEME: OnceLock<usize> = OnceLock::new();
    let module = *UXTHEME.get_or_init(|| {
        if windows_build() < 18362 {
            return 0;
        }
        // SAFETY: The literal is NUL-terminated; retain the module for the process lifetime so
        // all resolved function pointers remain valid.
        unsafe { LoadLibraryW(w!("uxtheme.dll")) }.map_or(0, |m| m.0 as usize)
    });
    if module == 0 {
        return None;
    }
    // SAFETY: module is the retained uxtheme handle; a nonzero low-word value requests an ordinal.
    unsafe { GetProcAddress(HMODULE(module as _), PCSTR(usize::from(ordinal) as *const u8)) }
}

/// Dark context menus when Windows is dark.
pub fn enable_dark_menus(hwnd: HWND) {
    // SAFETY: These ordinals are resolved only on build >= 18362. Their system-ABI signatures
    // match SetPreferredAppMode and AllowDarkModeForWindow as used by Explorer. Called on the UI
    // thread with its window; the library remains loaded for the process lifetime.
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
        // SAFETY: On build >= 18362, ordinal 136 has the system-ABI void(void) signature used
        // by Explorer. The library remains loaded, and this is called on the UI thread.
        unsafe {
            let flush_menu_themes: unsafe extern "system" fn() = std::mem::transmute(f);
            flush_menu_themes();
        }
    }
}

/// Gives unused pages back to the system (after the popup closes).
pub fn trim_memory() {
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle which must not be closed.
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}

pub fn tick_count() -> u64 {
    // SAFETY: GetTickCount64 takes no pointers or handles and has no preconditions.
    unsafe { GetTickCount64() }
}

pub fn set_timer(hwnd: HWND, id: usize, ms: u32) {
    // SAFETY: No callback pointer is supplied; Windows validates the handle and delivers WM_TIMER
    // to the window's owning thread. This call does not dispatch messages.
    unsafe { SetTimer(Some(hwnd), id, ms, None) };
}

pub fn kill_timer(hwnd: HWND, id: usize) {
    // SAFETY: This API only removes the timer identified by the handle and ID; neither is dereferenced.
    unsafe {
        let _ = KillTimer(Some(hwnd), id);
    }
}

pub fn post_message(hwnd: HWND, msg: u32) {
    // SAFETY: Callers use only application messages with zero payloads, so no borrowed pointers
    // outlive this call. Windows validates the handle, including when the worker outlives it.
    unsafe {
        let _ = PostMessageW(Some(hwnd), msg, WPARAM(0), LPARAM(0));
    }
}

pub fn destroy_window(hwnd: HWND) {
    // SAFETY: Called on the UI thread for its owned host window, after releasing App's borrows.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

pub fn post_quit() {
    // SAFETY: Posts to the calling UI thread's queue; there are no pointer or handle arguments.
    unsafe {
        PostQuitMessage(0);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_windows_build() {
        assert!(super::windows_build() >= 10240);
    }
}
