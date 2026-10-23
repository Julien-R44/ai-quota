//! The flyout: a Win32 popup window on a DWM Acrylic backdrop, drawn with `Direct2D`. Created on click and destroyed
//! on close, so it costs nothing while hidden.
//!
//! Geometry has a single source of truth: [`Layout`], computed from the current state. The window size and the
//! drawing both read it, so they cannot disagree.

mod chart;
mod gfx;
mod main_view;
mod settings_view;
mod theme;

use std::cell::RefCell;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D_SIZE_U};
use windows::Win32::Graphics::Dwm::{
    DWM_SYSTEMBACKDROP_TYPE, DWM_WINDOW_CORNER_PREFERENCE, DWMSBT_TRANSIENTWINDOW, DWMWA_SYSTEMBACKDROP_TYPE,
    DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DWMWINDOWATTRIBUTE,
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, MonitorFromRect,
    ValidateRect,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE};
use windows::Win32::UI::Shell::{
    ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETTASKBARPOS, APPBARDATA, SHAppBarMessage, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, CreateWindowExW, DefWindowProcW, DestroyWindow, GetCursorPos, GetForegroundWindow, GetWindowRect,
    GetWindowThreadProcessId, HTCLIENT, IDC_ARROW, KillTimer, LoadCursorW, PostMessageW, RegisterClassW, SW_SHOW,
    SW_SHOWNORMAL, SWP_NOACTIVATE, SWP_NOZORDER, SetForegroundWindow, SetTimer, SetWindowPos, ShowWindow, WA_INACTIVE,
    WM_ACTIVATE, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_NCACTIVATE, WM_NCCALCSIZE, WM_NCHITTEST, WM_NCPAINT, WM_PAINT, WM_TIMER, WNDCLASSW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_THICKFRAME,
};
use windows::core::{BOOL, HSTRING, PCWSTR, w};
use windows_numerics::Matrix3x2;

use crate::model::{LimitKey, Provider, Snapshot};
use crate::{app, secret, sys};
use gfx::Gfx;
use main_view::MainLayout;
use settings_view::{Ctl, ProviderLayout, SettingsCtx, SettingsLayout};
use theme::Theme;

// Layout, in DIP.
const WIDTH: f32 = 360.0;
const MARGIN: f32 = 12.0;
const HEADER_H: f32 = 54.0;
const SCREEN_GAP: f32 = 12.0;

const CLASS: PCWSTR = w!("AiQuotaPopup");
const ANIM_TIMER: usize = 1;
const CLOCK_TIMER: usize = 2;
/// Ends a pending confirmation ("Clear history") or a token note.
const CONFIRM_TIMER: usize = 3;
const CONFIRM_SECS: f32 = 4.0;

fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

fn ease_out(t: f32, power: i32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(power)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Main,
    Settings,
    /// A provider's own settings (the Amp token).
    Provider(Provider),
}

impl View {
    /// Navigation depth, for the slide direction.
    fn depth(self) -> u8 {
        match self {
            View::Main => 0,
            View::Settings => 1,
            View::Provider(_) => 2,
        }
    }
}

/// Positions and sizes of every block of the current view, and the window height they need.
enum Layout<'a> {
    Main(MainLayout<'a>),
    Settings(SettingsLayout),
    Provider(Provider, ProviderLayout),
}

impl<'a> Layout<'a> {
    fn new(view: View, snap: Option<&'a Snapshot>, expanded: Option<&LimitKey>) -> Self {
        match view {
            View::Main => Layout::Main(main_view::layout(snap, expanded)),
            View::Settings => Layout::Settings(settings_view::layout()),
            View::Provider(p) => Layout::Provider(p, settings_view::provider_layout()),
        }
    }

    fn height(&self) -> f32 {
        match self {
            Layout::Main(l) => l.height,
            Layout::Settings(l) => l.height,
            Layout::Provider(_, l) => l.height,
        }
    }
}

fn layout_height(view: View) -> f32 {
    let app = app();
    let (snap, expanded) = (app.snapshot.borrow(), app.expanded.borrow());
    Layout::new(view, snap.as_ref(), expanded.as_ref()).height()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Refresh,
    /// Link to the provider's usage page.
    Open(Provider),
    /// A limit row (index into `Hits::rows`): toggles its chart.
    Row(usize),
    /// Header cog: Settings view.
    Gear,
    Back,
    Ctl(Ctl),
}

