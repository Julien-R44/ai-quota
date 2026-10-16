#![windows_subsystem = "windows"]

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
use std::sync::atomic::{AtomicPtr, Ordering};

use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Shell::{NIN_SELECT, NINF_KEY};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use history::History;
use model::Snapshot;
use settings::Settings;
use tray::{MenuChoice, Tray};

const CLASS: PCWSTR = w!("AiQuotaHost");
const WM_TRAY: u32 = WM_APP + 1;
const WM_DATA: u32 = WM_APP + 2;
const WM_OPEN_POPUP: u32 = WM_APP + 3;

const POLL_TIMER: usize = 1;
const RESUME_TIMER: usize = 2;
const TRIM_TIMER: usize = 3;
/// Plus fréquent quand une limite approche (si activé dans les réglages).
const POLL_HOT_MS: u32 = 2 * 60 * 1000;
/// Au-delà, on rafraîchit à l'ouverture du popup (l'API Claude limite
/// vite le nombre de requêtes).
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
    /// Ligne dépliée (clé de la limite) : sa courbe est affichée.
    pub expanded: RefCell<Option<String>>,
    popup_closed_at: Cell<u64>,
    taskbar_created: u32,
}

static APP: AtomicPtr<App> = AtomicPtr::new(std::ptr::null_mut());

/// L'état global. Uniquement accédé depuis le thread UI.
pub fn app() -> &'static App {
    unsafe { &*APP.load(Ordering::Relaxed) }
}

impl App {
    /// `forced` : demandé par l'utilisateur (bouton, menu, F5), passe outre
    /// la pause après un 429. Les refresh automatiques la respectent.
    pub fn refresh(&self, forced: bool) {
        if self.loading.replace(true) {
            return;
        }
        popup::on_data();
        let hwnd = self.hwnd.0 as isize;
        let enabled = self.settings.borrow().enabled;
        let spawned = std::thread::Builder::new().stack_size(256 * 1024).spawn(move || {
            let snap = Box::into_raw(Box::new(providers::fetch_all(forced, enabled)));
            unsafe {
                if PostMessageW(Some(HWND(hwnd as _)), WM_DATA, WPARAM(0), LPARAM(snap as isize)).is_err() {
                    drop(Box::from_raw(snap));
                }
            }
        });
        if spawned.is_err() {
            self.loading.set(false);
        }
    }

    fn is_stale(&self, secs: i64) -> bool {
        self.snapshot.borrow().as_ref().is_none_or(|s| timefmt::now() - s.fetched_at > secs)
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
        let ms = if hot { POLL_HOT_MS } else { settings.poll_minutes * 60 * 1000 };
        unsafe { SetTimer(Some(self.hwnd), POLL_TIMER, ms, None) };
    }

    /// Modifie les réglages, les enregistre et les applique tout de suite.
    pub fn update_settings(&self, change: impl FnOnce(&mut Settings)) {
        let (before, after) = {
            let mut settings = self.settings.borrow_mut();
            let before = settings.clone();
            change(&mut settings);
            settings.save();
            (before, settings.clone())
        };
        i18n::set_lang(after.lang);
        // Un provider coupé disparaît tout de suite ; un provider réactivé est relu.
        if let Some(snap) = self.snapshot.borrow_mut().as_mut() {
            snap.usages.retain(|u| after.is_enabled(u.provider));
        }
        if let Some(snap) = self.snapshot.borrow().as_ref()
            && let Some(tray) = self.tray.borrow_mut().as_mut()
        {
            tray.set_snapshot(snap);
        }
        self.schedule_poll();
        if before.enabled != after.enabled && after.enabled.iter().zip(before.enabled).any(|(a, b)| *a && !b) {
            self.refresh(false);
        }
        popup::on_settings_changed(before.theme != after.theme);
    }

    /// Rend la mémoire au système une fois que les threads de travail
    /// (WinHTTP, rendu Direct2D) ont fini de se terminer.
    fn schedule_trim(&self) {
        unsafe { SetTimer(Some(self.hwnd), TRIM_TIMER, 2_000, None) };
    }

