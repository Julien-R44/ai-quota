//! Main view: header, then one card per provider with its limit rows.

use std::fmt::Write as _;

use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
};
use windows_numerics::{Matrix3x2, Vector2};

use super::chart::{self, CHART_H};
use super::gfx::Gfx;
use super::theme::Theme;
use super::{Anim, HEADER_H, Hit, Hits, MARGIN, WIDTH, ease_out, rect};
use crate::i18n::{self, Lang, tr};
use crate::model::{BankedResets, Credit, Limit, LimitKind, Snapshot, Status, Usage, dollars};
use crate::{app, timefmt};

const CARD_GAP: f32 = 8.0;
const CARD_PAD: f32 = 14.0;
const CARD_HEAD: f32 = 22.0;
const HEAD_GAP: f32 = 14.0;
const ROW_H: f32 = 52.0;
const ROW_GAP: f32 = 12.0;
const MESSAGE_H: f32 = 38.0;
/// One extra line at the bottom of a card (banked resets, credits).
const LINE_H: f32 = 26.0;

pub(super) struct MainLayout<'a> {
    content: Content<'a>,
    pub(super) height: f32,
}

enum Content<'a> {
    /// No snapshot yet.
    Loading(D2D_RECT_F),
    /// Snapshot without any provider.
    Empty(D2D_RECT_F),
    Cards(Vec<Card<'a>>),
}

struct Card<'a> {
    usage: &'a Usage,
    rect: D2D_RECT_F,
    body: CardBody<'a>,
}

enum CardBody<'a> {
    Limits { rows: Vec<Row<'a>>, resets: Option<(&'a BankedResets, f32)>, credits: Option<f32> },
    Message { text: String, is_error: bool, top: f32 },
}

struct Row<'a> {
    limit: &'a Limit,
    /// Position across all cards: staggers the bar animation.
    index: usize,
    top: f32,
    /// The chart is shown under the row.
    expanded: bool,
}

impl Row<'_> {
    fn height(&self) -> f32 {
        ROW_H + if self.expanded { CHART_H } else { 0.0 }
    }
}

/// Horizontal extent of a card's content.
fn inner(card: &D2D_RECT_F) -> (f32, f32) {
    (card.left + CARD_PAD, card.right - card.left - 2.0 * CARD_PAD)
}

pub(super) fn layout<'a>(snap: Option<&'a Snapshot>, expanded: Option<&str>) -> MainLayout<'a> {
    let (x, w) = (MARGIN, WIDTH - 2.0 * MARGIN);
    let placeholder = rect(x, HEADER_H, w, 2.0 * CARD_PAD + MESSAGE_H);
    let content = match snap {
        None => Content::Loading(placeholder),
        Some(s) if s.usages.is_empty() => Content::Empty(placeholder),
        Some(s) => {
            let (mut y, mut index) = (HEADER_H, 0);
            let cards = s
                .usages
                .iter()
                .map(|u| {
                    let card = card_layout(u, expanded, rect(x, y, w, 0.0), &mut index);
                    y = card.rect.bottom + CARD_GAP;
                    card
                })
                .collect();
            Content::Cards(cards)
        }
    };
    let bottom = match &content {
        Content::Loading(r) | Content::Empty(r) => r.bottom,
        Content::Cards(cards) => cards.last().map_or(HEADER_H, |c| c.rect.bottom),
    };
    MainLayout { content, height: bottom + MARGIN }
}

/// Lays out one card from its top edge (`frame.top`); `frame.bottom` is computed.
fn card_layout<'a>(u: &'a Usage, expanded: Option<&str>, mut frame: D2D_RECT_F, index: &mut usize) -> Card<'a> {
    let mut y = frame.top + CARD_PAD + CARD_HEAD + HEAD_GAP;
    let body = match &u.status {
        Status::Ok(limits) if !limits.is_empty() => {
            let rows: Vec<Row> = limits
                .iter()
                .map(|limit| {
                    let row = Row { limit, index: *index, top: y, expanded: expanded == Some(limit.key.as_str()) };
                    *index += 1;
                    y += row.height() + ROW_GAP;
                    row
                })
                .collect();
            y -= ROW_GAP;
            let mut line = || {
                y += LINE_H;
                y - LINE_H + 8.0
            };
            let resets = u.resets.as_ref().map(|r| (r, line()));
            let credits = (!u.credits.is_empty()).then(line);
            CardBody::Limits { rows, resets, credits }
        }
        status => {
            let (text, is_error) = match status {
                Status::Error(problem) => (problem.text(), true),
                Status::Ok(_) => {
                    (tr("Aucune limite active pour ce compte.", "No active limit on this account.").into(), false)
                }
            };
            let top = y;
            y += MESSAGE_H;
            CardBody::Message { text, is_error, top }
        }
    };
    frame.bottom = y + CARD_PAD;
    Card { usage: u, rect: frame, body }
}

