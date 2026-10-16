//! Settings view, the per-provider page (Amp token), their controls and actions.

use std::time::Instant;

use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{SW_SHOWNORMAL, SetTimer};
use windows::core::{HSTRING, w};

use super::gfx::Gfx;
use super::theme::Theme;
use super::{CONFIRM_SECS, CONFIRM_TIMER, HEADER_H, Hit, Hits, MARGIN, View, WIDTH, close, rect, with_popup};
use crate::i18n::{self, Lang, tr};
use crate::model::Provider;
use crate::settings::{POLL_CHOICES, ThemePref};
use crate::{app, secret, sys};

const SECTION_TITLE: f32 = 24.0;
const SETTING_ROW: f32 = 40.0;
/// Vertical padding inside a section card.
const SECTION_PAD: f32 = 4.0;
const SECTION_GAP: f32 = 10.0;
const FOOTER_H: f32 = 26.0;
const PROVIDER_HELP_H: f32 = 44.0;

const AMP_TOKEN_URL: &str = "https://ampcode.com/settings";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Ctl {
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
    /// A provider's cog: opens its settings page.
    ProviderPage(Provider),
    /// "Get a token" link.
    OpenTokenPage,
}

/// Providers that have their own settings page.
fn has_settings_page(p: Provider) -> bool {
    p == Provider::Amp
}

pub(super) struct SettingsCtx<'a> {
    pub(super) autostart: bool,
    pub(super) confirm_clear: bool,
    pub(super) memory: &'a str,
    pub(super) amp_token: bool,
    /// Feedback of the last token action ("Token saved"...).
    pub(super) amp_note: Option<&'a str>,
}

/// A titled card of `rows` setting rows.
struct Section {
    title: D2D_RECT_F,
    card: D2D_RECT_F,
    rows: usize,
}

impl Section {
    /// Stacks a section at `*y` and moves `*y` below it.
    fn stack(y: &mut f32, rows: usize) -> Self {
        let (x, w) = (MARGIN, WIDTH - 2.0 * MARGIN);
        let title = rect(x + 4.0, *y, w, SECTION_TITLE - 4.0);
        let card = rect(x, *y + SECTION_TITLE, w, 2.0 * SECTION_PAD + rows as f32 * SETTING_ROW);
        *y = card.bottom + SECTION_GAP;
        Section { title, card, rows }
    }

    /// Top of row `i`.
    fn row(&self, i: usize) -> f32 {
        debug_assert!(i < self.rows, "row {i} drawn outside a section of {} rows", self.rows);
        self.card.top + SECTION_PAD + i as f32 * SETTING_ROW
    }
}

pub(super) struct SettingsLayout {
    general: Section,
    tools: Section,
    refresh: Section,
    data: Section,
    footer: D2D_RECT_F,
    pub(super) height: f32,
}

pub(super) fn layout() -> SettingsLayout {
    let mut y = HEADER_H;
    let general = Section::stack(&mut y, 3);
    let tools = Section::stack(&mut y, Provider::ALL.len());
    let refresh = Section::stack(&mut y, 2);
    let data = Section::stack(&mut y, 1);
    let footer = rect(MARGIN + 4.0, data.card.bottom + 4.0, WIDTH - 2.0 * MARGIN - 8.0, FOOTER_H - 4.0);
    let height = data.card.bottom + FOOTER_H + MARGIN;
    SettingsLayout { general, tools, refresh, data, footer, height }
}

pub(super) struct ProviderLayout {
    token: Section,
    help: D2D_RECT_F,
    pub(super) height: f32,
}

pub(super) fn provider_layout() -> ProviderLayout {
    let mut y = HEADER_H;
    let token = Section::stack(&mut y, 2);
    let help = rect(MARGIN + 4.0, token.card.bottom + 8.0, WIDTH - 2.0 * MARGIN - 8.0, PROVIDER_HELP_H - 8.0);
    let height = token.card.bottom + PROVIDER_HELP_H + MARGIN;
    ProviderLayout { token, help, height }
}