/// Clickable areas of the last frame, and the hovered one.
#[derive(Default)]
struct Hits {
    hover: Option<Hit>,
    areas: Vec<(D2D_RECT_F, Hit)>,
    /// Keys of the limit rows drawn, in order: the targets of `Hit::Row`.
    rows: Vec<LimitKey>,
}

impl Hits {
    fn clear(&mut self) {
        self.areas.clear();
        self.rows.clear();
    }

    /// Registers an area; returns whether it is hovered. Later areas win over earlier ones.
    fn add(&mut self, r: D2D_RECT_F, hit: Hit) -> bool {
        self.areas.push((r, hit));
        self.hover == Some(hit)
    }

    fn add_row(&mut self, r: D2D_RECT_F, key: &LimitKey) -> bool {
        let hit = Hit::Row(self.rows.len());
        self.rows.push(key.clone());
        self.add(r, hit)
    }

    fn at(&self, x: f32, y: f32) -> Option<Hit> {
        let inside = |r: &D2D_RECT_F| x >= r.left && x < r.right && y >= r.top && y < r.bottom;
        self.areas.iter().rev().find(|(r, _)| inside(r)).map(|(_, h)| *h)
    }
}

struct Anim {
    /// Seconds since the popup opened.
    t: f32,
    /// Progress of the last chart expansion, 0..1.
    reveal: f32,
    loading: bool,
    /// Transform of the content (opening and view-change slide).
    base: Matrix3x2,
    /// Vertical part of `base`.
    dy: f32,
}

// These flags track independent settings and Win32 subscriptions, not mutually exclusive states.
#[allow(clippy::struct_excessive_bools)]
struct Popup {
    hwnd: HWND,
    gfx: Gfx,
    dpi: f32,
    theme: Theme,
    anchor: Option<RECT>,
    /// Height (DIP) the window was last sized for.
    height: f32,
    opened: Instant,
    pressed: Option<Hit>,
    hits: Hits,
    /// Last chart expansion (reveal animation).
    chart_opened: Instant,
    view: View,
    /// Last view change (slide animation).
    view_changed: Instant,
    /// Slide direction: +1 going deeper, -1 going back.
    slide: f32,
    /// Autostart state, read from the registry when entering Settings.
    autostart: bool,
    /// "Clear history" awaits a confirmation since then.
    confirm_clear: Option<Instant>,
    /// Process memory, read when entering Settings.
    memory: String,
    /// Feedback on the last token action, shown on the provider page.
    token_note: Option<(String, Instant)>,
    tracking: bool,
    animating: bool,
}

thread_local! {
    static POPUP: RefCell<Option<Popup>> = const { RefCell::new(None) };
}

fn with_popup<R>(f: impl FnOnce(&mut Popup) -> R) -> Option<R> {
    POPUP.with(|p| p.try_borrow_mut().ok()?.as_mut().map(f))
}

fn invalidate(hwnd: HWND) {
    // SAFETY: The handle identifies this module's popup on the UI thread; None requests the entire client area without
    // a rectangle pointer.
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn destroy_window(hwnd: HWND) {
    // SAFETY: The module owns this live HWND on the UI thread and releases POPUP borrows before destruction.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

fn stop_timer(hwnd: HWND, id: usize) {
    // SAFETY: The timer belongs to the live popup HWND on its owning UI thread.
    unsafe {
        let _ = KillTimer(Some(hwnd), id);
    }
}

pub fn is_open() -> bool {
    POPUP.with(|p| p.try_borrow().map_or(true, |p| p.is_some()))
}

pub fn close() {
    if let Some(hwnd) = with_popup(|p| p.hwnd) {
        destroy_window(hwnd);
    }
}

/// The data or the loading state changed.
pub fn on_data() {
    if let Some(hwnd) = with_popup(|p| {
        p.place();
        p.animate();
        p.hwnd
    }) {
        invalidate(hwnd);
    }
}

/// Opens the Settings view (from the tray menu).
pub fn show_settings() {
    if let Some(hwnd) = with_popup(|p| {
        p.set_view(View::Settings);
        p.hwnd
    }) {
        invalidate(hwnd);
    }
}

pub fn on_settings_changed(theme_changed: bool) {
    if theme_changed {
        on_theme_changed();
    }
    on_data();
}

pub fn on_theme_changed() {
    with_popup(|p| {
        p.theme = Theme::current();
        p.apply_dwm();
        invalidate(p.hwnd);
    });
}

/// Monitor and DPI for a popup anchored on the tray icon.
fn monitor_for(anchor: Option<RECT>) -> (MONITORINFO, f32) {
    // SAFETY: Monitor handles come from Win32; each output pointer refers to initialized, writable local storage with
    // the required cbSize.
    unsafe {
        let monitor = if let Some(r) = anchor {
            MonitorFromRect(&raw const r, MONITOR_DEFAULTTONEAREST)
        } else {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&raw mut pt);
            MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
        };
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &raw mut info);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dx, &raw mut dy);
        (info, dx as f32)
    }
}

