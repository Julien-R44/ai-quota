//! Le flyout : une fenêtre popup Win32 sur fond Acrylic (DWM), dessinée en
//! Direct2D. Créée au clic, détruite à la fermeture : quand il est fermé, il
//! ne coûte rien.

use std::cell::RefCell;
use std::time::Instant;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_ESCAPE};
use windows::Win32::UI::Shell::{
    ABE_BOTTOM, ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETTASKBARPOS, APPBARDATA, SHAppBarMessage, ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, PCWSTR, w};
use windows_numerics::{Matrix3x2, Vector2};

use crate::history::{self, Sample};
use crate::i18n::{self, Lang, tr};
use crate::model::{BankedResets, Credit, Limit, LimitKind, Provider, Snapshot, Status, Usage, dollars};
use crate::settings::{POLL_CHOICES, ThemePref};
use crate::{app, logos, secret, svgpath, sys, timefmt};

// ------------------------------------------------------------------ layout (DIP)

const WIDTH: f32 = 360.0;
const MARGIN: f32 = 12.0;
const HEADER_H: f32 = 54.0;
const CARD_GAP: f32 = 8.0;
const CARD_PAD: f32 = 14.0;
const CARD_HEAD: f32 = 22.0;
const HEAD_GAP: f32 = 14.0;
const ROW_H: f32 = 52.0;
const ROW_GAP: f32 = 12.0;
const MESSAGE_H: f32 = 38.0;
/// Courbe affichée sous une ligne dépliée.
const CHART_TOP: f32 = 10.0;
const CHART_PLOT: f32 = 74.0;
const CHART_H: f32 = CHART_TOP + CHART_PLOT + 42.0;
/// Ligne « resets en réserve » en bas de carte.
const RESETS_H: f32 = 26.0;
const SCREEN_GAP: f32 = 12.0;

const CLASS: PCWSTR = w!("AiQuotaPopup");
const ANIM_TIMER: usize = 1;
const CLOCK_TIMER: usize = 2;
/// Fin de la demande de confirmation « Vider l'historique ».
const CONFIRM_TIMER: usize = 3;
const CONFIRM_SECS: f32 = 4.0;

/// Réglages : titre de section, hauteur de ligne, marges de carte.
const SECTION_TITLE: f32 = 24.0;
const SETTING_ROW: f32 = 40.0;
const SECTION_PAD: f32 = 4.0;
const SECTION_GAP: f32 = 10.0;
const FOOTER_H: f32 = 26.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Main,
    Settings,
    /// Réglages propres à un provider (le token d'Amp).
    Provider(Provider),
}

impl View {
    /// Profondeur de navigation, pour le sens du glissement.
    fn depth(self) -> u8 {
        match self {
            View::Main => 0,
            View::Settings => 1,
            View::Provider(_) => 2,
        }
    }
}

/// Providers qui ont leur propre page de réglages.
fn has_settings_page(p: Provider) -> bool {
    p == Provider::Amp
}

const AMP_TOKEN_URL: &str = "https://ampcode.com/settings";

/// Contrôles de la vue Réglages.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctl {
    Lang(Lang),
    Theme(ThemePref),
    Autostart,
    Provider(Provider),
    Poll(u32),
    FasterWhenHot,
    OpenFolder,
    ClearHistory,
    PasteAmpToken,
    ClearAmpToken,
    /// Roue crantée d'un provider : ouvre sa page de réglages.
    ProviderPage(Provider),
    /// Lien « Récupérer un token ».
    OpenTokenPage,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Hit {
    Refresh,
    /// Lien vers la page d'usage du provider.
    Open(Provider),
    /// Une ligne de limite (index dans `Popup::row_keys`) : déplie sa courbe.
    Row(usize),
    /// Engrenage de l'en-tête : vue Réglages.
    Gear,
    /// Retour des réglages vers la vue principale.
    Back,
    Ctl(Ctl),
}

enum Body<'a> {
    Limits(&'a [Limit]),
    Message(String, bool),
}

fn body(u: &Usage) -> Body<'_> {
    match &u.status {
        Status::Ok(l) if l.is_empty() => {
            Body::Message(tr("Aucune limite active pour ce compte.", "No active limit on this account.").into(), false)
        }
        Status::Ok(l) => Body::Limits(l),
        Status::Error(problem) => Body::Message(problem.text(), true),
    }
}

fn card_height(u: &Usage, expanded: Option<&str>) -> f32 {
    let inner = match body(u) {
        Body::Limits(l) => {
            let chart = if l.iter().any(|l| Some(l.key.as_str()) == expanded) { CHART_H } else { 0.0 };
            let extra_lines = usize::from(u.resets.is_some()) + usize::from(!u.credits.is_empty());
            l.len() as f32 * ROW_H + (l.len() as f32 - 1.0) * ROW_GAP + chart + extra_lines as f32 * RESETS_H
        }
        Body::Message(..) => MESSAGE_H,
    };
    2.0 * CARD_PAD + CARD_HEAD + HEAD_GAP + inner
}

fn content_height(snap: Option<&Snapshot>, expanded: Option<&str>) -> f32 {
    let cards: f32 = match snap {
        Some(s) if !s.usages.is_empty() => {
            s.usages.iter().map(|u| card_height(u, expanded)).sum::<f32>() + (s.usages.len() as f32 - 1.0) * CARD_GAP
        }
        _ => 2.0 * CARD_PAD + MESSAGE_H,
    };
    HEADER_H + cards + MARGIN
}

/// Lignes de chaque section des réglages.
const SETTINGS_SECTIONS: [usize; 4] = [3, 3, 2, 1];
/// Page d'un provider : section token (2 lignes) + aide.
const PROVIDER_HELP_H: f32 = 44.0;

fn provider_settings_height() -> f32 {
    HEADER_H + SECTION_TITLE + 2.0 * SECTION_PAD + 2.0 * SETTING_ROW + PROVIDER_HELP_H + MARGIN
}

fn settings_height() -> f32 {
    let sections: f32 =
        SETTINGS_SECTIONS.iter().map(|&rows| SECTION_TITLE + 2.0 * SECTION_PAD + rows as f32 * SETTING_ROW).sum();
    HEADER_H + sections + (SETTINGS_SECTIONS.len() - 1) as f32 * SECTION_GAP + FOOTER_H + MARGIN
}

// ------------------------------------------------------------------ couleurs

fn rgb(hex: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a,
    }
}

struct Theme {
    dark: bool,
    text1: D2D1_COLOR_F,
    text2: D2D1_COLOR_F,
    text3: D2D1_COLOR_F,
    card: D2D1_COLOR_F,
    card_border: D2D1_COLOR_F,
    track: D2D1_COLOR_F,
    button_hover: D2D1_COLOR_F,
    pace: D2D1_COLOR_F,
    warn: D2D1_COLOR_F,
    crit: D2D1_COLOR_F,
}

impl Theme {
    fn current() -> Self {
        let dark = match app().settings.borrow().theme {
            ThemePref::Auto => sys::apps_dark(),
            ThemePref::Light => false,
            ThemePref::Dark => true,
        };
        if dark {
            Theme {
                dark: true,
                text1: rgb(0xFFFFFF, 0.94),
                text2: rgb(0xFFFFFF, 0.62),
                text3: rgb(0xFFFFFF, 0.42),
                card: rgb(0xFFFFFF, 0.045),
                card_border: rgb(0xFFFFFF, 0.07),
                track: rgb(0xFFFFFF, 0.10),
                button_hover: rgb(0xFFFFFF, 0.07),
                pace: rgb(0xFFFFFF, 0.55),
                warn: rgb(0xF2B035, 1.0),
                crit: rgb(0xFF6B6B, 1.0),
            }
        } else {
            Theme {
                dark: false,
                text1: rgb(0x000000, 0.90),
                text2: rgb(0x000000, 0.60),
                text3: rgb(0x000000, 0.44),
                card: rgb(0xFFFFFF, 0.55),
                card_border: rgb(0x000000, 0.06),
                track: rgb(0x000000, 0.08),
                button_hover: rgb(0x000000, 0.05),
                pace: rgb(0x000000, 0.45),
                warn: rgb(0xB87800, 1.0),
                crit: rgb(0xD13438, 1.0),
            }
        }
    }

    /// (couleur principale, texte de la pastille du plan)
    fn brand(&self, p: Provider) -> (D2D1_COLOR_F, D2D1_COLOR_F) {
        match (p, self.dark) {
            (Provider::Claude, true) => (rgb(0xD97757, 1.0), rgb(0xF2A386, 1.0)),
            (Provider::Claude, false) => (rgb(0xD97757, 1.0), rgb(0xB4532F, 1.0)),
            (Provider::Codex, true) => (rgb(0x8C9EFF, 1.0), rgb(0xB3BFFF, 1.0)),
            (Provider::Codex, false) => (rgb(0x4A5CE0, 1.0), rgb(0x3B4CCF, 1.0)),
            // Identité Amp : vert très sombre et crème (#DFDFC1).
            (Provider::Amp, true) => (rgb(0xDFDFC1, 1.0), rgb(0xDFDFC1, 1.0)),
            (Provider::Amp, false) => (rgb(0x2F4A2C, 1.0), rgb(0x2C3A2A, 1.0)),
        }
    }

    /// Couleur d'accent de l'app (interrupteurs, sélection) : le violet de
    /// l'icône. Celle de Windows peut être grise et rendre un interrupteur
    /// activé indiscernable d'un désactivé.
    fn accent(&self) -> D2D1_COLOR_F {
        if self.dark { rgb(0xA48BFF, 1.0) } else { rgb(0x6B4EFF, 1.0) }
    }

    /// Texte posé sur la couleur d'accent.
    fn on_accent(&self) -> D2D1_COLOR_F {
        if self.dark { rgb(0x000000, 0.9) } else { rgb(0xFFFFFF, 1.0) }
    }

