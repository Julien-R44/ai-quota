//! Historique des relevés d'usage, pour les courbes et la projection.
//!
//! Les relevés viennent de l'API d'usage du compte : ils incluent donc tout ce
//! qui consomme le quota (CLI local, tâches cloud, web). Stockage : un petit
//! fichier TSV dans `%LOCALAPPDATA%\ai-quota`, réécrit atomiquement.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::model::{Snapshot, Status};

/// On garde un peu plus qu'une fenêtre hebdomadaire.
const RETENTION: i64 = 8 * 86400;
/// Au-delà, deux relevés successifs ne sont pas contigus (PC éteint…).
pub const GAP: i64 = 15 * 60;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    pub ts: i64,
    pub percent: f32,
    /// Relevé arrivé après un trou (PC éteint…) : le segment qui y mène est
    /// une interpolation.
    pub gap: bool,
}

#[derive(Default)]
pub struct History {
    series: HashMap<String, Vec<Sample>>,
}

fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("ai-quota").join("history.tsv"))
}

impl History {
    pub fn load() -> Self {
        let mut history = History::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else { return history };
        for line in text.lines() {
            let mut cols = line.split('\t');
            let (Some(key), Some(ts), Some(percent)) = (cols.next(), cols.next(), cols.next()) else { continue };
            let (Ok(ts), Ok(percent)) = (ts.parse(), percent.parse()) else { continue };
            let gap = cols.next() == Some("1");
            history.series.entry(key.to_string()).or_default().push(Sample { ts, percent, gap });
        }
        for samples in history.series.values_mut() {
            samples.sort_by_key(|s| s.ts);
        }
        history
    }

    fn save(&self) {
        let Some(path) = path() else { return };
        let mut out = String::new();
        for (key, samples) in &self.series {
            for s in samples {
                out.push_str(&format!("{key}\t{}\t{}\t{}\n", s.ts, s.percent, u8::from(s.gap)));
            }
        }
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
        let tmp = path.with_extension("tsv.tmp");
        if std::fs::write(&tmp, out).is_err() || std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Ajoute les relevés frais d'un snapshot (pas les données conservées
    /// après une panne). Un palier stable est compressé en deux points.
    pub fn record(&mut self, snap: &Snapshot) {
        let ts = snap.fetched_at;
        let mut changed = false;
        for u in snap.usages.iter().filter(|u| u.stale_since.is_none()) {
            let Status::Ok(limits) = &u.status else { continue };
            for l in limits {
                let samples = self.series.entry(l.key.clone()).or_default();
                changed |= push_sample(samples, ts, l.percent);
            }
        }
        let cutoff = ts - RETENTION;
        for samples in self.series.values_mut() {
            let before = samples.len();
            samples.retain(|s| s.ts >= cutoff);
            changed |= samples.len() != before;
        }
        self.series.retain(|_, s| !s.is_empty());
        if changed {
            self.save();
        }
    }

    pub fn clear(&mut self) {
        self.series.clear();
        if let Some(path) = path() {
            let _ = std::fs::remove_file(path);
        }
    }

    /// Relevés de la fenêtre en cours (depuis `start`).
    pub fn window(&self, key: &str, start: i64) -> &[Sample] {
        let Some(samples) = self.series.get(key) else { return &[] };
        let first = samples.partition_point(|s| s.ts < start);
        &samples[first..]
    }
}

/// Ajoute un relevé. Sur un palier contigu (même valeur, pas de trou), on
/// déplace le dernier point au lieu d'en ajouter un.
fn push_sample(samples: &mut Vec<Sample>, ts: i64, percent: f32) -> bool {
    match samples.as_mut_slice() {
        [.., b] if b.ts >= ts => return false,
        [.., a, b] if !b.gap && a.percent == percent && b.percent == percent && ts - b.ts <= GAP => b.ts = ts,
        _ => {
            let gap = samples.last().is_some_and(|b| ts - b.ts > GAP);
            samples.push(Sample { ts, percent, gap });
        }
    }
    true
}

/// Valeur interpolée à l'instant `t`. Une fenêtre démarre toujours à 0 %.
fn value_at(samples: &[Sample], start: i64, t: i64) -> f32 {
    let mut prev = Sample { ts: start, percent: 0.0, gap: false };
    for &s in samples {
        if s.ts >= t {
            let span = (s.ts - prev.ts).max(1) as f32;
            return prev.percent + (s.percent - prev.percent) * (t - prev.ts) as f32 / span;
        }
        prev = s;
    }
    prev.percent
}

pub struct Projection {
    /// Usage attendu à la réinitialisation au rythme récent.
    pub at_reset: f32,
    /// Instant où la limite serait atteinte, si c'est avant la réinitialisation.
    pub limit_at: Option<i64>,
}

/// Extrapole le rythme récent (dernier septième de la fenêtre : ~45 min pour
/// une session de 5 h, un jour pour une semaine) jusqu'à la réinitialisation.
pub fn project(samples: &[Sample], start: i64, reset: i64) -> Option<Projection> {
    let last = *samples.last()?;
    let span = ((reset - start) / 7).max(30 * 60);
    let from = (last.ts - span).max(start);
    if last.ts - from < 5 * 60 {
        return None;
    }
    let rate = (last.percent - value_at(samples, start, from)) / (last.ts - from) as f32;
    let at_reset = last.percent + rate * (reset - last.ts) as f32;
    let limit_at = (rate > 0.0 && at_reset >= 100.0 && last.percent < 100.0)
        .then(|| last.ts + ((100.0 - last.percent) / rate).round() as i64);
    Some(Projection { at_reset: at_reset.max(last.percent), limit_at })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compresses_plateaus_but_keeps_gaps() {
        let mut samples = Vec::new();
        let points =
            [(0, 10.0), (300, 10.0), (600, 10.0), (900, 10.0), (5000, 10.0), (5300, 10.0), (5600, 12.0), (5600, 13.0)];
        for (ts, percent) in points {
            push_sample(&mut samples, ts, percent);
        }
        let got: Vec<(i64, bool)> = samples.iter().map(|s| (s.ts, s.gap)).collect();
        // le point d'après le trou n'est jamais déplacé : le trou reste visible
        assert_eq!(got, vec![(0, false), (900, false), (5000, true), (5300, false), (5600, false)]);
    }

    #[test]
    fn projects_recent_rate() {
        // fenêtre de 7 h : rythme récent mesuré sur la dernière heure
        let start = 0;
        let reset = 7 * 3600;
        let s = |ts, percent| Sample { ts, percent, gap: false };
        let samples = [s(3600, 5.0), s(2 * 3600, 10.0), s(3 * 3600, 40.0)];
        let p = project(&samples, start, reset).unwrap();
        // +30 %/h pendant 4 h → limite atteinte 2 h plus tard
        assert_eq!(p.limit_at, Some(5 * 3600));

        let calm = [s(3 * 3600, 6.0)];
        let p = project(&calm, start, reset).unwrap();
        assert!(p.limit_at.is_none());
        assert!((p.at_reset - 14.0).abs() < 0.01);
    }
}
