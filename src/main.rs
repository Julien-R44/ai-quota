#![windows_subsystem = "windows"]

//! `App` lives on the UI thread and is reached through thread-local storage, so workers cannot
//! borrow it. `RefCell` borrows must end before calling anything that pumps window messages.

mod art;
mod history;
mod http;
mod i18n;
mod logos;
mod model;
mod popup;
mod providers;
mod secret;
mod settings;
mod svgpath;
mod sys;
mod timefmt;
mod tray;

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::sync::mpsc::{self, Receiver};

use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{NIN_SELECT, NINF_KEY};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW, GetMessageW,
    GetWindowThreadProcessId, MSG, RegisterClassW, RegisterWindowMessageW, TranslateMessage, WM_APP, WM_CONTEXTMENU,
    WM_DESTROY, WM_DISPLAYCHANGE, WM_DPICHANGED, WM_POWERBROADCAST, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};
use windows::core::{PCWSTR, w};

use history::History;
use model::{Provider, Snapshot};
use settings::Settings;
use tray::{MenuChoice, Tray};

const CLASS: PCWSTR = w!("AiQuotaHost");
const WM_TRAY: u32 = WM_APP + 1;
const WM_DATA: u32 = WM_APP + 2;
const WM_OPEN_POPUP: u32 = WM_APP + 3;

const POLL_TIMER: usize = 1;
const RESUME_TIMER: usize = 2;
const TRIM_TIMER: usize = 3;
/// Used while a limit is close (if enabled in the settings).
const POLL_HOT_MS: u32 = 2 * 60 * 1000;
/// Older data is refreshed when the popup opens (no more often: the Claude API rate-limits quickly).
const STALE_SECS: i64 = 2 * 60;

const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
const NIN_KEYSELECT: u32 = NIN_SELECT | NINF_KEY;

pub struct App {
    hwnd: HWND,
    tray: RefCell<Option<Tray>>,
    pub snapshot: RefCell<Option<Snapshot>>,
    pub loading: Cell<bool>,
    pub history: RefCell<History>,
    pub settings: RefCell<Settings>,
    /// Expanded row (limit key): its chart is shown.
    pub expanded: RefCell<Option<model::LimitKey>>,
    popup_closed_at: Cell<u64>,
    taskbar_created: u32,
    // Explicitly disconnected on window destruction because App itself is process-lived.
    data_rx: RefCell<Option<Receiver<Snapshot>>>,
}

thread_local! {
    /// Only ever set on the UI thread, and `App` is not `Sync`: no other thread can reach it.
    static APP: Cell<Option<&'static App>> = const { Cell::new(None) };
}

/// UI thread only.
pub fn app() -> &'static App {
    APP.get().expect("app() called before init or off the UI thread")
}

impl App {
    /// `forced`: requested by the user (button, menu, F5), skips the back-off after a 429.
    /// Automatic refreshes honor it.
    pub fn refresh(&self, forced: bool) {
        if self.loading.replace(true) {
            return;
        }
        popup::on_data();
        let hwnd = self.hwnd.0 as usize;
        let enabled = self.settings.borrow().enabled;
        let (tx, rx) = mpsc::channel();
        *self.data_rx.borrow_mut() = Some(rx);
        let spawned = std::thread::Builder::new().stack_size(256 * 1024).spawn(move || {
            if tx.send(providers::fetch_all(forced, enabled)).is_ok() {
                sys::post_message(HWND(hwnd as _), WM_DATA);
            }
        });
        if spawned.is_err() {
            self.data_rx.borrow_mut().take();
            self.loading.set(false);
        }
    }

    fn drain_data(&self) {
        loop {
            // Release the borrow before updating the UI, which may dispatch more messages.
            let snap = self.data_rx.borrow().as_ref().and_then(|rx| rx.try_recv().ok());
            let Some(snap) = snap else { break };
            self.on_data(snap);
        }
    }

    fn is_stale(&self, secs: i64) -> bool {
        self.snapshot.borrow().as_ref().is_none_or(|s| timefmt::now().saturating_sub(s.fetched_at) > secs)
    }

    fn on_data(&self, snap: Snapshot) {
        self.history.borrow_mut().record(&snap);
        if let Some(tray) = self.tray.borrow_mut().as_mut() {
            tray.set_snapshot(&snap);
        }
        *self.snapshot.borrow_mut() = Some(snap);
        self.loading.set(false);
        self.schedule_poll();
        popup::on_data();
        self.schedule_trim();
    }

    fn schedule_poll(&self) {
        let settings = self.settings.borrow();
        let hot =
            settings.poll_faster_when_hot && self.snapshot.borrow().as_ref().is_some_and(|s| s.max_percent() >= 80.0);
        let ms = if hot { POLL_HOT_MS } else { settings.poll_minutes.saturating_mul(60_000) };
        sys::set_timer(self.hwnd, POLL_TIMER, ms);
    }

