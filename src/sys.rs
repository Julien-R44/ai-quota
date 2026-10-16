//! Petits utilitaires Windows : registre, thème, démarrage auto, mémoire.

use windows::Win32::Foundation::HWND;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::ProcessStatus::{
    K32EmptyWorkingSet, K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::Registry::*;
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::{HSTRING, PCSTR, PCWSTR, w};

const PERSONALIZE: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
const RUN: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("AI Quota");

/// Mémoire privée allouée par le process, en octets.
pub fn private_memory() -> usize {
    let mut counters =
        PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    let ok = unsafe {
        K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(), counters.cb)
    };
    if ok.as_bool() { counters.PrivateUsage } else { 0 }
}

fn read_dword(key: PCWSTR, value: PCWSTR) -> Option<u32> {
    let mut data = 0u32;
    let mut size = 4u32;
    let err = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            key,
            value,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    err.is_ok().then_some(data)
}

/// Thème des applications (popup).
pub fn apps_dark() -> bool {
    read_dword(PERSONALIZE, w!("AppsUseLightTheme")) == Some(0)
}

/// Thème du système (barre des tâches, donc icône du tray).
pub fn taskbar_dark() -> bool {
    read_dword(PERSONALIZE, w!("SystemUsesLightTheme")) != Some(1)
}

fn exe_path() -> String {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(None, &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

pub fn autostart_enabled() -> bool {
    let mut size = 0u32;
    let err = unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN, RUN_VALUE, RRF_RT_REG_SZ, None, None, Some(&mut size)) };
    err.is_ok()
}

pub fn set_autostart(enabled: bool) {
    unsafe {
        if enabled {
            let cmd = HSTRING::from(format!("\"{}\"", exe_path()));
            let bytes = std::slice::from_raw_parts(cmd.as_ptr().cast::<u8>(), (cmd.len() + 1) * 2);
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN,
                RUN_VALUE,
                REG_SZ.0,
                Some(bytes.as_ptr().cast()),
                bytes.len() as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN, RUN_VALUE);
        }
    }
}

/// Menus contextuels en sombre quand Windows l'est (API uxtheme non documentée,
/// stable depuis 1903 et utilisée par Explorer, Notepad++, etc.).
pub fn enable_dark_menus(hwnd: HWND) {
    unsafe {
        let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        let ordinal = |n: usize| GetProcAddress(uxtheme, PCSTR(n as *const u8));
        if let Some(f) = ordinal(135) {
            let set_preferred_app_mode: extern "system" fn(i32) -> i32 = std::mem::transmute(f);
            set_preferred_app_mode(1); // AllowDark
        }
        if let Some(f) = ordinal(133) {
            let allow_dark_for_window: extern "system" fn(HWND, i32) -> i32 = std::mem::transmute(f);
            allow_dark_for_window(hwnd, 1);
        }
        flush_menu_themes_with(uxtheme);
    }
}

pub fn flush_menu_themes() {
    unsafe {
        if let Ok(uxtheme) = LoadLibraryW(w!("uxtheme.dll")) {
            flush_menu_themes_with(uxtheme);
        }
    }
}

unsafe fn flush_menu_themes_with(uxtheme: windows::Win32::Foundation::HMODULE) {
    unsafe {
        if let Some(f) = GetProcAddress(uxtheme, PCSTR(136 as *const u8)) {
            let flush: extern "system" fn() = std::mem::transmute(f);
            flush();
        }
    }
}

/// Rend au système les pages qu'on n'utilise plus (après fermeture du popup).
pub fn trim_memory() {
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}