/// Taskbar rectangle and edge. Also works when it auto-hides, in which case the work area does not exclude it.
fn taskbar() -> Option<(RECT, u32)> {
    let mut abd = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, ..Default::default() };
    // SAFETY: abd has the required cbSize and remains writable for the synchronous query; the shell does not retain its
    // pointer.
    let ok = unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &raw mut abd) } != 0;
    ok.then_some((abd.rc, abd.uEdge))
}

/// Windows 11 flyout placement: against the taskbar, centered on the icon when possible, never off-screen.
fn placement(anchor: Option<RECT>, w: i32, h: i32) -> (i32, i32, f32) {
    let (info, dpi) = monitor_for(anchor);
    let (mon, mut area) = (info.rcMonitor, info.rcWork);
    let gap = (SCREEN_GAP * dpi / 96.0).round() as i32;

    let mut edge = ABE_BOTTOM;
    if let Some((tb, tb_edge)) = taskbar() {
        let (cx, cy) = (i32::midpoint(tb.left, tb.right), i32::midpoint(tb.top, tb.bottom));
        if cx >= mon.left && cx < mon.right && cy >= mon.top && cy < mon.bottom {
            edge = tb_edge;
            match edge {
                ABE_LEFT => area.left = area.left.max(tb.right),
                ABE_TOP => area.top = area.top.max(tb.bottom),
                ABE_RIGHT => area.right = area.right.min(tb.left),
                _ => area.bottom = area.bottom.min(tb.top),
            }
        }
    }

    let icon =
        anchor.unwrap_or(RECT { left: area.right - 40, top: area.bottom, right: area.right, bottom: area.bottom });
    let (icx, icy) = (i32::midpoint(icon.left, icon.right), i32::midpoint(icon.top, icon.bottom));
    let clamp_x = |x: i32| x.clamp(area.left + gap, (area.right - w - gap).max(area.left + gap));
    let clamp_y = |y: i32| y.clamp(area.top + gap, (area.bottom - h - gap).max(area.top + gap));
    let (x, y) = match edge {
        ABE_LEFT => (area.left + gap, clamp_y(icy - h / 2)),
        ABE_TOP => (clamp_x(icx - w / 2), area.top + gap),
        ABE_RIGHT => (area.right - w - gap, clamp_y(icy - h / 2)),
        _ => (clamp_x(icx - w / 2), area.bottom - h - gap),
    };
    (x, y, dpi)
}

fn window_size(hwnd: HWND) -> D2D_SIZE_U {
    let mut rc = RECT::default();
    // SAFETY: The module's popup handle is queried on its UI thread; rc is writable local storage for the returned
    // rectangle.
    unsafe {
        let _ = GetWindowRect(hwnd, &raw mut rc);
    }
    D2D_SIZE_U { width: (rc.right - rc.left).max(1) as u32, height: (rc.bottom - rc.top).max(1) as u32 }
}

fn size_px(dpi: f32, height: f32) -> (i32, i32) {
    let scale = dpi / 96.0;
    ((WIDTH * scale).round() as i32, (height * scale).round() as i32)
}