/// Returns whether every animation of the view has finished.
pub(super) fn draw(
    gfx: &Gfx,
    th: &Theme,
    hits: &mut Hits,
    layout: &MainLayout,
    snap: Option<&Snapshot>,
    anim: &Anim,
) -> bool {
    draw_header(gfx, th, hits, snap, anim);
    match &layout.content {
        Content::Loading(r) => {
            gfx.text(tr("Chargement…", "Loading…"), &gfx.fonts.label, *r, th.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
            true
        }
        Content::Empty(card) => {
            gfx.fill(*card, 8.0, th.card);
            gfx.stroke(*card, 8.0, th.card_border);
            let msg = tr(
                "Aucun outil suivi. Connecte-toi à Claude Code ou à Codex, ou active-les dans les réglages.",
                "Nothing tracked. Sign in to Claude Code or Codex, or enable them in Settings.",
            );
            let (x, w) = inner(card);
            draw_message(gfx, th, msg, false, x, card.top + CARD_PAD, w);
            true
        }
        Content::Cards(cards) => {
            let mut done = true;
            for card in cards {
                done &= draw_card(gfx, th, hits, card, anim);
            }
            done
        }
    }
}

fn draw_header(gfx: &Gfx, th: &Theme, hits: &mut Hits, snap: Option<&Snapshot>, anim: &Anim) {
    gfx.gauge(18.0, 19.0, 18.0);
    gfx.text("AI Quota", &gfx.fonts.title, rect(44.0, 12.0, 120.0, 32.0), th.text1, DWRITE_TEXT_ALIGNMENT_LEADING);
    let btn = rect(WIDTH - MARGIN - 32.0, 12.0, 32.0, 32.0);
    if hits.add(btn, Hit::Refresh) {
        gfx.fill(btn, 6.0, th.button_hover);
    }
    let (cx, cy) = (f32::midpoint(btn.left, btn.right), f32::midpoint(btn.top, btn.bottom) + anim.dy);
    if anim.loading {
        gfx.transform(anim.base * Matrix3x2::rotation_around(anim.t * 400.0, Vector2 { X: cx, Y: cy }));
    }
    gfx.text("\u{E72C}", &gfx.fonts.icon, btn, th.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
    gfx.transform(anim.base);
    let gear = rect(btn.left - 34.0, 12.0, 32.0, 32.0);
    if hits.add(gear, Hit::Gear) {
        gfx.fill(gear, 6.0, th.button_hover);
    }
    gfx.text("\u{E713}", &gfx.fonts.icon, gear, th.text2, DWRITE_TEXT_ALIGNMENT_CENTER);
    let status = match (snap, anim.loading) {
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
}

/// Returns whether the card's animations have finished.
fn draw_card(gfx: &Gfx, th: &Theme, hits: &mut Hits, card: &Card, anim: &Anim) -> bool {
    let (u, r) = (card.usage, card.rect);
    gfx.fill(r, 8.0, th.card);
    gfx.stroke(r, 8.0, th.card_border);
    let (x, w) = inner(&r);
    let open = rect(x + w - 22.0, r.top + CARD_PAD - 3.0, 28.0, CARD_HEAD + 6.0);
    let open_hovered = hits.add(open, Hit::Open(u.provider));
    draw_card_header(gfx, th, u, x, r.top + CARD_PAD, w, open_hovered);

    let mut done = true;
    match &card.body {
        CardBody::Limits { rows, resets, credits } => {
            let (brand, _) = th.brand(u.provider);
            for row in rows {
                let start = 0.08 + row.index as f32 * 0.05;
                let progress = ease_out((anim.t - start) / 0.7, 4);
                done &= progress >= 1.0;
                let area = rect(x - 6.0, row.top - 5.0, w + 12.0, row.height() + 8.0);
                if hits.add_row(area, &row.limit.key) {
                    gfx.fill(area, 6.0, th.button_hover);
                }
                draw_row(gfx, th, row, brand, progress, x, w);
                if row.expanded {
                    let l = row.limit;
                    let window = l.resets_at.zip(l.window_secs).map_or(0, |(r, w)| r - w);
                    let history = app().history.borrow();
                    let samples = history.window(&l.key, window);
                    chart::draw(gfx, th, l, samples, brand, anim.reveal, rect(x, row.top + ROW_H, w, CHART_H));
                    done &= anim.reveal >= 1.0;
                }
            }
            if let Some((resets, y)) = resets {
                draw_resets(gfx, th, resets, brand, (x, *y), w);
            }
            if let Some(y) = credits {
                draw_credits(gfx, th, &u.credits, brand, (x, *y), w);
            }
        }
        CardBody::Message { text, is_error, top } => draw_message(gfx, th, text, *is_error, x, *top, w),
    }
    done
}

fn draw_card_header(gfx: &Gfx, th: &Theme, u: &Usage, x: f32, y: f32, w: f32, open_hovered: bool) {
    let (brand, pill_text) = th.brand(u.provider);
    gfx.provider_logo(th, u.provider, x, y + 1.0);
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
    // Data carried over from an earlier fetch (network error, rate limit...).
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

fn draw_row(gfx: &Gfx, th: &Theme, row: &Row, brand: D2D1_COLOR_F, progress: f32, x: f32, w: f32) {
    let (l, y) = (row.limit, row.top);
    let value = i18n::percent(l.percent);
    let label = l.label();
    gfx.text(&label, &gfx.fonts.label, rect(x, y, w - 76.0, 18.0), th.text1, DWRITE_TEXT_ALIGNMENT_LEADING);
    // Remaining dollars or hours (Amp), after the label.
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
    let chevron = if row.expanded { "\u{E70E}" } else { "\u{E70D}" };
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
    // Pace marker: where usage would be if consumed evenly over the window.
    if let Some(pace) = l.pace(now) {
        let px = (x + w * pace).round();
        gfx.fill(rect(px - 1.0, y + 22.0, 2.0, 12.0), 1.0, th.pace);
    }

    if let Some(at) = l.resets_at {
        // Amp subscriptions renew on a date, not at a time of day.
        let text = match l.kind {
            LimitKind::AmpAgent | LimitKind::AmpOrbs(_) => timefmt::renewal_text(at),
            _ => timefmt::reset_text(at, now),
        };
        gfx.text(&text, &gfx.fonts.caption, rect(x, y + 36.0, w, 16.0), th.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
    }
}

fn draw_resets(gfx: &Gfx, th: &Theme, r: &BankedResets, brand: D2D1_COLOR_F, (x, y): (f32, f32), w: f32) {
    let h = LINE_H - 8.0;
    gfx.text("\u{E777}", &gfx.fonts.icon_small, rect(x, y, 14.0, h), brand, DWRITE_TEXT_ALIGNMENT_LEADING);
    let mut text = match (i18n::lang(), r.count) {
        (Lang::Fr, 1) => "1 reset gratuit en réserve".to_string(),
        (Lang::Fr, n) => format!("{n} resets gratuits en réserve"),
        (Lang::En, 1) => "1 free reset available".to_string(),
        (Lang::En, n) => format!("{n} free resets available"),
    };
    if let Some(at) = r.next_expiry {
        let _ = write!(text, " · {} {}", tr("expire", "expires"), timefmt::date_text(at));
    }
    gfx.text(&text, &gfx.fonts.caption, rect(x + 20.0, y, w - 20.0, h), th.text2, DWRITE_TEXT_ALIGNMENT_LEADING);
}

/// "Credits: $20 · Test Team: $7.25"
fn draw_credits(gfx: &Gfx, th: &Theme, credits: &[Credit], brand: D2D1_COLOR_F, (x, y): (f32, f32), w: f32) {
    let h = LINE_H - 8.0;
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

fn draw_message(gfx: &Gfx, th: &Theme, msg: &str, is_error: bool, x: f32, y: f32, w: f32) {
    let (tx, color) = if is_error {
        gfx.text("\u{E7BA}", &gfx.fonts.icon, rect(x, y, 16.0, 20.0), th.warn, DWRITE_TEXT_ALIGNMENT_LEADING);
        (x + 24.0, th.text1)
    } else {
        (x, th.text2)
    };
    gfx.text(msg, &gfx.fonts.message, rect(tx, y + 1.0, w - (tx - x), MESSAGE_H), color, DWRITE_TEXT_ALIGNMENT_LEADING);
}