    fn level(&self, percent: f32, normal: D2D1_COLOR_F) -> D2D1_COLOR_F {
        match percent {
            p if p >= 95.0 => self.crit,
            p if p >= 80.0 => self.warn,
            _ => normal,
        }
    }
}

// ------------------------------------------------------------------ dessin

fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

fn contains(r: &D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

struct Fonts {
    title: IDWriteTextFormat,
    name: IDWriteTextFormat,
    label: IDWriteTextFormat,
    value: IDWriteTextFormat,
    caption: IDWriteTextFormat,
    pill: IDWriteTextFormat,
    message: IDWriteTextFormat,
    icon: IDWriteTextFormat,
    icon_small: IDWriteTextFormat,
    tiny: IDWriteTextFormat,
    caption_wrap: IDWriteTextFormat,
}

struct Gfx {
    factory: ID2D1Factory,
    dwrite: IDWriteFactory,
    target: ID2D1HwndRenderTarget,
    brush: ID2D1SolidColorBrush,
    round: ID2D1StrokeStyle,
    dash: ID2D1StrokeStyle,
    /// Logos vectoriels, dans l'ordre de `Provider`.
    logos: [ID2D1PathGeometry; 3],
    fonts: Fonts,
    /// Opacité globale (animation d'ouverture).
    alpha: f32,
}

fn has_font(dwrite: &IDWriteFactory, family: PCWSTR) -> bool {
    unsafe {
        let mut collection = None;
        if dwrite.GetSystemFontCollection(&mut collection, false).is_err() {
            return false;
        }
        let Some(collection) = collection else { return false };
        let (mut index, mut exists) = (0u32, BOOL::default());
        collection.FindFamilyName(family, &mut index, &mut exists).is_ok() && exists.as_bool()
    }
}

impl Gfx {
    fn new(hwnd: HWND, dpi: f32) -> windows::core::Result<Self> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let target = Self::create_target(&factory, hwnd, dpi)?;
            let brush = target.CreateSolidColorBrush(&rgb(0, 1.0), None)?;
            let round = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    startCap: D2D1_CAP_STYLE_ROUND,
                    endCap: D2D1_CAP_STYLE_ROUND,
                    lineJoin: D2D1_LINE_JOIN_ROUND,
                    ..Default::default()
                },
                None,
            )?;
            let dash = factory.CreateStrokeStyle(
                &D2D1_STROKE_STYLE_PROPERTIES {
                    dashCap: D2D1_CAP_STYLE_ROUND,
                    dashStyle: D2D1_DASH_STYLE_CUSTOM,
                    ..Default::default()
                },
                Some(&[0.1, 3.0]),
            )?;

            let text_family = if has_font(&dwrite, w!("Segoe UI Variable Text")) {
                w!("Segoe UI Variable Text")
            } else {
                w!("Segoe UI")
            };
            let icon_family = if has_font(&dwrite, w!("Segoe Fluent Icons")) {
                w!("Segoe Fluent Icons")
            } else {
                w!("Segoe MDL2 Assets")
            };
            let make = |family: PCWSTR,
                        weight: DWRITE_FONT_WEIGHT,
                        size: f32,
                        wrap: bool|
             -> windows::core::Result<IDWriteTextFormat> {
                let f = dwrite.CreateTextFormat(
                    family,
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("fr-fr"),
                )?;
                f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
                if !wrap {
                    f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
                    let trimming =
                        DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, ..Default::default() };
                    let sign = dwrite.CreateEllipsisTrimmingSign(&f)?;
                    f.SetTrimming(&trimming, &sign)?;
                }
                Ok(f)
            };
            let fonts = Fonts {
                title: make(text_family, DWRITE_FONT_WEIGHT_SEMI_BOLD, 15.0, false)?,
                name: make(text_family, DWRITE_FONT_WEIGHT_SEMI_BOLD, 14.0, false)?,
                label: make(text_family, DWRITE_FONT_WEIGHT_NORMAL, 13.0, false)?,
                value: make(text_family, DWRITE_FONT_WEIGHT_SEMI_BOLD, 13.0, false)?,
                caption: make(text_family, DWRITE_FONT_WEIGHT_NORMAL, 12.0, false)?,
                pill: make(text_family, DWRITE_FONT_WEIGHT_SEMI_BOLD, 11.0, false)?,
                message: make(text_family, DWRITE_FONT_WEIGHT_NORMAL, 13.0, true)?,
                icon: make(icon_family, DWRITE_FONT_WEIGHT_NORMAL, 14.0, false)?,
                icon_small: make(icon_family, DWRITE_FONT_WEIGHT_NORMAL, 11.0, false)?,
                tiny: make(text_family, DWRITE_FONT_WEIGHT_NORMAL, 10.0, false)?,
                caption_wrap: make(text_family, DWRITE_FONT_WEIGHT_NORMAL, 12.0, true)?,
            };
            let logos = [
                svgpath::geometry(&factory, logos::CLAUDE, false)?,
                svgpath::geometry(&factory, logos::CODEX, true)?,
                svgpath::geometry(&factory, logos::AMP, false)?,
            ];
            Ok(Gfx { factory, dwrite, target, brush, round, dash, logos, fonts, alpha: 1.0 })
        }
    }

    fn create_target(factory: &ID2D1Factory, hwnd: HWND, dpi: f32) -> windows::core::Result<ID2D1HwndRenderTarget> {
        unsafe {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                // Rendu logiciel : pas de driver GPU chargé dans le process
                // (mesuré : ~8 Mo alloués popup ouvert, contre ~60 Mo en
                // GPU). Largement assez rapide pour 360×400 DIP.
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: dpi,
                dpiY: dpi,
                ..Default::default()
            };
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: window_size(hwnd),
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let target = factory.CreateHwndRenderTarget(&props, &hwnd_props)?;
            target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            Ok(target)
        }
    }

    fn color(&self, c: D2D1_COLOR_F) -> &ID2D1SolidColorBrush {
        unsafe { self.brush.SetColor(&D2D1_COLOR_F { a: c.a * self.alpha, ..c }) };
        &self.brush
    }

    fn fill(&self, r: D2D_RECT_F, radius: f32, c: D2D1_COLOR_F) {
        let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
        unsafe { self.target.FillRoundedRectangle(&rr, self.color(c)) };
    }

    fn stroke(&self, r: D2D_RECT_F, radius: f32, c: D2D1_COLOR_F) {
        let r = D2D_RECT_F { left: r.left + 0.5, top: r.top + 0.5, right: r.right - 0.5, bottom: r.bottom - 0.5 };
        let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
        unsafe { self.target.DrawRoundedRectangle(&rr, self.color(c), 1.0, None) };
    }

    fn line(&self, a: (f32, f32), b: (f32, f32), width: f32, c: D2D1_COLOR_F) {
        unsafe {
            self.target.DrawLine(
                Vector2 { X: a.0, Y: a.1 },
                Vector2 { X: b.0, Y: b.1 },
                self.color(c),
                width,
                &self.round,
            )
        };
    }

    fn dashed(&self, a: (f32, f32), b: (f32, f32), width: f32, c: D2D1_COLOR_F) {
        unsafe {
            self.target.DrawLine(
                Vector2 { X: a.0, Y: a.1 },
                Vector2 { X: b.0, Y: b.1 },
                self.color(c),
                width,
                &self.dash,
            )
        };
    }

    /// Logo du provider (viewBox 24×24) dans un carré de `size` DIP.
    fn logo(&self, p: Provider, x: f32, y: f32, size: f32, c: D2D1_COLOR_F) {
        self.path(&self.logos[p as usize], x, y, size / 24.0, c);
    }

    /// Remplit une géométrie à l'échelle `s`, placée en (`x`, `y`).
    fn path(&self, geometry: &ID2D1PathGeometry, x: f32, y: f32, s: f32, c: D2D1_COLOR_F) {
        unsafe {
            let mut current = Matrix3x2::identity();
            self.target.GetTransform(&mut current);
            self.target.SetTransform(&(Matrix3x2::scale(s, s) * Matrix3x2::translation(x, y) * current));
            self.target.FillGeometry(geometry, self.color(c), None);
            self.target.SetTransform(&current);
        }
    }

    fn fill_vertical_gradient(&self, r: D2D_RECT_F, radius: f32, top: D2D1_COLOR_F, bottom: D2D1_COLOR_F) {
        let stops =
            [D2D1_GRADIENT_STOP { position: 0.0, color: top }, D2D1_GRADIENT_STOP { position: 1.0, color: bottom }];
        unsafe {
            let Ok(collection) =
                self.target.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
            else {
                return;
            };
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: Vector2 { X: 0.0, Y: r.top },
                endPoint: Vector2 { X: 0.0, Y: r.bottom },
            };
            let Ok(brush) = self.target.CreateLinearGradientBrush(&props, None, &collection) else { return };
            brush.SetOpacity(self.alpha);
            let rr = D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius };
            self.target.FillRoundedRectangle(&rr, &brush);
        }
    }

    fn disc(&self, (x, y): (f32, f32), r: f32, c: D2D1_COLOR_F) {
        let e = D2D1_ELLIPSE { point: Vector2 { X: x, Y: y }, radiusX: r, radiusY: r };
        unsafe { self.target.FillEllipse(&e, self.color(c)) };
    }

    /// Aire sous une courbe, en dégradé vertical vers la transparence.
    fn area(&self, points: &[(f32, f32)], top: f32, bottom: f32, c: D2D1_COLOR_F) {
        let (Some(first), Some(last)) = (points.first(), points.last()) else { return };
        let v = |(x, y): (f32, f32)| Vector2 { X: x, Y: y };
        unsafe {
            let Ok(path) = self.factory.CreatePathGeometry() else { return };
            let Ok(sink) = path.Open() else { return };
            sink.BeginFigure(v((first.0, bottom)), D2D1_FIGURE_BEGIN_FILLED);
            for &p in points {
                sink.AddLine(v(p));
            }
            sink.AddLine(v((last.0, bottom)));
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            if sink.Close().is_err() {
                return;
            }
            let stops = [
                D2D1_GRADIENT_STOP { position: 0.0, color: D2D1_COLOR_F { a: 0.34, ..c } },
                D2D1_GRADIENT_STOP { position: 1.0, color: D2D1_COLOR_F { a: 0.02, ..c } },
            ];
            let Ok(collection) =
                self.target.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
            else {
                return;
            };
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: v((0.0, top)), endPoint: v((0.0, bottom)) };
            let Ok(brush) = self.target.CreateLinearGradientBrush(&props, None, &collection) else { return };
            brush.SetOpacity(self.alpha);
            self.target.FillGeometry(&path, &brush, None);
        }
    }

    fn text(&self, s: &str, format: &IDWriteTextFormat, r: D2D_RECT_F, c: D2D1_COLOR_F, align: DWRITE_TEXT_ALIGNMENT) {
        unsafe {
            let _ = format.SetTextAlignment(align);
            self.target.DrawText(
                &wide(s),
                format,
                &r,
                self.color(c),
                D2D1_DRAW_TEXT_OPTIONS_NONE,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    fn text_width(&self, s: &str, format: &IDWriteTextFormat) -> f32 {
        unsafe {
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
            let Ok(layout) = self.dwrite.CreateTextLayout(&wide(s), format, 1000.0, 100.0) else { return 0.0 };
            let mut m = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&mut m);
            m.widthIncludingTrailingWhitespace
        }
    }

    /// La jauge de l'icône, en dégradé (même dessin que `art.rs`, en vectoriel).
    fn gauge(&self, x: f32, y: f32, size: f32) {
        let s = size / 16.0;
        let p = |px: f32, py: f32| Vector2 { X: x + px * s, Y: y + py * s };
        let polar = |deg: f32, r: f32| {
            let a = deg.to_radians();
            p(8.0 + r * a.cos(), 9.4 - r * a.sin())
        };
        unsafe {
            let Ok(path) = self.factory.CreatePathGeometry() else { return };
            let Ok(sink) = path.Open() else { return };
            sink.BeginFigure(polar(210.0, 6.0), D2D1_FIGURE_BEGIN_HOLLOW);
            sink.AddArc(&D2D1_ARC_SEGMENT {
                point: polar(-30.0, 6.0),
                size: D2D_SIZE_F { width: 6.0 * s, height: 6.0 * s },
                rotationAngle: 0.0,
                sweepDirection: D2D1_SWEEP_DIRECTION_CLOCKWISE,
                arcSize: D2D1_ARC_SIZE_LARGE,
            });
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
            if sink.Close().is_err() {
                return;
            }
            let stops = [
                D2D1_GRADIENT_STOP { position: 0.0, color: rgb(0xFF8A5B, 1.0) },
                D2D1_GRADIENT_STOP { position: 1.0, color: rgb(0x8B6BFF, 1.0) },
            ];
            let Ok(collection) =
                self.target.CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
            else {
                return;
            };
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES { startPoint: p(1.0, 2.0), endPoint: p(15.0, 14.0) };
            let Ok(brush) = self.target.CreateLinearGradientBrush(&props, None, &collection) else { return };
            brush.SetOpacity(self.alpha);
            self.target.DrawGeometry(&path, &brush, 1.9 * s, &self.round);
            self.target.DrawLine(p(8.0, 9.4), polar(42.0, 3.9), &brush, crate::art::NEEDLE_W * s, &self.round);
            let r = crate::art::HUB_R * s;
            let hub = D2D1_ELLIPSE { point: p(8.0, 9.4), radiusX: r, radiusY: r };
            self.target.FillEllipse(&hub, &brush);
        }
    }

    fn transform(&self, m: Matrix3x2) {
        unsafe { self.target.SetTransform(&m) };
    }
}

// ------------------------------------------------------------------ popup

struct Popup {
    hwnd: HWND,
    gfx: Gfx,
    dpi: f32,
    theme: Theme,
    anchor: Option<RECT>,
    opened: Instant,
    hover: Option<Hit>,
    pressed: Option<Hit>,
    hits: Vec<(D2D_RECT_F, Hit)>,
    /// Clés des lignes affichées, dans l'ordre (cible `Hit::Row`).
    row_keys: Vec<String>,
    /// Dépliage de la dernière courbe (animation de révélation).
    chart_opened: Instant,
    view: View,
    /// Changement de vue (animation de glissement).
    view_changed: Instant,
    /// Sens du glissement : +1 en entrant plus profond, -1 en revenant.
    slide: f32,
    /// État du démarrage auto, lu dans le registre à l'entrée des réglages.
    autostart: bool,
    /// « Vider l'historique » attend une confirmation depuis cet instant.
    confirm_clear: Option<Instant>,
    /// Mémoire du process, relevée à l'entrée des réglages.
    memory: String,
    /// Un token Amp est enregistré.
    amp_token: bool,
    amp_note: Option<(String, Instant)>,
    tracking: bool,
    animating: bool,
}

thread_local! {
    static POPUP: RefCell<Option<Popup>> = const { RefCell::new(None) };
}

fn with_popup<R>(f: impl FnOnce(&mut Popup) -> R) -> Option<R> {
    POPUP.with(|p| p.try_borrow_mut().ok()?.as_mut().map(f))
}

pub fn is_open() -> bool {
    POPUP.with(|p| p.try_borrow().map(|p| p.is_some()).unwrap_or(true))
}

pub fn close() {
    if let Some(hwnd) = with_popup(|p| p.hwnd) {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// Les données ou l'état de chargement ont changé.
pub fn on_data() {
    if let Some(hwnd) = with_popup(|p| {
        p.place();
        p.animate();
        p.hwnd
    }) {
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

/// Ouvre la vue Réglages (depuis le menu du tray).
pub fn show_settings() {
    if let Some(hwnd) = with_popup(|p| {
        p.set_view(View::Settings);
        p.hwnd
    }) {
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

/// Un réglage a changé : langue, providers, thème…
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
        unsafe {
            let _ = InvalidateRect(Some(p.hwnd), None, false);
        }
    });
}

/// Moniteur et DPI à utiliser pour un popup ancré sur l'icône du tray.
fn monitor_for(anchor: Option<RECT>) -> (MONITORINFO, f32) {
    unsafe {
        let monitor = match anchor {
            Some(r) => MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST),
            None => {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
            }
        };
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(monitor, &mut info);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        (info, dx as f32)
    }
}

/// Rectangle et bord de la barre des tâches (y compris masquée automatiquement,
/// auquel cas la zone de travail ne l'exclut pas).
fn taskbar() -> Option<(RECT, u32)> {
    let mut abd = APPBARDATA { cbSize: size_of::<APPBARDATA>() as u32, ..Default::default() };
    let ok = unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut abd) } != 0;
    ok.then_some((abd.rc, abd.uEdge))
}

/// Position façon flyout Windows 11 : collé à la barre des tâches, centré
/// sur l'icône quand c'est possible, jamais hors de l'écran.
fn placement(anchor: Option<RECT>, w: i32, h: i32) -> (i32, i32, f32) {
    let (info, dpi) = monitor_for(anchor);
    let (mon, mut area) = (info.rcMonitor, info.rcWork);
    let gap = (SCREEN_GAP * dpi / 96.0).round() as i32;

    let mut edge = ABE_BOTTOM;
    if let Some((tb, tb_edge)) = taskbar() {
        let (cx, cy) = ((tb.left + tb.right) / 2, (tb.top + tb.bottom) / 2);
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
    let (icx, icy) = ((icon.left + icon.right) / 2, (icon.top + icon.bottom) / 2);
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
    unsafe {
        let _ = GetWindowRect(hwnd, &mut rc);
    }
    D2D_SIZE_U { width: (rc.right - rc.left).max(1) as u32, height: (rc.bottom - rc.top).max(1) as u32 }
}

fn size_px(dpi: f32, view: View) -> (i32, i32) {
    let snap = app().snapshot.borrow();
    let scale = dpi / 96.0;
    let expanded = app().expanded.borrow();
    let height = match view {
        View::Main => content_height(snap.as_ref(), expanded.as_deref()),
        View::Settings => settings_height(),
        View::Provider(_) => provider_settings_height(),
    };
    ((WIDTH * scale).round() as i32, (height * scale).round() as i32)
}

pub fn open(anchor: Option<RECT>) {
    if is_open() {
        return;
    }
    unsafe {
        let instance = GetModuleHandleW(None).unwrap_or_default();
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&class);

        let (_, dpi) = monitor_for(anchor);
        let (w, h) = size_px(dpi, View::Main);
        let (x, y, _) = placement(anchor, w, h);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            CLASS,
            w!("AI Quota"),
            // Un cadre redimensionnable dont on masque la zone non cliente
            // (WM_NCCALCSIZE) : c'est ce qui permet à DWM d'appliquer le fond
            // Acrylic et l'ombre système, ce qu'il refuse à un WS_POPUP nu.
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
        let dpi = GetDpiForWindow(hwnd) as f32;
        let Ok(gfx) = Gfx::new(hwnd, dpi) else {
            let _ = DestroyWindow(hwnd);
            return;
        };
        let popup = Popup {
            hwnd,
            gfx,
            dpi,
            theme: Theme::current(),
            anchor,
            opened: Instant::now(),
            hover: None,
            pressed: None,
            hits: Vec::new(),
            tracking: false,
            row_keys: Vec::new(),
            chart_opened: Instant::now(),
            view: View::Main,
            slide: 1.0,
            view_changed: Instant::now() - std::time::Duration::from_secs(1),
            autostart: false,
            confirm_clear: None,
            memory: String::new(),
            amp_token: false,
            amp_note: None,
            animating: false,
        };
        popup.apply_dwm();
        POPUP.with(|p| *p.borrow_mut() = Some(popup));
        with_popup(|p| p.animate());
        SetTimer(Some(hwnd), CLOCK_TIMER, 30_000, None);

        let _ = ShowWindow(hwnd, SW_SHOW);
        bring_to_front(hwnd);
    }
}

/// Windows refuse `SetForegroundWindow` quand l'ouverture ne vient pas d'un
/// clic sur notre icône (relance de l'exe, raccourci…). On se rattache alors
/// brièvement à la file d'entrée de la fenêtre au premier plan.
fn bring_to_front(hwnd: HWND) {
    unsafe {
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

fn ease_out(t: f32, power: i32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(power)
}

impl Popup {
    fn apply_dwm(&self) {
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
            let _ = DwmExtendFrameIntoClientArea(self.hwnd, &margins);
        }
    }

    /// Recalcule taille et position (la hauteur dépend des données).
    fn place(&mut self) {
        let (w, h) = size_px(self.dpi, self.view);
        let (x, y, _) = placement(self.anchor, w, h);
        unsafe {
            let mut rc = RECT::default();
            let _ = GetWindowRect(self.hwnd, &mut rc);
            if (rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top) == (x, y, w, h) {
                return;
            }
            let _ = SetWindowPos(self.hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        }
        self.sync_target_size();
    }

    /// Le buffer de rendu doit avoir exactement la taille de la fenêtre :
    /// sinon il est étiré à l'affichage (contenu flou et rogné).
    fn sync_target_size(&self) {
        let size = window_size(self.hwnd);
        let current = unsafe { self.gfx.target.GetPixelSize() };
        if (current.width, current.height) != (size.width, size.height) {
            let _ = unsafe { self.gfx.target.Resize(&size) };
        }
    }

    fn set_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        self.slide = if view.depth() > self.view.depth() { 1.0 } else { -1.0 };
        self.view = view;
        self.view_changed = Instant::now();
        self.hover = None;
        self.amp_token = secret::has_amp_token();
        self.amp_note = None;
        if view == View::Settings {
            self.autostart = sys::autostart_enabled();
            self.memory = memory_text();
            self.confirm_clear = None;
            self.amp_token = secret::has_amp_token();
            self.amp_note = None;
        }
        self.place();
        self.animate();
    }

    fn animate(&mut self) {
        if !self.animating {
            self.animating = true;
            unsafe { SetTimer(Some(self.hwnd), ANIM_TIMER, 8, None) };
        }
    }

    fn render(&mut self) {
        self.sync_target_size();
        let app = app();
        let snap = app.snapshot.borrow();
        let loading = app.loading.get();
        let t = self.opened.elapsed().as_secs_f32();
        let gfx = &mut self.gfx;
        let th = &self.theme;
        self.hits.clear();
        self.row_keys.clear();
        let expanded = app.expanded.borrow();
        let history = app.history.borrow();
        let reveal = ease_out(self.chart_opened.elapsed().as_secs_f32() / 0.5, 3);

        gfx.alpha = ease_out(t / 0.18, 3);
        let dy = (1.0 - gfx.alpha) * 10.0;
        // Changement de vue : le contenu glisse et apparaît en fondu.
        let vt = ease_out(self.view_changed.elapsed().as_secs_f32() / 0.22, 3);
        gfx.alpha *= vt;
        let dx = (1.0 - vt) * 16.0 * self.slide;
        let base = Matrix3x2::translation(dx, dy);
        let mut bars_done = vt >= 1.0;

        unsafe {
            gfx.target.BeginDraw();
            gfx.target.Clear(Some(&rgb(0, 0.0)));
        }
        gfx.transform(base);

        if self.view != View::Main {
            let confirm = self.confirm_clear.is_some_and(|at| at.elapsed().as_secs_f32() < CONFIRM_SECS);
            let note = self.amp_note.as_ref().filter(|(_, at)| at.elapsed().as_secs_f32() < CONFIRM_SECS);
            let ctx = SettingsCtx {
                autostart: self.autostart,
                confirm_clear: confirm,
                memory: &self.memory,
                amp_token: self.amp_token,
                amp_note: note.map(|(text, _)| text.as_str()),
            };
            match self.view {
                View::Provider(p) => draw_provider_settings(gfx, th, &mut self.hits, self.hover, &ctx, p),
                _ => draw_settings(gfx, th, &mut self.hits, self.hover, &ctx),
            }
        } else {
            // En-tête
            gfx.gauge(18.0, 19.0, 18.0);
            gfx.text(
                "AI Quota",
                &gfx.fonts.title,
                rect(44.0, 12.0, 120.0, 32.0),
                th.text1,
                DWRITE_TEXT_ALIGNMENT_LEADING,
            );
            let btn = rect(WIDTH - MARGIN - 32.0, 12.0, 32.0, 32.0);
            self.hits.push((btn, Hit::Refresh));
            if self.hover == Some(Hit::Refresh) {
                gfx.fill(btn, 6.0, th.button_hover);
            }
            let (cx, cy) = ((btn.left + btn.right) / 2.0, (btn.top + btn.bottom) / 2.0 + dy);
            if loading {
                gfx.transform(base * Matrix3x2::rotation_around(t * 400.0, Vector2 { X: cx, Y: cy }));
            }
            gfx.text("\u{E72C}", &gfx.fonts.icon, btn, th.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
            gfx.transform(base);
            let gear = rect(btn.left - 34.0, 12.0, 32.0, 32.0);
            self.hits.push((gear, Hit::Gear));
            if self.hover == Some(Hit::Gear) {
                gfx.fill(gear, 6.0, th.button_hover);
            }
            gfx.text("\u{E713}", &gfx.fonts.icon, gear, th.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
            let status = match (&*snap, loading) {
                (_, true) => tr("Actualisation…", "Refreshing…").to_string(),
                (Some(s), false) => timefmt::ago_text(s.fetched_at, timefmt::now()),
                (None, false) => String::new(),
            };
            gfx.text(
                &status,
                &gfx.fonts.caption,
                rect(130.0, 12.0, gear.left - 134.0, 32.0),
                th.text3,
                DWRITE_TEXT_ALIGNMENT_TRAILING,
            );

            // Cartes
            let mut y = HEADER_H;
            let x = MARGIN;
            let cw = WIDTH - 2.0 * MARGIN;
            match &*snap {
                Some(s) if !s.usages.is_empty() => {
                    let mut row_index = 0;
                    for u in &s.usages {
                        let h = card_height(u, expanded.as_deref());
                        let card = rect(x, y, cw, h);
                        gfx.fill(card, 8.0, th.card);
                        gfx.stroke(card, 8.0, th.card_border);
                        let open = rect(x + cw - CARD_PAD - 22.0, y + CARD_PAD - 3.0, 28.0, CARD_HEAD + 6.0);
                        self.hits.push((open, Hit::Open(u.provider)));
                        let open_hovered = self.hover == Some(Hit::Open(u.provider));
                        draw_card_header(gfx, th, u, x + CARD_PAD, y + CARD_PAD, cw - 2.0 * CARD_PAD, open_hovered);

                        let inner_x = x + CARD_PAD;
                        let inner_w = cw - 2.0 * CARD_PAD;
                        let mut ry = y + CARD_PAD + CARD_HEAD + HEAD_GAP;
                        match body(u) {
                            Body::Limits(limits) => {
                                let (brand, _) = th.brand(u.provider);
                                for l in limits {
                                    let start = 0.08 + row_index as f32 * 0.05;
                                    let p = ease_out((t - start) / 0.7, 4);
                                    bars_done &= p >= 1.0;
                                    let is_open = expanded.as_deref() == Some(l.key.as_str());
                                    let row_h = ROW_H + if is_open { CHART_H } else { 0.0 };
                                    let row = rect(inner_x - 6.0, ry - 5.0, inner_w + 12.0, row_h + 8.0);
                                    let hit = Hit::Row(self.row_keys.len());
                                    self.row_keys.push(l.key.clone());
                                    self.hits.push((row, hit));
                                    if self.hover == Some(hit) {
                                        gfx.fill(row, 6.0, th.button_hover);
                                    }
                                    draw_row(gfx, th, l, brand, p, (inner_x, ry), inner_w, is_open);
                                    if is_open {
                                        let window = l.resets_at.zip(l.window_secs).map_or(0, |(r, w)| r - w);
                                        let samples = history.window(&l.key, window);
                                        draw_chart(gfx, th, l, samples, brand, reveal, (inner_x, ry + ROW_H), inner_w);
                                        bars_done &= reveal >= 1.0;
                                    }
                                    ry += row_h + ROW_GAP;
                                    row_index += 1;
                                }
                                let mut ly = ry - ROW_GAP + 8.0;
                                if let Some(resets) = &u.resets {
                                    draw_resets(gfx, th, resets, brand, (inner_x, ly), inner_w);
                                    ly += RESETS_H;
                                }
                                if !u.credits.is_empty() {
                                    draw_credits(gfx, th, &u.credits, brand, (inner_x, ly), inner_w);
                                }
                            }
                            Body::Message(msg, is_error) => draw_message(gfx, th, &msg, is_error, inner_x, ry, inner_w),
                        }
                        y += h + CARD_GAP;
                    }
                }
                Some(_) => {
                    let card = rect(x, y, cw, 2.0 * CARD_PAD + MESSAGE_H);
                    gfx.fill(card, 8.0, th.card);
                    gfx.stroke(card, 8.0, th.card_border);
                    let msg = tr(
                        "Aucun outil suivi. Connecte-toi à Claude Code ou à Codex, ou active-les dans les réglages.",
                        "Nothing tracked. Sign in to Claude Code or Codex, or enable them in Settings.",
                    );
                    draw_message(gfx, th, msg, false, x + CARD_PAD, y + CARD_PAD, cw - 2.0 * CARD_PAD);
                }
                None => {
                    let r = rect(x, y, cw, 2.0 * CARD_PAD + MESSAGE_H);
                    gfx.text(
                        tr("Chargement…", "Loading…"),
                        &gfx.fonts.label,
                        r,
                        th.text2,
                        DWRITE_TEXT_ALIGNMENT_CENTER,
                    );
                }
            }
        }

        gfx.transform(Matrix3x2::identity());
        let result = unsafe { gfx.target.EndDraw(None, None) };
        // Device perdu (veille, changement d'écran) : on recrée la cible.
        if result.as_ref().is_err_and(|e| e.code() == D2DERR_RECREATE_TARGET)
            && let Ok(target) = Gfx::create_target(&gfx.factory, self.hwnd, self.dpi)
            && let Ok(brush) = unsafe { target.CreateSolidColorBrush(&rgb(0, 1.0), None) }
        {
            gfx.target = target;
            gfx.brush = brush;
        }

        let still = t < 0.18 || !bars_done || loading;
        if !still && self.animating {
            self.animating = false;
            unsafe {
                let _ = KillTimer(Some(self.hwnd), ANIM_TIMER);
            }
        }
    }

    fn hit_at(&self, lparam: LPARAM) -> Option<Hit> {
        let scale = self.dpi / 96.0;
        let x = (lparam.0 & 0xFFFF) as i16 as f32 / scale;
        let y = ((lparam.0 >> 16) & 0xFFFF) as i16 as f32 / scale;
        self.hits.iter().rev().find(|(r, _)| contains(r, x, y)).map(|(_, h)| *h)
    }

    fn set_hover(&mut self, hit: Option<Hit>) {
        if self.hover != hit {
            self.hover = hit;
            unsafe {
                let _ = InvalidateRect(Some(self.hwnd), None, false);
            }
        }
    }
}

fn draw_card_header(gfx: &Gfx, th: &Theme, u: &Usage, x: f32, y: f32, w: f32, open_hovered: bool) {
    let (brand, pill_text) = th.brand(u.provider);
    draw_logo(gfx, th, u.provider, x, y + 1.0);
    let name = u.provider.name();
    let name_w = gfx.text_width(name, &gfx.fonts.name);
    gfx.text(
        name,
        &gfx.fonts.name,
        rect(x + 30.0, y, name_w + 2.0, CARD_HEAD),
        th.text1,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    );
    if let Some(plan) = &u.plan {
        let pw = gfx.text_width(plan, &gfx.fonts.pill) + 14.0;
        let pill = rect(x + 30.0 + name_w + 8.0, y + 2.0, pw, 18.0);
        gfx.fill(pill, 9.0, D2D1_COLOR_F { a: if th.dark { 0.16 } else { 0.12 }, ..brand });
        gfx.text(plan, &gfx.fonts.pill, pill, pill_text, DWRITE_TEXT_ALIGNMENT_CENTER);
    }
    if open_hovered {
        gfx.fill(rect(x + w - 22.0, y - 3.0, 28.0, CARD_HEAD + 6.0), 6.0, th.button_hover);
    }
    let open = if open_hovered { th.text1 } else { th.text3 };
    gfx.text(
        "\u{E8A7}",
        &gfx.fonts.icon_small,
        rect(x + w - 20.0, y, 20.0, CARD_HEAD),
        open,
        DWRITE_TEXT_ALIGNMENT_TRAILING,
    );
    // Données reprises d'un fetch précédent (réseau, limite de requêtes…)
    if let Some(since) = u.stale_since {
        let age = timefmt::age_text(since, timefmt::now());
        let right = x + w - 28.0;
        let age_w = gfx.text_width(&age, &gfx.fonts.caption);
        let age_rect = rect(right - age_w, y, age_w, CARD_HEAD);
        gfx.text(&age, &gfx.fonts.caption, age_rect, th.text3, DWRITE_TEXT_ALIGNMENT_TRAILING);
        let icon_rect = rect(right - age_w - 18.0, y, 14.0, CARD_HEAD);
        gfx.text("\u{E7BA}", &gfx.fonts.icon_small, icon_rect, th.warn, DWRITE_TEXT_ALIGNMENT_TRAILING);
    }
}

fn draw_logo(gfx: &Gfx, th: &Theme, p: Provider, x: f32, y: f32) {
    let color = match p {
        Provider::Claude => rgb(0xD97757, 1.0),
        Provider::Codex => th.text1,
        Provider::Amp => {
            // Icône d'app Amp : carré vert sombre, mot-symbole crème.
            let badge = rect(x, y, 20.0, 20.0);
            gfx.fill_vertical_gradient(badge, 5.0, rgb(0x2C3A2A, 1.0), rgb(0x061615, 1.0));
            gfx.stroke(badge, 5.0, rgb(0xDFDFC1, if th.dark { 0.14 } else { 0.0 }));
            let (vw, _) = logos::AMP_VIEWBOX;
            let s = 15.5 / vw;
            // le mot occupe y 20..143 du viewBox : on centre cette zone
            gfx.path(&gfx.logos[2], x + 2.25, y + 10.0 - 81.5 * s, s, rgb(0xDFDFC1, 1.0));
            return;
        }
    };
    gfx.logo(p, x, y, 20.0, color);
}

#[allow(clippy::too_many_arguments)]
fn draw_row(
    gfx: &Gfx,
    th: &Theme,
    l: &Limit,
    brand: D2D1_COLOR_F,
    progress: f32,
    (x, y): (f32, f32),
    w: f32,
    expanded: bool,
) {
    let value = i18n::percent(l.percent);
    let label = l.label();
    gfx.text(&label, &gfx.fonts.label, rect(x, y, w - 76.0, 18.0), th.text1, DWRITE_TEXT_ALIGNMENT_LEADING);
    // Restant en dollars ou en heures (Amp), à la suite du libellé.
    if let Some(amount) = l.amount {
        let lw = gfx.text_width(&label, &gfx.fonts.label);
        let r = rect(x + lw + 6.0, y, (w - 76.0 - lw - 6.0).max(0.0), 18.0);
        gfx.text(&format!("· {}", amount.text()), &gfx.fonts.label, r, th.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
    }
    gfx.text(
        &value,
        &gfx.fonts.value,
        rect(x + w - 76.0, y, 60.0, 18.0),
        th.level(l.percent, th.text1),
        DWRITE_TEXT_ALIGNMENT_TRAILING,
    );
    let chevron = if expanded { "\u{E70E}" } else { "\u{E70D}" };
    gfx.text(
        chevron,
        &gfx.fonts.icon_small,
        rect(x + w - 12.0, y, 12.0, 18.0),
        th.text3,
        DWRITE_TEXT_ALIGNMENT_TRAILING,
    );

    let now = timefmt::now();
    let bar = rect(x, y + 25.0, w, 6.0);
    gfx.fill(bar, 3.0, th.track);
    let frac = (l.percent / 100.0).clamp(0.0, 1.0) * progress;
    if frac > 0.0 {
        let fill_w = (w * frac).max(6.0);
        gfx.fill(rect(x, y + 25.0, fill_w, 6.0), 3.0, th.level(l.percent, brand));
    }
    // Repère du rythme : là où on en serait en consommant régulièrement.
    if let Some(pace) = l.pace(now) {
        let px = (x + w * pace).round();
        gfx.fill(rect(px - 1.0, y + 22.0, 2.0, 12.0), 1.0, th.pace);
    }

    if let Some(at) = l.resets_at {
        // Les abonnements Amp se renouvellent à une date, pas à une heure.
        let text = match l.kind {
            LimitKind::AmpAgent | LimitKind::AmpOrbs(_) => timefmt::renewal_text(at),
            _ => timefmt::reset_text(at, now),
        };
        gfx.text(&text, &gfx.fonts.caption, rect(x, y + 36.0, w, 16.0), th.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
    }
}

/// Courbe de la fenêtre en cours : consommation relevée, rythme régulier
/// (diagonale) et projection au rythme récent jusqu'à la réinitialisation.
#[allow(clippy::too_many_arguments)]
fn draw_chart(
    gfx: &Gfx,
    th: &Theme,
    l: &Limit,
    samples: &[Sample],
    brand: D2D1_COLOR_F,
    reveal: f32,
    (x, y): (f32, f32),
    w: f32,
) {
    let top = y + CHART_TOP;
    let bottom = top + CHART_PLOT;
    let caption = rect(x, bottom + 20.0, w, 16.0);
    let (Some(reset), Some(window)) = (l.resets_at, l.window_secs) else {
        let msg = tr("Pas de fenêtre de temps connue pour cette limite.", "No known time window for this limit.");
        gfx.text(msg, &gfx.fonts.caption, caption, th.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
        return;
    };
    let start = reset - window;
    let now = timefmt::now();
    let with_day = window > 86400;
    let labels = rect(x, bottom + 3.0, w, 12.0);
    let (from, to) = (timefmt::axis_label(start, with_day), timefmt::axis_label(reset, with_day));
    gfx.text(&from, &gfx.fonts.tiny, labels, th.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
    gfx.text(&to, &gfx.fonts.tiny, labels, th.text3, DWRITE_TEXT_ALIGNMENT_TRAILING);
    let tx = |t: i64| x + w * ((t - start) as f32 / window as f32).clamp(0.0, 1.0);
    let py = |p: f32| bottom - CHART_PLOT * (p / 100.0).clamp(0.0, 1.0);
    let faint = |c: D2D1_COLOR_F, k: f32| D2D1_COLOR_F { a: c.a * k, ..c };

    // Fond : repères 0 / 50 / 100 % et diagonale du rythme régulier
    // (0 % au début de la fenêtre, 100 % à sa réinitialisation).
    for p in [0.0, 50.0, 100.0] {
        gfx.fill(rect(x, py(p) - 0.5, w, 1.0), 0.0, faint(th.track, if p == 0.0 { 1.0 } else { 0.6 }));
    }
    gfx.dashed((x, bottom), (x + w, top), 1.2, faint(th.pace, 0.7));
    let legend = rect(x + w - 124.0, top + 17.0, 120.0, 12.0);
    let steady = tr("rythme régulier", "steady pace");
    gfx.text(steady, &gfx.fonts.tiny, legend, faint(th.pace, 0.9), DWRITE_TEXT_ALIGNMENT_TRAILING);
    gfx.text(
        &i18n::percent(100.0),
        &gfx.fonts.tiny,
        rect(x + 3.0, top + 1.0, 60.0, 12.0),
        th.text3,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    );

    unsafe {
        gfx.target.PushAxisAlignedClip(
            &rect(x - 6.0, top - 6.0, (w + 12.0) * reveal, CHART_PLOT + 12.0),
            D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
        );
    }
    let projection = history::project(samples, start, reset);
    if let Some(last) = samples.last() {
        // Une fenêtre démarre à 0 % : on part de là.
        let mut points = vec![(tx(start), py(0.0))];
        points.extend(samples.iter().map(|s| (tx(s.ts), py(s.percent))));
        gfx.area(&points, top, bottom, brand);

        let line = th.level(l.percent, brand);
        for (i, pair) in points.windows(2).enumerate() {
            // Segment avant le premier relevé, ou trou (PC éteint) : interpolé.
            let interpolated = i == 0 && samples[0].ts - start > history::GAP || i > 0 && samples[i].gap;
            if interpolated {
                gfx.dashed(pair[0], pair[1], 1.6, faint(line, 0.8));
            } else {
                gfx.line(pair[0], pair[1], 1.8, line);
            }
        }

        let here = (tx(last.ts), py(last.percent));
        if let Some(p) = &projection {
            let (end, hits) = match p.limit_at {
                Some(t) if t < reset => ((tx(t), py(100.0)), true),
                _ => ((tx(reset), py(p.at_reset)), false),
            };
            gfx.dashed(here, end, 1.6, faint(if hits { th.crit } else { line }, 0.75));
            if let (true, Some(t)) = (hits, p.limit_at) {
                gfx.disc(end, 3.0, th.crit);
                let label = timefmt::axis_label(t, with_day);
                gfx.text(
                    &label,
                    &gfx.fonts.tiny,
                    rect(end.0 - 40.0, end.1 + 4.0, 80.0, 12.0),
                    th.crit,
                    DWRITE_TEXT_ALIGNMENT_CENTER,
                );
            }
        }
        gfx.disc(here, 6.0, faint(line, 0.25));
        gfx.disc(here, 3.2, line);
    }
    unsafe { gfx.target.PopAxisAlignedClip() };

    let (text, color) = match (samples.last(), &projection) {
        (None, _) => (tr("Courbe en cours de constitution…", "Building up history…").to_string(), th.text3),
        (_, None) => (
            tr("Pas encore assez de relevés pour estimer le rythme.", "Not enough data yet to estimate the pace.")
                .to_string(),
            th.text3,
        ),
        (_, Some(p)) => match p.limit_at {
            Some(t) if t < reset => {
                let eta = timefmt::eta_text(t, now);
                (format!("{} {eta}", tr("À ce rythme, limite atteinte", "At this pace, limit reached")), th.warn)
            }
            _ => {
                let at = i18n::percent(p.at_reset.min(100.0));
                (
                    format!(
                        "{} ~{at} {}",
                        tr("À ce rythme :", "At this pace:"),
                        tr("à la réinitialisation", "at reset")
                    ),
                    th.text2,
                )
            }
        },
    };
    gfx.text(&text, &gfx.fonts.caption, caption, color, DWRITE_TEXT_ALIGNMENT_LEADING);
}

fn draw_resets(gfx: &Gfx, th: &Theme, r: &BankedResets, brand: D2D1_COLOR_F, (x, y): (f32, f32), w: f32) {
    let h = RESETS_H - 8.0;
    gfx.text("\u{E777}", &gfx.fonts.icon_small, rect(x, y, 14.0, h), brand, DWRITE_TEXT_ALIGNMENT_LEADING);
    let mut text = match (i18n::lang(), r.count) {
        (Lang::Fr, 1) => "1 reset gratuit en réserve".to_string(),
        (Lang::Fr, n) => format!("{n} resets gratuits en réserve"),
        (Lang::En, 1) => "1 free reset available".to_string(),
        (Lang::En, n) => format!("{n} free resets available"),
    };
    if let Some(at) = r.next_expiry {
        text.push_str(&format!(" · {} {}", tr("expire", "expires"), timefmt::date_text(at)));
    }
    gfx.text(&text, &gfx.fonts.caption, rect(x + 20.0, y, w - 20.0, h), th.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
}

/// « Crédits : 20 $ · Test Team : 7,25 $ »
fn draw_credits(gfx: &Gfx, th: &Theme, credits: &[Credit], brand: D2D1_COLOR_F, (x, y): (f32, f32), w: f32) {
    let h = RESETS_H - 8.0;
    gfx.text("\u{E825}", &gfx.fonts.icon_small, rect(x, y, 14.0, h), brand, DWRITE_TEXT_ALIGNMENT_LEADING);
    let colon = tr(" : ", ": ");
    let parts: Vec<String> = credits
        .iter()
        .map(|c| match &c.workspace {
            None => format!("{}{colon}{}", tr("Crédits", "Credits"), dollars(c.dollars)),
            Some(name) => format!("{name}{colon}{}", dollars(c.dollars)),
        })
        .collect();
    gfx.text(
        &parts.join(" · "),
        &gfx.fonts.caption,
        rect(x + 20.0, y, w - 20.0, h),
        th.text2,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    );
}

struct SettingsCtx<'a> {
    autostart: bool,
    confirm_clear: bool,
    memory: &'a str,
    amp_token: bool,
    /// Retour d'action sur le token (« Token enregistré »…).
    amp_note: Option<&'a str>,
}

/// « AI Quota 0.1.0 · 6,3 Mo en mémoire »
fn memory_text() -> String {
    let bytes = sys::private_memory();
    let mb = bytes as f64 / (1024.0 * 1024.0);
    let version = env!("CARGO_PKG_VERSION");
    match i18n::lang() {
        Lang::Fr => format!("AI Quota {version} · {} Mo en mémoire", format!("{mb:.1}").replace('.', ",")),
        Lang::En => format!("AI Quota {version} · {mb:.1} MB in memory"),
    }
}

fn draw_settings(gfx: &Gfx, th: &Theme, hits: &mut Vec<(D2D_RECT_F, Hit)>, hover: Option<Hit>, ctx: &SettingsCtx) {
    let app = app();
    let settings = app.settings.borrow().clone();

    // En-tête : retour + titre
    let back = rect(MARGIN, 12.0, 32.0, 32.0);
    hits.push((back, Hit::Back));
    if hover == Some(Hit::Back) {
        gfx.fill(back, 6.0, th.button_hover);
    }
    gfx.text("\u{E72B}", &gfx.fonts.icon, back, th.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
    let title = tr("Réglages", "Settings");
    gfx.text(title, &gfx.fonts.title, rect(MARGIN + 40.0, 12.0, 200.0, 32.0), th.text1, DWRITE_TEXT_ALIGNMENT_LEADING);

    let x = MARGIN;
    let w = WIDTH - 2.0 * MARGIN;
    let mut y = HEADER_H;
    let section = |name: &str, rows: usize, y: &mut f32| -> f32 {
        gfx.text(
            name,
            &gfx.fonts.value,
            rect(x + 4.0, *y, w, SECTION_TITLE - 4.0),
            th.text2,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        );
        let card = rect(x, *y + SECTION_TITLE, w, 2.0 * SECTION_PAD + rows as f32 * SETTING_ROW);
        gfx.fill(card, 8.0, th.card);
        gfx.stroke(card, 8.0, th.card_border);
        for i in 1..rows {
            let sy = card.top + SECTION_PAD + i as f32 * SETTING_ROW;
            gfx.fill(rect(x + 14.0, sy - 0.5, w - 28.0, 1.0), 0.0, th.card_border);
        }
        let first_row = card.top + SECTION_PAD;
        *y = card.bottom + SECTION_GAP;
        first_row
    };
    // `reserve` : place laissée au contrôle à droite.
    let label = |text: &str, ry: f32, indent: f32, reserve: f32| {
        let r = rect(x + 14.0 + indent, ry, w - 28.0 - indent - reserve, SETTING_ROW);
        gfx.text(text, &gfx.fonts.label, r, th.text1, DWRITE_TEXT_ALIGNMENT_LEADING);
    };
    // Ligne entière cliquable (interrupteurs).
    let row_hit = |hits: &mut Vec<(D2D_RECT_F, Hit)>, ry: f32, ctl: Ctl| {
        let r = rect(x + 4.0, ry + 2.0, w - 8.0, SETTING_ROW - 4.0);
        hits.push((r, Hit::Ctl(ctl)));
        if hover == Some(Hit::Ctl(ctl)) {
            gfx.fill(r, 6.0, th.button_hover);
        }
    };

    // Général
    let ry = section(tr("Général", "General"), 3, &mut y);
    label(tr("Langue", "Language"), ry, 0.0, 180.0);
    let langs = [
        ("Français", Ctl::Lang(Lang::Fr), settings.lang == Lang::Fr),
        ("English", Ctl::Lang(Lang::En), settings.lang == Lang::En),
    ];
    segmented(gfx, th, hits, hover, x + w - 14.0, ry, &langs);
    label(tr("Thème", "Theme"), ry + SETTING_ROW, 0.0, 180.0);
    let themes = [
        (tr("Auto", "Auto"), Ctl::Theme(ThemePref::Auto), settings.theme == ThemePref::Auto),
        (tr("Clair", "Light"), Ctl::Theme(ThemePref::Light), settings.theme == ThemePref::Light),
        (tr("Sombre", "Dark"), Ctl::Theme(ThemePref::Dark), settings.theme == ThemePref::Dark),
    ];
    segmented(gfx, th, hits, hover, x + w - 14.0, ry + SETTING_ROW, &themes);
    let ry3 = ry + 2.0 * SETTING_ROW;
    row_hit(hits, ry3, Ctl::Autostart);
    label(tr("Lancer au démarrage de Windows", "Start with Windows"), ry3, 0.0, 52.0);
    toggle(gfx, th, x + w - 14.0, ry3, ctx.autostart);

    // Providers
    let ry = section(tr("Outils suivis", "Tracked tools"), 3, &mut y);
    for (i, p) in Provider::ALL.into_iter().enumerate() {
        let py = ry + i as f32 * SETTING_ROW;
        row_hit(hits, py, Ctl::Provider(p));
        draw_logo(gfx, th, p, x + 14.0, py + 10.0);
        label(p.name(), py, 30.0, 90.0);
        toggle(gfx, th, x + w - 14.0, py, settings.is_enabled(p));
        if has_settings_page(p) {
            // Après la ligne : la roue crantée l'emporte sur l'interrupteur.
            let cog = rect(x + w - 14.0 - 40.0 - 36.0, py + 6.0, 28.0, SETTING_ROW - 12.0);
            let ctl = Ctl::ProviderPage(p);
            hits.push((cog, Hit::Ctl(ctl)));
            if hover == Some(Hit::Ctl(ctl)) {
                gfx.fill(cog, 6.0, th.button_hover);
            }
            gfx.text("\u{E713}", &gfx.fonts.icon, cog, th.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
        }
    }

    // Actualisation
    let ry = section(tr("Actualisation", "Refresh"), 2, &mut y);
    label(tr("Toutes les", "Every"), ry, 0.0, 240.0);
    let polls: Vec<(String, Ctl, bool)> =
        POLL_CHOICES.iter().map(|&m| (format!("{m}\u{a0}min"), Ctl::Poll(m), settings.poll_minutes == m)).collect();
    let polls: Vec<(&str, Ctl, bool)> = polls.iter().map(|(s, c, b)| (s.as_str(), *c, *b)).collect();
    segmented(gfx, th, hits, hover, x + w - 14.0, ry, &polls);
    let ry2 = ry + SETTING_ROW;
    row_hit(hits, ry2, Ctl::FasterWhenHot);
    let hot = match i18n::lang() {
        Lang::Fr => "Toutes les 2\u{a0}min au-delà de 80\u{a0}%",
        Lang::En => "Every 2 min above 80%",
    };
    label(hot, ry2, 0.0, 52.0);
    toggle(gfx, th, x + w - 14.0, ry2, settings.poll_faster_when_hot);

    // Données
    let ry = section(tr("Données", "Data"), 1, &mut y);
    let bw = (w - 28.0 - 8.0) / 2.0;
    let open = rect(x + 14.0, ry + 5.0, bw, SETTING_ROW - 10.0);
    let clear = rect(open.right + 8.0, ry + 5.0, bw, SETTING_ROW - 10.0);
    let clear_label =
        if ctx.confirm_clear { tr("Confirmer ?", "Confirm?") } else { tr("Vider l’historique", "Clear history") };
    button(gfx, th, hits, hover, open, tr("Ouvrir le dossier", "Open folder"), Ctl::OpenFolder, false);
    button(gfx, th, hits, hover, clear, clear_label, Ctl::ClearHistory, ctx.confirm_clear);

    let footer = rect(x + 4.0, y - SECTION_GAP + 4.0, w - 8.0, FOOTER_H - 4.0);
    gfx.text(ctx.memory, &gfx.fonts.caption, footer, th.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
}

/// Page de réglages d'un provider : pour l'instant le token d'Amp.
fn draw_provider_settings(
    gfx: &Gfx,
    th: &Theme,
    hits: &mut Vec<(D2D_RECT_F, Hit)>,
    hover: Option<Hit>,
    ctx: &SettingsCtx,
    p: Provider,
) {
    // En-tête : retour + logo + nom
    let back = rect(MARGIN, 12.0, 32.0, 32.0);
    hits.push((back, Hit::Back));
    if hover == Some(Hit::Back) {
        gfx.fill(back, 6.0, th.button_hover);
    }
    gfx.text("\u{E72B}", &gfx.fonts.icon, back, th.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
    draw_logo(gfx, th, p, MARGIN + 42.0, 18.0);
    gfx.text(
        p.name(),
        &gfx.fonts.title,
        rect(MARGIN + 70.0, 12.0, 200.0, 32.0),
        th.text1,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    );

    let x = MARGIN;
    let w = WIDTH - 2.0 * MARGIN;
    let y = HEADER_H;
    let title = tr("Token d’accès", "Access token");
    gfx.text(
        title,
        &gfx.fonts.value,
        rect(x + 4.0, y, w, SECTION_TITLE - 4.0),
        th.text2,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    );
    let card = rect(x, y + SECTION_TITLE, w, 2.0 * SECTION_PAD + 2.0 * SETTING_ROW);
    gfx.fill(card, 8.0, th.card);
    gfx.stroke(card, 8.0, th.card_border);
    let ry = card.top + SECTION_PAD;
    gfx.fill(rect(x + 14.0, ry + SETTING_ROW - 0.5, w - 28.0, 1.0), 0.0, th.card_border);

    // Ligne 1 : état + Coller / Retirer
    let state = ctx.amp_note.unwrap_or(if ctx.amp_token {
        tr("Token enregistré (chiffré)", "Token saved (encrypted)")
    } else {
        tr("Aucun token", "No token")
    });
    let mut right = x + w - 14.0;
    if ctx.amp_token {
        let remove = rect(right - 70.0, ry + 7.0, 70.0, SETTING_ROW - 14.0);
        button(gfx, th, hits, hover, remove, tr("Retirer", "Remove"), Ctl::ClearAmpToken, false);
        right = remove.left - 6.0;
    }
    let paste = rect(right - 70.0, ry + 7.0, 70.0, SETTING_ROW - 14.0);
    button(gfx, th, hits, hover, paste, tr("Coller", "Paste"), Ctl::PasteAmpToken, false);
    let color = if ctx.amp_token { th.text1 } else { th.text2 };
    gfx.text(
        state,
        &gfx.fonts.label,
        rect(x + 14.0, ry, paste.left - x - 20.0, SETTING_ROW),
        color,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    );

    // Ligne 2 : lien vers la page où créer le token
    let ly = ry + SETTING_ROW;
    let link = rect(x + 4.0, ly + 2.0, w - 8.0, SETTING_ROW - 4.0);
    let ctl = Ctl::OpenTokenPage;
    hits.push((link, Hit::Ctl(ctl)));
    let hovered = hover == Some(Hit::Ctl(ctl));
    if hovered {
        gfx.fill(link, 6.0, th.button_hover);
    }
    let text = tr("Récupérer un token sur ampcode.com", "Get a token on ampcode.com");
    let tw = gfx.text_width(text, &gfx.fonts.label);
    let tr_ = rect(x + 14.0, ly, tw + 2.0, SETTING_ROW);
    gfx.text(text, &gfx.fonts.label, tr_, th.accent(), DWRITE_TEXT_ALIGNMENT_LEADING);
    if hovered {
        let uy = ly + SETTING_ROW / 2.0 + 9.0;
        gfx.fill(rect(x + 14.0, uy, tw, 1.0), 0.0, th.accent());
    }
    gfx.text(
        "\u{E8A7}",
        &gfx.fonts.icon_small,
        rect(x + 14.0 + tw + 6.0, ly, 14.0, SETTING_ROW),
        th.accent(),
        DWRITE_TEXT_ALIGNMENT_LEADING,
    );

    // Aide
    let help = tr(
        "Crée un token d’accès dans tes réglages Amp, copie-le, puis clique sur Coller. Il est chiffré avec ta session Windows.",
        "Create an access token in your Amp settings, copy it, then click Paste. It is encrypted with your Windows session.",
    );
    unsafe {
        let _ = gfx.fonts.caption_wrap.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
    }
    let hr = rect(x + 4.0, card.bottom + 8.0, w - 8.0, PROVIDER_HELP_H - 8.0);
    gfx.text(help, &gfx.fonts.caption_wrap, hr, th.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
}

/// Sélecteur segmenté aligné à droite (`right`), centré dans une ligne.
fn segmented(
    gfx: &Gfx,
    th: &Theme,
    hits: &mut Vec<(D2D_RECT_F, Hit)>,
    hover: Option<Hit>,
    right: f32,
    row_y: f32,
    items: &[(&str, Ctl, bool)],
) {
    let widths: Vec<f32> = items.iter().map(|(t, ..)| gfx.text_width(t, &gfx.fonts.caption) + 18.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 4.0;
    let h = 26.0;
    let top = row_y + (SETTING_ROW - h) / 2.0;
    let container = rect(right - total, top, total, h);
    gfx.fill(container, 6.0, th.track);
    let mut sx = container.left + 2.0;
    for ((text, ctl, selected), sw) in items.iter().zip(&widths) {
        let seg = rect(sx, top + 2.0, *sw, h - 4.0);
        hits.push((seg, Hit::Ctl(*ctl)));
        let color = if *selected {
            gfx.fill(seg, 4.0, th.accent());
            th.on_accent()
        } else {
            if hover == Some(Hit::Ctl(*ctl)) {
                gfx.fill(seg, 4.0, th.button_hover);
            }
            th.text2
        };
        gfx.text(text, &gfx.fonts.caption, seg, color, DWRITE_TEXT_ALIGNMENT_CENTER);
        sx += sw;
    }
}

/// Interrupteur façon Windows 11, aligné à droite.
fn toggle(gfx: &Gfx, th: &Theme, right: f32, row_y: f32, on: bool) {
    let (w, h) = (40.0, 20.0);
    let r = rect(right - w, row_y + (SETTING_ROW - h) / 2.0, w, h);
    let cy = (r.top + r.bottom) / 2.0;
    if on {
        gfx.fill(r, h / 2.0, th.accent());
        gfx.disc((r.right - 10.0, cy), 6.0, th.on_accent());
    } else {
        gfx.stroke(r, h / 2.0, th.text2);
        gfx.disc((r.left + 10.0, cy), 5.0, th.text2);
    }
}

#[allow(clippy::too_many_arguments)]
fn button(
    gfx: &Gfx,
    th: &Theme,
    hits: &mut Vec<(D2D_RECT_F, Hit)>,
    hover: Option<Hit>,
    r: D2D_RECT_F,
    text: &str,
    ctl: Ctl,
    danger: bool,
) {
    hits.push((r, Hit::Ctl(ctl)));
    let hovered = hover == Some(Hit::Ctl(ctl));
    let fill = if danger {
        D2D1_COLOR_F { a: 0.18, ..th.crit }
    } else if hovered {
        th.button_hover
    } else {
        th.card
    };
    gfx.fill(r, 6.0, fill);
    gfx.stroke(r, 6.0, if danger { th.crit } else { th.card_border });
    gfx.text(text, &gfx.fonts.caption, r, if danger { th.crit } else { th.text1 }, DWRITE_TEXT_ALIGNMENT_CENTER);
}

fn note_amp(text: &str) {
    with_popup(|p| {
        p.amp_token = secret::has_amp_token();
        p.amp_note = Some((text.to_string(), Instant::now()));
        unsafe { SetTimer(Some(p.hwnd), CONFIRM_TIMER, (CONFIRM_SECS * 1000.0) as u32, None) };
    });
}

fn apply_setting(ctl: Ctl) {
    let app = app();
    match ctl {
        Ctl::Lang(lang) => {
            app.update_settings(|s| s.lang = lang);
            with_popup(|p| p.memory = memory_text());
        }
        Ctl::Theme(theme) => app.update_settings(|s| s.theme = theme),
        Ctl::Provider(p) => app.update_settings(|s| s.enabled[p as usize] = !s.enabled[p as usize]),
        Ctl::Poll(minutes) => app.update_settings(|s| s.poll_minutes = minutes),
        Ctl::FasterWhenHot => app.update_settings(|s| s.poll_faster_when_hot = !s.poll_faster_when_hot),
        Ctl::Autostart => {
            sys::set_autostart(!sys::autostart_enabled());
            with_popup(|p| p.autostart = sys::autostart_enabled());
        }
        Ctl::OpenFolder => {
            close();
            if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
                let dir = std::path::PathBuf::from(dir).join("ai-quota");
                let _ = std::fs::create_dir_all(&dir);
                unsafe {
                    ShellExecuteW(None, w!("open"), &HSTRING::from(dir.as_os_str()), None, None, SW_SHOWNORMAL);
                }
            }
        }
        Ctl::ProviderPage(p) => {
            with_popup(|popup| popup.set_view(View::Provider(p)));
        }
        Ctl::OpenTokenPage => unsafe {
            ShellExecuteW(None, w!("open"), &HSTRING::from(AMP_TOKEN_URL), None, None, SW_SHOWNORMAL);
        },
        Ctl::PasteAmpToken => {
            let token = secret::clipboard_text().map(|t| t.trim().to_string()).unwrap_or_default();
            let plausible = token.len() >= 16 && !token.chars().any(char::is_whitespace);
            let note = if plausible && secret::save_amp_token(&token) {
                app.refresh(false);
                tr("Token enregistré", "Token saved")
            } else {
                tr("Pas de token dans le presse-papiers", "No token in the clipboard")
            };
            note_amp(note);
        }
        Ctl::ClearAmpToken => {
            secret::clear_amp_token();
            if let Some(snap) = app.snapshot.borrow_mut().as_mut() {
                snap.usages.retain(|u| u.provider != Provider::Amp);
            }
            note_amp(tr("Token retiré", "Token removed"));
        }
        Ctl::ClearHistory => {
            let confirmed = with_popup(|p| {
                let confirmed = p.confirm_clear.is_some_and(|at| at.elapsed().as_secs_f32() < CONFIRM_SECS);
                p.confirm_clear = if confirmed { None } else { Some(Instant::now()) };
                if !confirmed {
                    unsafe { SetTimer(Some(p.hwnd), CONFIRM_TIMER, (CONFIRM_SECS * 1000.0) as u32, None) };
                }
                confirmed
            })
            .unwrap_or(false);
            if confirmed {
                app.history.borrow_mut().clear();
            }
        }
    }
    with_popup(|p| unsafe {
        let _ = InvalidateRect(Some(p.hwnd), None, false);
    });
}

fn draw_message(gfx: &Gfx, th: &Theme, msg: &str, is_error: bool, x: f32, y: f32, w: f32) {
    let (tx, color) = if is_error {
        gfx.text("\u{E7BA}", &gfx.fonts.icon, rect(x, y, 16.0, 20.0), th.warn, DWRITE_TEXT_ALIGNMENT_LEADING);
        (x + 24.0, th.text1)
    } else {
        (x, th.text2)
    };
    unsafe {
        let _ = gfx.fonts.message.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
    }
    gfx.text(msg, &gfx.fonts.message, rect(tx, y + 1.0, w - (tx - x), MESSAGE_H), color, DWRITE_TEXT_ALIGNMENT_LEADING);
}

enum Action {
    Refresh,
    Open(Provider),
    /// Déplie / replie la courbe d'une limite.
    Toggle(String),
    View(View),
    /// Retour à la vue parente (page provider → réglages → principale).
    Back,
    Setting(Ctl),
    Close,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                with_popup(|p| p.render());
                let _ = ValidateRect(Some(hwnd), None);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            // Zone client = fenêtre entière, y compris pendant la création
            // (wParam = 0), sinon Windows y retranche le cadre de redimensionnement.
            WM_NCCALCSIZE => LRESULT(0),
            // Pas de cadre classique repeint par-dessus (liseré blanc après un
            // redimensionnement) : DWM s'occupe des bords et de l'ombre.
            WM_NCPAINT => LRESULT(0),
            WM_NCACTIVATE => DefWindowProcW(hwnd, msg, wparam, LPARAM(-1)),
            WM_NCHITTEST => LRESULT(HTCLIENT as isize),
            WM_TIMER => {
                if wparam.0 == CONFIRM_TIMER {
                    let _ = KillTimer(Some(hwnd), CONFIRM_TIMER);
                }
                if matches!(wparam.0, ANIM_TIMER | CLOCK_TIMER | CONFIRM_TIMER) {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_ACTIVATE => {
                if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
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
                        p.tracking = TrackMouseEvent(&mut tme).is_ok();
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
                    let pressed = p.pressed.take();
                    match hit {
                        Some(Hit::Refresh) if pressed == hit => Some(Action::Refresh),
                        Some(Hit::Open(provider)) if pressed == hit => Some(Action::Open(provider)),
                        Some(Hit::Row(i)) if pressed == hit => p.row_keys.get(i).cloned().map(Action::Toggle),
                        Some(Hit::Gear) if pressed == hit => Some(Action::View(View::Settings)),
                        Some(Hit::Back) if pressed == hit => Some(Action::Back),
                        Some(Hit::Ctl(ctl)) if pressed == hit => Some(Action::Setting(ctl)),
                        _ => None,
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
                    run(Some(Action::Refresh)); // R ou F5
                }
                LRESULT(0)
            }
            WM_DPICHANGED => {
                with_popup(|p| {
                    p.dpi = (wparam.0 & 0xFFFF) as f32;
                    p.gfx.target.SetDpi(p.dpi, p.dpi);
                    p.place();
                });
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let popup = POPUP.with(|p| p.try_borrow_mut().ok().and_then(|mut p| p.take()));
                drop(popup);
                crate::popup_closed();
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

fn run(action: Option<Action>) {
    match action {
        Some(Action::Refresh) => app().refresh(true),
        Some(Action::Open(provider)) => {
            close();
            unsafe {
                ShellExecuteW(None, w!("open"), &HSTRING::from(provider.usage_url()), None, None, SW_SHOWNORMAL);
            }
        }
        Some(Action::Toggle(key)) => {
            {
                let mut expanded = app().expanded.borrow_mut();
                *expanded = if expanded.as_deref() == Some(key.as_str()) { None } else { Some(key) };
            }
            if let Some(hwnd) = with_popup(|p| {
                p.chart_opened = Instant::now();
                p.place();
                p.animate();
                p.hwnd
            }) {
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        }
        Some(Action::View(view)) => {
            if let Some(hwnd) = with_popup(|p| {
                p.set_view(view);
                p.hwnd
            }) {
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        }
        Some(Action::Back) => {
            if let Some(hwnd) = with_popup(|p| {
                let parent = if matches!(p.view, View::Provider(_)) { View::Settings } else { View::Main };
                p.set_view(parent);
                p.hwnd
            }) {
                unsafe {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        }
        Some(Action::Setting(ctl)) => apply_setting(ctl),
        Some(Action::Close) => close(),
        None => {}
    }
}