    fn toggle_popup(&self) {
        if popup::is_open() {
            popup::close();
        } else if unsafe { GetTickCount64() } - self.popup_closed_at.get() > 300 {
            // (le clic sur l'icône a déjà fermé le popup en lui retirant le focus)
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
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

pub fn popup_closed() {
    let app = app();
    app.popup_closed_at.set(unsafe { GetTickCount64() });
    app.schedule_trim();
}

fn main() {
    unsafe {
        // Une seule instance : la seconde se contente d'ouvrir le popup de la première.
        let _mutex = CreateMutexW(None, true, w!("Local\\ai-quota-single-instance"));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if let Ok(existing) = FindWindowW(CLASS, None) {
                let mut pid = 0;
                GetWindowThreadProcessId(existing, Some(&mut pid));
                let _ = AllowSetForegroundWindow(pid);
                let _ = PostMessageW(Some(existing), WM_OPEN_POPUP, WPARAM(0), LPARAM(0));
            }
            return;
        }

        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassW(&class);
        let Ok(hwnd) = CreateWindowExW(
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
        ) else {
            return;
        };

        let settings = Settings::load();
        i18n::set_lang(settings.lang);
        let app = Box::leak(Box::new(App {
            hwnd,
            tray: RefCell::new(None),
            snapshot: RefCell::new(None),
            loading: Cell::new(false),
            history: RefCell::new(History::load()),
            settings: RefCell::new(settings),
            expanded: RefCell::new(None),
            popup_closed_at: Cell::new(0),
            taskbar_created: RegisterWindowMessageW(w!("TaskbarCreated")),
        }));
        APP.store(app, Ordering::Relaxed);

        sys::enable_dark_menus(hwnd);
        *app.tray.borrow_mut() = Some(Tray::new(hwnd, WM_TRAY));
        app.refresh(false);

        if std::env::args().any(|a| a == "--open") {
            let _ = PostMessageW(Some(hwnd), WM_OPEN_POPUP, WPARAM(0), LPARAM(0));
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let app = APP.load(Ordering::Relaxed);
        if app.is_null() {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        let app = &*app;
        match msg {
            WM_TRAY => {
                match (lparam.0 & 0xFFFF) as u32 {
                    NIN_SELECT | NIN_KEYSELECT => app.toggle_popup(),
                    WM_CONTEXTMENU => {
                        popup::close();
                        let at = POINT {
                            x: (wparam.0 & 0xFFFF) as i16 as i32,
                            y: ((wparam.0 >> 16) & 0xFFFF) as i16 as i32,
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
                let snap = Box::from_raw(lparam.0 as *mut Snapshot);
                app.on_data(*snap);
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
                    // On continue session verrouillée : les tâches cloud consomment
                    // le quota, et l'historique doit les voir (un GET / 5 min).
                    POLL_TIMER => app.refresh(false),
                    RESUME_TIMER => {
                        let _ = KillTimer(Some(hwnd), RESUME_TIMER);
                        app.refresh(false);
                    }
                    TRIM_TIMER => {
                        let _ = KillTimer(Some(hwnd), TRIM_TIMER);
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
                    // on laisse le réseau revenir
                    SetTimer(Some(hwnd), RESUME_TIMER, 5_000, None);
                }
                LRESULT(1)
            }
            WM_SETTINGCHANGE => {
                let area = PCWSTR(lparam.0 as *const u16);
                if !area.is_null() && area.to_string().is_ok_and(|s| s == "ImmersiveColorSet") {
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
                PostQuitMessage(0);
                LRESULT(0)
            }
            m if m == app.taskbar_created && m != 0 => {
                // Explorer a redémarré : on réinscrit l'icône.
                if let Some(tray) = app.tray.borrow_mut().as_mut() {
                    tray.add(WM_TRAY);
                    tray.refresh_icon();
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
