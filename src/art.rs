//! Icône de l'app dessinée en champ de distance signée (SDF).
//! Partagé entre `build.rs` (génère le .ico de l'exe) et l'app (icône du tray,
//! rendue à la taille exacte du DPI courant, donc toujours nette).

/// Distance d'un point au segment [a, b].
fn seg_dist(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (pax, pay) = (p.0 - a.0, p.1 - a.1);
    let (bax, bay) = (b.0 - a.0, b.1 - a.1);
    let len2 = bax * bax + bay * bay;
    let h = if len2 > 0.0 { ((pax * bax + pay * bay) / len2).clamp(0.0, 1.0) } else { 0.0 };
    ((pax - bax * h).powi(2) + (pay - bay * h).powi(2)).sqrt()
}

pub const NEEDLE_W: f32 = 1.5;
pub const HUB_R: f32 = 1.5;

/// Distance signée à la jauge, dans un repère 16×16 (y vers le bas).
/// Un arc de 240° ouvert en bas, une aiguille et un moyeu.
fn gauge(x: f32, y: f32) -> f32 {
    const CX: f32 = 8.0;
    const CY: f32 = 9.4;
    const R: f32 = 6.0;
    const W: f32 = 1.9;
    let polar = |deg: f32| {
        let r = deg.to_radians();
        (CX + R * r.cos(), CY - R * r.sin())
    };
    let (dx, dy) = (x - CX, CY - y);
    let len = (dx * dx + dy * dy).sqrt();
    let a = dy.atan2(dx).to_degrees();
    let arc = if a > -150.0 && a < -30.0 {
        let (e1, e2) = (polar(-30.0), polar(-150.0));
        seg_dist((x, y), e1, e1).min(seg_dist((x, y), e2, e2))
    } else {
        (len - R).abs()
    } - W / 2.0;
    let tip = {
        let r = 42f32.to_radians();
        (CX + 3.9 * r.cos(), CY - 3.9 * r.sin())
    };
    let needle = seg_dist((x, y), (CX, CY), tip) - NEEDLE_W / 2.0;
    let hub = len - HUB_R;
    arc.min(needle).min(hub)
}

fn coverage(d_px: f32) -> f32 {
    (0.5 - d_px).clamp(0.0, 1.0)
}

/// Glyphe monochrome du tray : couverture alpha (0..1) par pixel, ligne par ligne.
pub fn tray_glyph(size: u32) -> Vec<f32> {
    let s = size as f32 / 16.0;
    let mut out = Vec::with_capacity((size * size) as usize);
    for j in 0..size {
        for i in 0..size {
            let (x, y) = ((i as f32 + 0.5) / s, (j as f32 + 0.5) / s);
            out.push(coverage(gauge(x, y) * s));
        }
    }
    out
}

#[allow(dead_code)] // utilisé par build.rs
/// Icône couleur de l'exe : carré arrondi en dégradé + jauge blanche.
/// Renvoie des pixels RGBA (alpha non prémultiplié), ligne par ligne.
pub fn app_icon(size: u32) -> Vec<[u8; 4]> {
    const FROM: [f32; 3] = [255.0, 138.0, 91.0];
    const TO: [f32; 3] = [123.0, 97.0, 255.0];
    let n = size as f32;
    let radius = n * 0.24;
    let inset = n * 0.17;
    let gs = (n - 2.0 * inset) / 16.0;
    let mut out = Vec::with_capacity((size * size) as usize);
    for j in 0..size {
        for i in 0..size {
            let (px, py) = (i as f32 + 0.5, j as f32 + 0.5);
            // carré arrondi
            let qx = (px - n / 2.0).abs() - (n / 2.0 - radius);
            let qy = (py - n / 2.0).abs() - (n / 2.0 - radius);
            let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
            let bg = coverage(outside + qx.max(qy).min(0.0) - radius);
            let fg = coverage(gauge((px - inset) / gs, (py - inset) / gs) * gs);
            let t = ((px + py) / (2.0 * n)).clamp(0.0, 1.0);
            let mut rgb = [0u8; 3];
            for c in 0..3 {
                let base = FROM[c] + (TO[c] - FROM[c]) * t;
                rgb[c] = (base + (255.0 - base) * fg).round() as u8;
            }
            out.push([rgb[0], rgb[1], rgb[2], (bg * 255.0).round() as u8]);
        }
    }
    out
}