/// "AI Quota 0.1.0 · 6.3 MB in memory"
pub(super) fn memory_text() -> String {
    let bytes = sys::private_memory();
    let mb = bytes as f64 / (1024.0 * 1024.0);
    let version = env!("CARGO_PKG_VERSION");
    match i18n::lang() {
        Lang::Fr => format!("AI Quota {version} · {} Mo en mémoire", format!("{mb:.1}").replace('.', ",")),
        Lang::En => format!("AI Quota {version} · {mb:.1} MB in memory"),
    }
}

fn draw_back(gfx: &Gfx, th: &Theme, hits: &mut Hits) {
    let back = rect(MARGIN, 12.0, 32.0, 32.0);
    if hits.add(back, Hit::Back) {
        gfx.fill(back, 6.0, th.button_hover);
    }
    gfx.text("\u{E72B}", &gfx.fonts.icon, back, th.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
}

fn draw_section(gfx: &Gfx, th: &Theme, name: &str, s: &Section) {
    gfx.text(name, &gfx.fonts.value, s.title, th.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
    gfx.fill(s.card, 8.0, th.card);
    gfx.stroke(s.card, 8.0, th.card_border);
    let (x, w) = (s.card.left, s.card.right - s.card.left);
    for i in 1..s.rows {
        gfx.fill(rect(x + 14.0, s.row(i) - 0.5, w - 28.0, 1.0), 0.0, th.card_border);
    }
}

pub(super) fn draw(gfx: &Gfx, th: &Theme, hits: &mut Hits, ctx: &SettingsCtx, layout: &SettingsLayout) {
    let settings = app().settings.borrow().clone();

    draw_back(gfx, th, hits);
    let title = tr("Réglages", "Settings");
    gfx.text(title, &gfx.fonts.title, rect(MARGIN + 40.0, 12.0, 200.0, 32.0), th.text1, DWRITE_TEXT_ALIGNMENT_LEADING);

    let x = MARGIN;
    let w = WIDTH - 2.0 * MARGIN;
    // `reserve`: room left for the control on the right.
    let label = |text: &str, ry: f32, indent: f32, reserve: f32| {
        let r = rect(x + 14.0 + indent, ry, w - 28.0 - indent - reserve, SETTING_ROW);
        gfx.text(text, &gfx.fonts.label, r, th.text1, DWRITE_TEXT_ALIGNMENT_LEADING);
    };
    // Whole row clickable (toggles).
    let row_hit = |hits: &mut Hits, ry: f32, ctl: Ctl| {
        let r = rect(x + 4.0, ry + 2.0, w - 8.0, SETTING_ROW - 4.0);
        if hits.add(r, Hit::Ctl(ctl)) {
            gfx.fill(r, 6.0, th.button_hover);
        }
    };
    let right = x + w - 14.0;

    let s = &layout.general;
    draw_section(gfx, th, tr("Général", "General"), s);
    label(tr("Langue", "Language"), s.row(0), 0.0, 180.0);
    let langs = [
        ("Français", Ctl::Lang(Lang::Fr), settings.lang == Lang::Fr),
        ("English", Ctl::Lang(Lang::En), settings.lang == Lang::En),
    ];
    segmented(gfx, th, hits, right, s.row(0), &langs);
    label(tr("Thème", "Theme"), s.row(1), 0.0, 180.0);
    let themes = [
        (tr("Auto", "Auto"), Ctl::Theme(ThemePref::Auto), settings.theme == ThemePref::Auto),
        (tr("Clair", "Light"), Ctl::Theme(ThemePref::Light), settings.theme == ThemePref::Light),
        (tr("Sombre", "Dark"), Ctl::Theme(ThemePref::Dark), settings.theme == ThemePref::Dark),
    ];
    segmented(gfx, th, hits, right, s.row(1), &themes);
    row_hit(hits, s.row(2), Ctl::Autostart);
    label(tr("Lancer au démarrage de Windows", "Start with Windows"), s.row(2), 0.0, 52.0);
    toggle(gfx, th, right, s.row(2), ctx.autostart);

    let s = &layout.tools;
    draw_section(gfx, th, tr("Outils suivis", "Tracked tools"), s);
    for (i, p) in Provider::ALL.into_iter().enumerate() {
        let py = s.row(i);
        row_hit(hits, py, Ctl::Provider(p));
        gfx.provider_logo(th, p, x + 14.0, py + 10.0);
        label(p.name(), py, 30.0, 90.0);
        toggle(gfx, th, right, py, settings.is_enabled(p));
        if has_settings_page(p) {
            // Registered after the row, so the cog wins over the toggle.
            let cog = rect(right - 40.0 - 36.0, py + 6.0, 28.0, SETTING_ROW - 12.0);
            if hits.add(cog, Hit::Ctl(Ctl::ProviderPage(p))) {
                gfx.fill(cog, 6.0, th.button_hover);
            }
            gfx.text("\u{E713}", &gfx.fonts.icon, cog, th.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
        }
    }

    let s = &layout.refresh;
    draw_section(gfx, th, tr("Actualisation", "Refresh"), s);
    label(tr("Toutes les", "Every"), s.row(0), 0.0, 240.0);
    let polls: Vec<(String, Ctl, bool)> =
        POLL_CHOICES.iter().map(|&m| (format!("{m}\u{a0}min"), Ctl::Poll(m), settings.poll_minutes == m)).collect();
    let polls: Vec<(&str, Ctl, bool)> = polls.iter().map(|(s, c, b)| (s.as_str(), *c, *b)).collect();
    segmented(gfx, th, hits, right, s.row(0), &polls);
    row_hit(hits, s.row(1), Ctl::FasterWhenHot);
    let hot = match i18n::lang() {
        Lang::Fr => "Toutes les 2\u{a0}min au-delà de 80\u{a0}%",
        Lang::En => "Every 2 min above 80%",
    };
    label(hot, s.row(1), 0.0, 52.0);
    toggle(gfx, th, right, s.row(1), settings.poll_faster_when_hot);

    let s = &layout.data;
    draw_section(gfx, th, tr("Données", "Data"), s);
    let bw = (w - 28.0 - 8.0) / 2.0;
    let open = rect(x + 14.0, s.row(0) + 5.0, bw, SETTING_ROW - 10.0);
    let clear = rect(open.right + 8.0, s.row(0) + 5.0, bw, SETTING_ROW - 10.0);
    let clear_label =
        if ctx.confirm_clear { tr("Confirmer ?", "Confirm?") } else { tr("Vider l’historique", "Clear history") };
    button(gfx, th, hits, open, tr("Ouvrir le dossier", "Open folder"), Ctl::OpenFolder, false);
    button(gfx, th, hits, clear, clear_label, Ctl::ClearHistory, ctx.confirm_clear);

    gfx.text(ctx.memory, &gfx.fonts.caption, layout.footer, th.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
}

/// A provider's settings page: for now, the Amp token.
pub(super) fn draw_provider(
    gfx: &Gfx,
    th: &Theme,
    hits: &mut Hits,
    ctx: &SettingsCtx,
    layout: &ProviderLayout,
    p: Provider,
) {
    draw_back(gfx, th, hits);
    gfx.provider_logo(th, p, MARGIN + 42.0, 18.0);
    gfx.text(
        p.name(),
        &gfx.fonts.title,
        rect(MARGIN + 70.0, 12.0, 200.0, 32.0),
        th.text1,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    );

    let x = MARGIN;
    let w = WIDTH - 2.0 * MARGIN;
    let s = &layout.token;
    draw_section(gfx, th, tr("Token d’accès", "Access token"), s);

    // Row 1: state + Paste / Remove.
    let ry = s.row(0);
    let state = ctx.amp_note.unwrap_or(if ctx.amp_token {
        tr("Token enregistré (chiffré)", "Token saved (encrypted)")
    } else {
        tr("Aucun token", "No token")
    });
    let mut right = x + w - 14.0;
    if ctx.amp_token {
        let remove = rect(right - 70.0, ry + 7.0, 70.0, SETTING_ROW - 14.0);
        button(gfx, th, hits, remove, tr("Retirer", "Remove"), Ctl::ClearAmpToken, false);
        right = remove.left - 6.0;
    }
    let paste = rect(right - 70.0, ry + 7.0, 70.0, SETTING_ROW - 14.0);
    button(gfx, th, hits, paste, tr("Coller", "Paste"), Ctl::PasteAmpToken, false);
    let color = if ctx.amp_token { th.text1 } else { th.text2 };
    gfx.text(
        state,
        &gfx.fonts.label,
        rect(x + 14.0, ry, paste.left - x - 20.0, SETTING_ROW),
        color,
        DWRITE_TEXT_ALIGNMENT_LEADING,
    );

    // Row 2: link to the page where the token is created.
    let ly = s.row(1);
    let link = rect(x + 4.0, ly + 2.0, w - 8.0, SETTING_ROW - 4.0);
    let hovered = hits.add(link, Hit::Ctl(Ctl::OpenTokenPage));
    if hovered {
        gfx.fill(link, 6.0, th.button_hover);
    }
    let text = tr("Récupérer un token sur ampcode.com", "Get a token on ampcode.com");
    let tw = gfx.text_width(text, &gfx.fonts.label);
    gfx.text(
        text,
        &gfx.fonts.label,
        rect(x + 14.0, ly, tw + 2.0, SETTING_ROW),
        th.accent(),
        DWRITE_TEXT_ALIGNMENT_LEADING,
    );
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

    let help = tr(
        "Crée un token d’accès dans tes réglages Amp, copie-le, puis clique sur Coller. Il est chiffré avec ta session Windows.",
        "Create an access token in your Amp settings, copy it, then click Paste. It is encrypted with your Windows session.",
    );
    gfx.text(help, &gfx.fonts.caption_wrap, layout.help, th.text3, DWRITE_TEXT_ALIGNMENT_LEADING);
}

/// Segmented picker, right-aligned on `right` and centered vertically in the row.
fn segmented(gfx: &Gfx, th: &Theme, hits: &mut Hits, right: f32, row_y: f32, items: &[(&str, Ctl, bool)]) {
    let widths: Vec<f32> = items.iter().map(|(t, ..)| gfx.text_width(t, &gfx.fonts.caption) + 18.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 4.0;
    let h = 26.0;
    let top = row_y + (SETTING_ROW - h) / 2.0;
    let container = rect(right - total, top, total, h);
    gfx.fill(container, 6.0, th.track);
    let mut sx = container.left + 2.0;
    for ((text, ctl, selected), sw) in items.iter().zip(&widths) {
        let seg = rect(sx, top + 2.0, *sw, h - 4.0);
        let hovered = hits.add(seg, Hit::Ctl(*ctl));
        let color = if *selected {
            gfx.fill(seg, 4.0, th.accent());
            th.on_accent()
        } else {
            if hovered {
                gfx.fill(seg, 4.0, th.button_hover);
            }
            th.text2
        };
        gfx.text(text, &gfx.fonts.caption, seg, color, DWRITE_TEXT_ALIGNMENT_CENTER);
        sx += sw;
    }
}

/// Windows 11 style toggle, right-aligned.
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

fn button(gfx: &Gfx, th: &Theme, hits: &mut Hits, r: D2D_RECT_F, text: &str, ctl: Ctl, danger: bool) {
    let hovered = hits.add(r, Hit::Ctl(ctl));
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

pub(super) fn apply(ctl: Ctl) {
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
