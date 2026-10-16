use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFY_ICON_MESSAGE,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER, Shell_NotifyIconGetRect, Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CopyIcon, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, HICON, ICONINFO,
    IDI_APPLICATION, LoadIconW, MF_CHECKED, MF_SEPARATOR, MF_STRING, PostMessageW, SM_CXSMICON, SetForegroundWindow,
    TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTALIGN, TPM_RIGHTBUTTON, TrackPopupMenu, WM_NULL,
};

use crate::model::{Snapshot, Status};
use crate::{art, i18n};
use windows::core::HSTRING;

pub const TRAY_ID: u32 = 1;

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    tip: String,
}

impl Tray {
    pub fn new(hwnd: HWND, callback: u32) -> Self {
        let mut tray = Tray { hwnd, icon: HICON::default(), tip: "AI Quota".into() };
        tray.icon = make_icon(hwnd);
        tray.add(callback);
        tray
    }

    fn data(&self) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            uFlags: NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            hIcon: self.icon,
            ..Default::default()
        };
        let tip: Vec<u16> = self.tip.encode_utf16().take(nid.szTip.len() - 1).collect();
        nid.szTip[..tip.len()].copy_from_slice(&tip);
        nid
    }

    /// (Re)registers the icon, including after an Explorer restart.
    pub fn add(&self, callback: u32) {
        let mut nid = self.data();
        nid.uFlags |= NIF_MESSAGE;
        nid.uCallbackMessage = callback;
        notify_icon(NIM_ADD, &nid);
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        notify_icon(NIM_SETVERSION, &nid);
    }

    /// Theme or DPI changed: redraw the icon.
    pub fn refresh_icon(&mut self) {
        let old = self.icon;
        self.icon = make_icon(self.hwnd);
        notify_icon(NIM_MODIFY, &self.data());
        destroy_icon(old);
    }

    pub fn set_snapshot(&mut self, snap: &Snapshot) {
        let mut tip = String::from("AI Quota");
        for u in &snap.usages {
            tip.push('\n');
            tip.push_str(u.provider.short_name());
            tip.push_str(i18n::tr(" : ", ": "));
            match &u.status {
                Status::Ok(limits) if !limits.is_empty() => {
                    let parts: Vec<String> =
                        limits.iter().map(|l| format!("{} {}", i18n::percent(l.percent), l.short())).collect();
                    tip.push_str(&parts.join(" · "));
                }
                Status::Ok(_) => tip.push_str(i18n::tr("aucune limite", "no limits")),
                Status::Error(_) => tip.push_str(i18n::tr("indisponible", "unavailable")),
            }
        }
        self.tip = tip;
        notify_icon(NIM_MODIFY, &self.data());
    }

    pub fn icon_rect(&self) -> Option<RECT> {
        let id = NOTIFYICONIDENTIFIER {
            cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ID,
            ..Default::default()
        };
        // SAFETY: id has the required size and identifies this tray icon. The API copies the
        // identifier synchronously and returns a RECT by value, or an error if Explorer lost it.
        unsafe { Shell_NotifyIconGetRect(&raw const id).ok() }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        notify_icon(NIM_DELETE, &self.data());
        destroy_icon(self.icon);
    }
}

/// Monochrome icon rendered at the exact size the tray DPI asks for.
fn make_icon(hwnd: HWND) -> HICON {
    // SAFETY: Windows validates hwnd for DPI queries. The 32-bit top-down DIB allocates size*size
    // aligned pixels, written only while its bitmap is owned here. CreateIconIndirect copies
    // both bitmaps before they are deleted; CopyIcon makes the fallback icon independently owned.
    unsafe {
        let dpi = GetDpiForWindow(hwnd).max(96);
        let size = GetSystemMetricsForDpi(SM_CXSMICON, dpi).max(16) as u32;
        let rgb: u32 = if crate::sys::taskbar_dark() { 0xFFFFFF } else { 0x1A1A1A };
        let cov = art::tray_glyph(size);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size as i32,
                biHeight: -(size as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let Ok(color) = CreateDIBSection(None, &raw const bmi, DIB_RGB_COLORS, &raw mut bits, None, 0) else {
            return LoadIconW(None, IDI_APPLICATION).and_then(|icon| CopyIcon(icon)).unwrap_or_default();
        };
        let px = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (size * size) as usize);
        for (dst, a) in px.iter_mut().zip(cov) {
            *dst = ((a * 255.0).round() as u32) << 24 | rgb;
        }
        let mask = CreateBitmap(size as i32, size as i32, 1, 1, None);
        let info = ICONINFO { fIcon: true.into(), hbmMask: mask, hbmColor: color, ..Default::default() };
        let icon = CreateIconIndirect(&raw const info).unwrap_or_default();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

pub enum MenuChoice {
    Refresh,
    ToggleAutostart,
    Settings,
    Quit,
}

pub fn show_menu(hwnd: HWND, at: POINT, autostart: bool) -> Option<MenuChoice> {
    match menu_command(hwnd, at, autostart)? {
        1 => Some(MenuChoice::Refresh),
        2 => Some(MenuChoice::ToggleAutostart),
        3 => Some(MenuChoice::Quit),
        4 => Some(MenuChoice::Settings),
        _ => None,
    }
}

fn menu_command(hwnd: HWND, at: POINT, autostart: bool) -> Option<i32> {
    // SAFETY: menu is created and destroyed once on this UI thread. AppendMenuW copies the
    // temporary strings. TrackPopupMenu borrows the menu and owner for its modal loop, with no
    // App/Popup RefCell borrow held by the caller. The posted WM_NULL carries no pointers.
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let item = |fr, en| HSTRING::from(i18n::tr(fr, en));
        let _ = AppendMenuW(menu, MF_STRING, 1, &item("Actualiser", "Refresh"));
        let _ = AppendMenuW(menu, MF_STRING, 4, &item("Réglages…", "Settings…"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let flags = if autostart { MF_STRING | MF_CHECKED } else { MF_STRING };
        let _ = AppendMenuW(menu, flags, 2, &item("Lancer au démarrage de Windows", "Start with Windows"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, 3, &item("Quitter", "Quit"));

        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_RIGHTALIGN,
            at.x,
            at.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM::default(), LPARAM::default());
        let _ = DestroyMenu(menu);
        Some(cmd.0)
    }
}

fn notify_icon(action: NOTIFY_ICON_MESSAGE, data: &NOTIFYICONDATAW) {
    // SAFETY: data is fully initialized, includes cbSize, and stays live while Shell_NotifyIconW
    // copies its fields. For ADD/MODIFY its icon remains owned by Tray until after this call.
    unsafe {
        let _ = Shell_NotifyIconW(action, data);
    }
}

fn destroy_icon(icon: HICON) {
    // SAFETY: make_icon returns an owned CreateIconIndirect/CopyIcon handle, destroyed once
    // when replaced or dropped. A null handle from a failed creation is rejected by Windows.
    unsafe {
        let _ = DestroyIcon(icon);
    }
}