pub fn open(anchor: Option<RECT>) {
    if is_open() {
        return;
    }
    let (_, dpi) = monitor_for(anchor);
    let height = layout_height(View::Main);
    let (w, h) = size_px(dpi, height);
    let (x, y, _) = placement(anchor, w, h);
    // SAFETY: This runs on the UI thread. The callback uses the system ABI; class and title strings are static,
    // and the class structure outlives registration. The returned HWND is owned by this module.
    let (hwnd, dpi) = unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&raw const class);

        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("AI Quota"),
            // A resizable frame whose non-client area is hidden (WM_NCCALCSIZE): that is what lets DWM apply the
            // Acrylic backdrop and the system shadow, which it refuses to a bare WS_POPUP.
            WS_POPUP | WS_THICKFRAME,
            x,
            y,
            w,
            h,
            None,
            None,
            Some(instance.into()),
            None,
        ) else {
            return;
        };
        (hwnd, GetDpiForWindow(hwnd) as f32)
    };
    let Ok(gfx) = Gfx::new(hwnd, dpi) else {
        destroy_window(hwnd);
        return;
    };
    let popup = Popup {
        hwnd,
        gfx,
        dpi,
        theme: Theme::current(),
        anchor,
        height,
        opened: Instant::now(),
        pressed: None,
        hits: Hits::default(),
        tracking: false,
        chart_opened: Instant::now(),
        view: View::Main,
        slide: 1.0,
        view_changed: Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now),
        autostart: false,
        confirm_clear: None,
        memory: String::new(),
        token_note: None,
        animating: false,
    };
    popup.apply_dwm();
    POPUP.with(|p| *p.borrow_mut() = Some(popup));
    with_popup(Popup::animate);
    with_popup(|p| p.start_timer(CLOCK_TIMER, 30_000));

    show_window(hwnd);
}

/// Windows refuses `SetForegroundWindow` when the popup is not opened from a click on our icon (exe relaunched,
/// shortcut...). In that case, briefly attach to the input queue of the foreground window.
fn show_window(hwnd: HWND) {
    // SAFETY: The popup HWND is live on this UI thread. Thread IDs come from Windows; any successful input-queue
    // attachment is detached before returning.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        if SetForegroundWindow(hwnd).as_bool() {
            return;
        }
        let fg_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let me = GetCurrentThreadId();
        let attached = fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(me, fg_thread, false);
        }
    }
}

impl Popup {
    fn apply_dwm(&self) {
        // SAFETY: The popup HWND is live on the UI thread. Each attribute pointer addresses an initialized value of
        // exactly the size and type required by that attribute; margins lasts through its call.
        unsafe {
            let dark = BOOL::from(self.theme.dark);
            let corner = DWMWCP_ROUND;
            let backdrop = DWMSBT_TRANSIENTWINDOW;
            let set = |attr: DWMWINDOWATTRIBUTE, ptr: *const std::ffi::c_void, size: usize| {
                let _ = DwmSetWindowAttribute(self.hwnd, attr, ptr, size as u32);
            };
            set(DWMWA_USE_IMMERSIVE_DARK_MODE, (&raw const dark).cast(), size_of::<BOOL>());
            set(DWMWA_WINDOW_CORNER_PREFERENCE, (&raw const corner).cast(), size_of::<DWM_WINDOW_CORNER_PREFERENCE>());
            set(DWMWA_SYSTEMBACKDROP_TYPE, (&raw const backdrop).cast(), size_of::<DWM_SYSTEMBACKDROP_TYPE>());
            let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
            let _ = DwmExtendFrameIntoClientArea(self.hwnd, &raw const margins);
        }
    }

    /// Recomputes size and position: the height depends on the data and the view.
    fn place(&mut self) {
        self.place_at(layout_height(self.view));
    }

