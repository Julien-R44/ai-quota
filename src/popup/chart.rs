//! Usage chart shown under an expanded limit row.

use windows::Win32::Graphics::Direct2D::Common::{D2D_RECT_F, D2D1_COLOR_F};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
};

use super::gfx::Gfx;
use super::rect;
use super::theme::Theme;
use crate::history::{self, Sample};
use crate::i18n::{self, tr};
use crate::model::Limit;
use crate::timefmt;

const CHART_TOP: f32 = 10.0;
const CHART_PLOT: f32 = 74.0;
pub(super) const CHART_H: f32 = CHART_TOP + CHART_PLOT + 42.0;

/// Current window: recorded usage, steady pace (the diagonal) and the projection at the recent rate up to the
/// reset.
pub(super) fn draw(
    gfx: &Gfx,
    th: &Theme,
    l: &Limit,
    samples: &[Sample],
    brand: D2D1_COLOR_F,
    reveal: f32,
    bounds: D2D_RECT_F,
) {
    let (x, y, w) = (bounds.left, bounds.top, bounds.right - bounds.left);
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

    // Background: 0 / 50 / 100 % gridlines and the steady-pace diagonal (0 % when the window starts, 100 % at
    // its reset).
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

    let clip = gfx.clip(rect(x - 6.0, top - 6.0, (w + 12.0) * reveal, CHART_PLOT + 12.0));
    let projection = history::project(samples, start, reset);
    if let Some(last) = samples.last() {
        // A window starts at 0 %.
        let mut points = vec![(tx(start), py(0.0))];
        points.extend(samples.iter().map(|s| (tx(s.ts), py(s.percent))));
        gfx.area(&points, top, bottom, brand);

        let line = th.level(l.percent, brand);
        for (i, pair) in points.windows(2).enumerate() {
            // Dashed where interpolated: before the first sample, or across a gap (PC off).
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
    drop(clip);

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