    /// Changes the settings, saves them and applies them right away.
    pub fn update_settings(&self, change: impl FnOnce(&mut Settings)) {
        let (before, after) = {
            let mut settings = self.settings.borrow_mut();
            let before = settings.clone();
            change(&mut settings);
            settings.save();
            (before, settings.clone())
        };
        i18n::set_lang(after.lang);
        // A disabled provider disappears right away; a re-enabled one is fetched again.
        if let Some(snap) = self.snapshot.borrow_mut().as_mut() {
            snap.usages.retain(|u| after.enabled[u.provider]);
        }
        if let Some(snap) = self.snapshot.borrow().as_ref()
            && let Some(tray) = self.tray.borrow_mut().as_mut()
        {
            tray.set_snapshot(snap);
        }
        self.schedule_poll();
        if Provider::ALL.into_iter().any(|p| after.enabled[p] && !before.enabled[p]) {
            self.refresh(false);
        }
        popup::on_settings_changed(before.theme != after.theme);
    }

    /// Gives memory back to the system once worker threads (`WinHTTP`, `Direct2D` rendering) are done
    /// exiting.
    fn schedule_trim(&self) {
        sys::set_timer(self.hwnd, TRIM_TIMER, 2_000);
    }

    fn toggle_popup(&self) {
        // Clicking the icon of an open popup closes it first (focus loss): the delay keeps that same
        // click from reopening it.
        if popup::is_open() {
            popup::close();
        } else if sys::tick_count().saturating_sub(self.popup_closed_at.get()) > 300 {
            self.open_popup();
        }
    }

    fn open_popup(&self) {
        let anchor = self.tray.borrow().as_ref().and_then(Tray::icon_rect);
        popup::open(anchor);
        if self.is_stale(STALE_SECS) {
            self.refresh(false);
        }
    }

    fn quit(&self) {
        popup::close();
        self.tray.borrow_mut().take();
        sys::destroy_window(self.hwnd);
    }
}

pub fn popup_closed() {
    let app = app();
    app.popup_closed_at.set(sys::tick_count());
    app.schedule_trim();
}

/// With `panic = "abort"` and a Windows-subsystem exe, a panic (say, a `RefCell` double borrow)
/// would kill the app without a trace: leave one in `%LOCALAPPDATA%\ai-quota\crash.log`.
fn install_crash_log() {
    const MAX_LOG: u64 = 64 * 1024;
    std::panic::set_hook(Box::new(|info| {
        let Some(dir) = std::env::var_os("LOCALAPPDATA") else { return };
        let path = std::path::PathBuf::from(dir).join("ai-quota").join("crash.log");
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
        let full = std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_LOG);
        let file = std::fs::OpenOptions::new().create(true).write(true).append(!full).truncate(full).open(&path);
        if let Ok(mut file) = file {
            let thread = std::thread::current();
            let _ = writeln!(
                file,
                "{} [{}] panicked at {}: {}",
                timefmt::utc_iso8601(timefmt::now()),
                thread.name().unwrap_or("?"),
                info.location().map_or_else(|| "?".into(), ToString::to_string),
                info.payload_as_str().unwrap_or("?"),
            );
        }
    }));
}

fn main() {
    install_crash_log();
    let Some(_instance) = acquire_instance() else { return };
    let Some(hwnd) = create_host_window() else { return };
    let settings = Settings::load();
    i18n::set_lang(settings.lang);
    // SAFETY: The message name is a static NUL-terminated string.
    let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
    let app = Box::leak(Box::new(App {
        hwnd,
        tray: RefCell::new(None),
        snapshot: RefCell::new(None),
        loading: Cell::new(false),
        history: RefCell::new(History::load()),
        settings: RefCell::new(settings),
        expanded: RefCell::new(None),
        popup_closed_at: Cell::new(0),
        taskbar_created,
        data_rx: RefCell::new(None),
    }));
    APP.set(Some(app));

    sys::enable_dark_menus(hwnd);
    *app.tray.borrow_mut() = Some(Tray::new(hwnd, WM_TRAY));
    app.refresh(false);
    if std::env::args().any(|a| a == "--open") {
        sys::post_message(hwnd, WM_OPEN_POPUP);
    }
    run_message_loop();
    // Also disconnect on a GetMessageW error, without relying on WM_DESTROY being dispatched.
    app.data_rx.borrow_mut().take();
}

struct InstanceMutex(Option<HANDLE>);