    fn place_at(&mut self, height: f32) {
        self.height = height;
        let (w, h) = size_px(self.dpi, height);
        let (x, y, _) = placement(self.anchor, w, h);
        // SAFETY: The popup HWND is live on its owning UI thread; rc is writable local storage and SetWindowPos
        // receives no borrowed pointers.
        unsafe {
            let mut rc = RECT::default();
            let _ = GetWindowRect(self.hwnd, &raw mut rc);
            if (rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top) == (x, y, w, h) {
                return;
            }
            let _ = SetWindowPos(self.hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        self.sync_target_size();
    }

    /// The render buffer must match the window size exactly, otherwise it is stretched on screen (blurry and
    /// clipped content).
    fn sync_target_size(&self) {
        self.gfx.resize(window_size(self.hwnd));
    }

    fn set_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        self.slide = if view.depth() > self.view.depth() { 1.0 } else { -1.0 };
        self.view = view;
        self.view_changed = Instant::now();
        self.hits.hover = None;
        self.token_note = None;
        if view == View::Settings {
            self.autostart = sys::autostart_enabled();
            self.memory = settings_view::memory_text();
            self.confirm_clear = None;
        }
        self.place();
        self.animate();
    }

    fn start_timer(&self, id: usize, milliseconds: u32) {
        // SAFETY: The live popup HWND belongs to this UI thread; no callback pointer is registered.
        unsafe { SetTimer(Some(self.hwnd), id, milliseconds, None) };
    }

    fn start_confirmation_timer(&self) {
        self.start_timer(CONFIRM_TIMER, (CONFIRM_SECS * 1000.0) as u32);
    }

    fn animate(&mut self) {
        if !self.animating {
            self.animating = true;
            self.start_timer(ANIM_TIMER, 8);
        }
    }

    fn render(&mut self) {
        let app = app();
        let snap = app.snapshot.borrow();
        let expanded = app.expanded.borrow();
        let layout = Layout::new(self.view, snap.as_ref(), expanded.as_ref());
        // Safety net for state changes that did not go through `place`: the frame is drawn from this layout, so
        // the window must have its height.
        if layout.height().to_bits() != self.height.to_bits() {
            self.place_at(layout.height());
        }
        self.sync_target_size();

        let loading = app.loading.get();
        let t = self.opened.elapsed().as_secs_f32();
        let confirm_clear = self.confirm_clear.is_some_and(|at| at.elapsed().as_secs_f32() < CONFIRM_SECS);
        let note = self.token_note.as_ref().filter(|(_, at)| at.elapsed().as_secs_f32() < CONFIRM_SECS);
        let token_saved = match self.view {
            View::Provider(p) => secret::has_token(p),
            View::Main | View::Settings => false,
        };
        let ctx = SettingsCtx {
            autostart: self.autostart,
            confirm_clear,
            memory: &self.memory,
            token_saved,
            token_note: note.map(|(text, _)| text.as_str()),
        };
        let (gfx, th, hits) = (&mut self.gfx, &self.theme, &mut self.hits);
        hits.clear();

        gfx.alpha = ease_out(t / 0.18, 3);
        let dy = (1.0 - gfx.alpha) * 10.0;
        // On a view change, the content slides and fades in.
        let vt = ease_out(self.view_changed.elapsed().as_secs_f32() / 0.22, 3);
        gfx.alpha *= vt;
        let base = Matrix3x2::translation((1.0 - vt) * 16.0 * self.slide, dy);
        gfx.begin_draw(base);
        let mut done = vt >= 1.0;
        match &layout {
            Layout::Main(l) => {
                let reveal = ease_out(self.chart_opened.elapsed().as_secs_f32() / 0.5, 3);
                let anim = Anim { t, reveal, loading, base, dy };
                done &= main_view::draw(gfx, th, hits, l, snap.as_ref(), &anim);
            }
            Layout::Settings(l) => settings_view::draw(gfx, th, hits, &ctx, l),
            Layout::Provider(p, l) => settings_view::draw_provider(gfx, th, hits, &ctx, l, *p),
        }
        gfx.end_draw(self.hwnd, self.dpi);

        let still = t < 0.18 || !done || loading;
        if !still && self.animating {
            self.animating = false;
            stop_timer(self.hwnd, ANIM_TIMER);
        }
    }

    fn hit_at(&self, lparam: LPARAM) -> Option<Hit> {
        let scale = self.dpi / 96.0;
        let x = f32::from((lparam.0 & 0xFFFF) as i16) / scale;
        let y = f32::from(((lparam.0 >> 16) & 0xFFFF) as i16) / scale;
        self.hits.at(x, y)
    }

    fn set_hover(&mut self, hit: Option<Hit>) {
        if self.hits.hover != hit {
            self.hits.hover = hit;
            invalidate(self.hwnd);
        }
    }
}

enum Action {
    Refresh,
    Open(Provider),
    /// Expands / collapses a limit's chart.
    Toggle(LimitKey),
    View(View),
    /// Back to the parent view (provider page -> settings -> main).
    Back,
    Setting(Ctl),
    Close,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            with_popup(Popup::render);
            // SAFETY: Windows supplied this HWND to its UI-thread callback; None validates the entire client area.
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        // Client area = whole window, including during creation (wParam = 0); otherwise Windows subtracts
        // the resize frame.
        // DWM also paints the non-client edges, so suppress the classic frame.
        WM_NCCALCSIZE | WM_NCPAINT => LRESULT(0),
        WM_NCACTIVATE => {
            // SAFETY: Windows supplied this message and HWND; WM_NCACTIVATE permits -1 to suppress repainting.
            unsafe { DefWindowProcW(hwnd, msg, wparam, LPARAM(-1)) }
        }
        WM_NCHITTEST => LRESULT(HTCLIENT as isize),
        WM_TIMER => {
            if wparam.0 == CONFIRM_TIMER {
                stop_timer(hwnd, CONFIRM_TIMER);
            }
            if matches!(wparam.0, ANIM_TIMER | CLOCK_TIMER | CONFIRM_TIMER) {
                invalidate(hwnd);
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                // SAFETY: The callback's HWND is live on this UI thread; WM_CLOSE carries no pointer payload.
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            with_popup(|p| {
                if !p.tracking {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    // SAFETY: tme has the required size and a live HWND on its owning UI thread; Windows copies it
                    // during the call.
                    p.tracking = unsafe { TrackMouseEvent(&raw mut tme) }.is_ok();
                }
                let hit = p.hit_at(lparam);
                p.set_hover(hit);
            });
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with_popup(|p| {
                p.tracking = false;
                p.set_hover(None);
            });
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            with_popup(|p| p.pressed = p.hit_at(lparam));
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let action = with_popup(|p| {
                let hit = p.hit_at(lparam);
                if p.pressed.take() != hit {
                    return None;
                }
                match hit? {
                    Hit::Refresh => Some(Action::Refresh),
                    Hit::Open(provider) => Some(Action::Open(provider)),
                    Hit::Row(i) => p.hits.rows.get(i).cloned().map(Action::Toggle),
                    Hit::Gear => Some(Action::View(View::Settings)),
                    Hit::Back => Some(Action::Back),
                    Hit::Ctl(ctl) => Some(Action::Setting(ctl)),
                }
            })
            .flatten();
            run(action);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let in_settings = with_popup(|p| p.view != View::Main).unwrap_or(false);
            if wparam.0 == VK_ESCAPE.0 as usize && in_settings {
                run(Some(Action::Back));
            } else if wparam.0 == VK_ESCAPE.0 as usize {
                run(Some(Action::Close));
            } else if wparam.0 == 'R' as usize || wparam.0 == 0x74 {
                run(Some(Action::Refresh)); // R or F5
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            with_popup(|p| {
                p.dpi = (wparam.0 & 0xFFFF) as f32;
                p.gfx.set_dpi(p.dpi);
                p.place();
            });
            LRESULT(0)
        }
        WM_CLOSE => {
            destroy_window(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let popup = POPUP.with(|p| p.try_borrow_mut().ok().and_then(|mut p| p.take()));
            drop(popup);
            crate::popup_closed();
            LRESULT(0)
        }
        _ => {
            // SAFETY: Windows supplied the HWND and message payload to this callback; forwarding preserves them.
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
    }
}

fn open_external(target: impl AsRef<std::ffi::OsStr>) {
    let target = HSTRING::from(target.as_ref());
    // SAFETY: Both strings are terminated and remain alive for the synchronous call; no owner HWND is used.
    unsafe { ShellExecuteW(None, w!("open"), &target, None, None, SW_SHOWNORMAL) };
}

fn run(action: Option<Action>) {
    match action {
        Some(Action::Refresh) => app().refresh(true),
        Some(Action::Open(provider)) => {
            close();
            open_external(provider.usage_url());
        }
        Some(Action::Toggle(key)) => {
            {
                let mut expanded = app().expanded.borrow_mut();
                *expanded = if expanded.as_ref() == Some(&key) { None } else { Some(key) };
            }
            if let Some(hwnd) = with_popup(|p| {
                p.chart_opened = Instant::now();
                p.place();
                p.animate();
                p.hwnd
            }) {
                invalidate(hwnd);
            }
        }
        Some(Action::View(view)) => {
            if let Some(hwnd) = with_popup(|p| {
                p.set_view(view);
                p.hwnd
            }) {
                invalidate(hwnd);
            }
        }
        Some(Action::Back) => {
            if let Some(hwnd) = with_popup(|p| {
                let parent = if matches!(p.view, View::Provider(_)) { View::Settings } else { View::Main };
                p.set_view(parent);
                p.hwnd
            }) {
                invalidate(hwnd);
            }
        }
        Some(Action::Setting(ctl)) => settings_view::apply(ctl),
        Some(Action::Close) => close(),
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{Layout, Provider, View};

    /// Heights given by the hand-written formulas the layout pass replaced.
    #[test]
    fn heights_match_previous_formulas() {
        assert_eq!(Layout::new(View::Main, None, None).height(), 132.0);
        assert_eq!(Layout::new(View::Settings, None, None).height(), 650.0);
        assert_eq!(Layout::new(View::Provider(Provider::Amp), None, None).height(), 236.0);
    }
}