impl Drop for InstanceMutex {
    fn drop(&mut self) {
        if let Some(handle) = self.0 {
            // SAFETY: This is the uniquely owned handle returned by CreateMutexW, closed once
            // after the message loop exits. Windows destroys the mutex after its last handle closes.
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
    }
}

fn acquire_instance() -> Option<InstanceMutex> {
    // SAFETY: The mutex has default security and a static NUL-terminated name. Its successful
    // handle is owned by the guard. GetLastError is read before any other Windows API call.
    let (mutex, already_exists) = unsafe {
        let mutex = CreateMutexW(None, true, w!("Local\\ai-quota-single-instance"));
        (mutex, GetLastError() == ERROR_ALREADY_EXISTS)
    };
    let mutex = InstanceMutex(mutex.ok());
    if already_exists {
        open_existing_instance();
        None
    } else {
        Some(mutex)
    }
}

fn open_existing_instance() {
    // SAFETY: CLASS is NUL-terminated, pid is writable, and the post carries no pointers. Windows
    // validates the other process's window if it exits between lookup and posting.
    unsafe {
        if let Ok(existing) = FindWindowW(CLASS, None) {
            let mut pid = 0;
            GetWindowThreadProcessId(existing, Some(&raw mut pid));
            let _ = AllowSetForegroundWindow(pid);
            sys::post_message(existing, WM_OPEN_POPUP);
        }
    }
}

fn create_host_window() -> Option<HWND> {
    // SAFETY: The module stays loaded for the process lifetime. class and all string literals
    // live through registration/creation; wndproc has the required system ABI. Window creation
    // and all subsequent dispatch happen on this UI thread.
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassW(&raw const class);
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            CLASS,
            w!("AI Quota"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .ok()
    }
}

fn run_message_loop() {
    let mut msg = MSG::default();
    // SAFETY: msg is a writable MSG allocation. Only a positive GetMessageW result is dispatched,
    // on this UI thread, with no App RefCell borrow held across dispatch or retrieval.
    unsafe {
        while GetMessageW(&raw mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Messages sent while the window is being created, before `App` exists.
    let Some(app) = APP.get() else { return default_window_proc(hwnd, msg, wparam, lparam) };
    match msg {
        WM_TRAY => {
            match (lparam.0 & 0xFFFF) as u32 {
                NIN_SELECT | NIN_KEYSELECT => app.toggle_popup(),
                WM_CONTEXTMENU => {
                    popup::close();
                    let at = POINT {
                        x: i32::from((wparam.0 & 0xFFFF) as i16),
                        y: i32::from(((wparam.0 >> 16) & 0xFFFF) as i16),
                    };
                    match tray::show_menu(hwnd, at, sys::autostart_enabled()) {
                        Some(MenuChoice::Refresh) => app.refresh(true),
                        Some(MenuChoice::ToggleAutostart) => sys::set_autostart(!sys::autostart_enabled()),
                        Some(MenuChoice::Settings) => {
                            app.open_popup();
                            popup::show_settings();
                        }
                        Some(MenuChoice::Quit) => app.quit(),
                        None => {}
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DATA => {
            app.drain_data();
            LRESULT(0)
        }
        WM_OPEN_POPUP => {
            if !popup::is_open() {
                app.open_popup();
            }
            LRESULT(0)
        }
        WM_TIMER => {
            match wparam.0 {
                // Keep polling while the session is locked: cloud tasks consume the quota and the
                // history must see them (one GET every 5 min).
                POLL_TIMER => app.refresh(false),
                RESUME_TIMER => {
                    sys::kill_timer(hwnd, RESUME_TIMER);
                    app.refresh(false);
                }
                TRIM_TIMER => {
                    sys::kill_timer(hwnd, TRIM_TIMER);
                    if !popup::is_open() {
                        sys::trim_memory();
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            if wparam.0 == PBT_APMRESUMEAUTOMATIC {
                // let the network come back first
                sys::set_timer(hwnd, RESUME_TIMER, 5_000);
            }
            LRESULT(1)
        }
        WM_SETTINGCHANGE => {
            let area = PCWSTR(lparam.0 as *const u16);
            // SAFETY: For WM_SETTINGCHANGE, Windows supplies a NUL-terminated area string
            // valid for the duration of this callback, or a null pointer.
            let theme_changed = !area.is_null() && unsafe { area.to_string() }.is_ok_and(|s| s == "ImmersiveColorSet");
            if theme_changed {
                if let Some(tray) = app.tray.borrow_mut().as_mut() {
                    tray.refresh_icon();
                }
                sys::flush_menu_themes();
                popup::on_theme_changed();
            }
            LRESULT(0)
        }
        WM_DISPLAYCHANGE | WM_DPICHANGED => {
            if let Some(tray) = app.tray.borrow_mut().as_mut() {
                tray.refresh_icon();
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            app.data_rx.borrow_mut().take();
            APP.set(None);
            sys::post_quit();
            LRESULT(0)
        }
        m if m == app.taskbar_created && m != 0 => {
            // Explorer restarted: register the icon again.
            if let Some(tray) = app.tray.borrow_mut().as_mut() {
                tray.add(WM_TRAY);
                tray.refresh_icon();
            }
            LRESULT(0)
        }
        _ => default_window_proc(hwnd, msg, wparam, lparam),
    }
}

fn default_window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: Only called from wndproc with the unchanged parameters supplied by Windows.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
