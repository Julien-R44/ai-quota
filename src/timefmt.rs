use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::Time::SystemTimeToTzSpecificLocalTime;

use crate::i18n::{Lang, lang, tr};

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Jours depuis 1970-01-01 (algorithme de Howard Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

/// `2026-10-16T02:19:59.522843+00:00` ou `...Z` → timestamp Unix.
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    if b.len() < 19 {
        return None;
    }
    let days = days_from_civil(num(0..4)?, num(5..7)?, num(8..10)?);
    let mut t = days * 86400 + num(11..13)? * 3600 + num(14..16)? * 60 + num(17..19)?;
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
    }
    if let Some(&sign @ (b'+' | b'-')) = b.get(i) {
        let off = num(i + 1..i + 3)? * 3600 + num(i + 4..i + 6)? * 60;
        t -= if sign == b'+' { off } else { -off };
    }
    Some(t)
}

/// Décalage de New York : UTC−4 en heure d'été, UTC−5 sinon (règles US
/// depuis 2007 : du 2e dimanche de mars au 1er dimanche de novembre, à 2 h).
fn new_york_offset(ts: i64) -> i64 {
    let (year, _, _) = civil_from_days(ts.div_euclid(86400));
    let nth_sunday = |month: i64, n: i64| {
        let first = days_from_civil(year, month, 1);
        let monday_based = (first + 3).rem_euclid(7);
        first + (6 - monday_based).rem_euclid(7) + 7 * (n - 1)
    };
    let summer_start = nth_sunday(3, 2) * 86400 + 7 * 3600;
    let summer_end = nth_sunday(11, 1) * 86400 + 6 * 3600;
    if (summer_start..summer_end).contains(&ts) { -4 * 3600 } else { -5 * 3600 }
}

/// Prochain passage à `hour` h, heure de New York (reset quotidien d'Amp Free).
pub fn next_new_york_time(now: i64, hour: i64) -> i64 {
    let offset = new_york_offset(now);
    let mut t = (now + offset).div_euclid(86400) * 86400 + hour * 3600 - offset;
    if t <= now {
        t += 86400;
    }
    // le jour visé peut être de l'autre côté d'un changement d'heure
    t + offset - new_york_offset(t)
}

struct LocalTime {
    weekday: usize,
    day: u16,
    month: u16,
    hour: u16,
    minute: u16,
}

fn local_time(ts: i64) -> Option<LocalTime> {
    let (y, m, d) = civil_from_days(ts.div_euclid(86400));
    let secs = ts.rem_euclid(86400);
    let utc = SYSTEMTIME {
        wYear: y as u16,
        wMonth: m as u16,
        wDay: d as u16,
        wHour: (secs / 3600) as u16,
        wMinute: (secs % 3600 / 60) as u16,
        wSecond: (secs % 60) as u16,
        ..Default::default()
    };
    let mut local = SYSTEMTIME::default();
    unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).ok()? };
    let days = days_from_civil(local.wYear as i64, local.wMonth as i64, local.wDay as i64);
    Some(LocalTime {
        // 1970-01-01 était un jeudi
        weekday: (days + 3).rem_euclid(7) as usize,
        day: local.wDay,
        month: local.wMonth,
        hour: local.wHour,
        minute: local.wMinute,
    })
}

const WEEKDAYS: [[&str; 7]; 2] = [
    ["lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi", "dimanche"],
    ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"],
];
const WEEKDAYS_SHORT: [[&str; 7]; 2] =
    [["lun.", "mar.", "mer.", "jeu.", "ven.", "sam.", "dim."], ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]];
const MONTHS: [[&str; 12]; 2] = [
    ["janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.", "déc."],
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"],
];

fn l() -> usize {
    match lang() {
        Lang::Fr => 0,
        Lang::En => 1,
    }
}

fn hm(t: &LocalTime) -> String {
    format!("{:02}:{:02}", t.hour, t.minute)
}

/// Durée : « 40 min », « 3 h 20 » / « 3h 20m ».
fn duration(secs: i64) -> String {
    match (lang(), secs) {
        (_, ..3600) => format!("{}\u{a0}min", secs / 60),
        (Lang::Fr, _) => format!("{}\u{a0}h\u{a0}{:02}", secs / 3600, secs % 3600 / 60),
        (Lang::En, _) => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// Repère d'axe : « 14:30 », ou « jeu. 22 » pour les fenêtres de plusieurs jours.
pub fn axis_label(ts: i64, with_day: bool) -> String {
    let Some(t) = local_time(ts + 30) else { return String::new() };
    if with_day { format!("{} {}", WEEKDAYS_SHORT[l()][t.weekday], t.day) } else { hm(&t) }
}

/// « jeudi 22 oct. » / « Thursday, Oct 22 » (abrégé : « jeu. 22 oct. » /
/// « Thu, Oct 22 ») : le jour seul est ambigu dès qu'on parle de la semaine.
fn day_date(t: &LocalTime, short: bool) -> String {
    let day = if short { WEEKDAYS_SHORT[l()][t.weekday] } else { WEEKDAYS[l()][t.weekday] };
    let month = MONTHS[l()][(t.month as usize).clamp(1, 12) - 1];
    match lang() {
        Lang::Fr => format!("{day} {}\u{a0}{month}", t.day),
        Lang::En => format!("{day}, {month} {}", t.day),
    }
}

/// « Renouvellement samedi 31 oct. » : date seule, sans heure. Amp ne
/// fournit qu'un jour (en UTC) ; on le lit donc en UTC, sans conversion.
pub fn renewal_text(ts: i64) -> String {
    let days = ts.div_euclid(86400);
    let (_, month, day) = civil_from_days(days);
    let t = LocalTime {
        weekday: (days + 3).rem_euclid(7) as usize,
        day: day as u16,
        month: month as u16,
        hour: 0,
        minute: 0,
    };
    format!("{} {}", tr("Renouvellement", "Renews"), day_date(&t, false))
}

/// « jeu. 22 oct. »
pub fn date_text(ts: i64) -> String {
    local_time(ts + 30).map(|t| day_date(&t, true)).unwrap_or_default()
}

/// « Réinitialisation dans 2 h 13 » / « Réinitialisation jeudi 22 oct. à 10:54 »
pub fn reset_text(resets_at: i64, now: i64) -> String {
    let diff = resets_at - now;
    let fr = lang() == Lang::Fr;
    if diff <= 0 {
        return tr("Réinitialisation imminente", "Resetting now").into();
    }
    if diff < 60 {
        return tr("Réinitialisation dans moins d’une minute", "Resets in under a minute").into();
    }
    if diff < 86400 {
        return format!("{} {}", tr("Réinitialisation dans", "Resets in"), duration(diff));
    }
    // l'API renvoie 11:59:59.5 : on arrondit à la minute la plus proche
    let Some(t) = local_time(resets_at + 30) else { return String::new() };
    if fr {
        format!("Réinitialisation {} à {}", day_date(&t, false), hm(&t))
    } else {
        format!("Resets {} at {}", day_date(&t, false), hm(&t))
    }
}

/// Échéance future : « dans 40 min », « dans 3 h 20 », « lun. 19 oct. vers 04:00 »
pub fn eta_text(ts: i64, now: i64) -> String {
    let diff = ts - now;
    match diff {
        ..60 => tr("d’un instant à l’autre", "any moment now").into(),
        60..86400 => format!("{} {}", tr("dans", "in"), duration(diff)),
        _ => match local_time(ts + 30) {
            Some(t) => format!("{} {} {}", day_date(&t, true), tr("vers", "around"), hm(&t)),
            None => String::new(),
        },
    }
}

/// « Mis à jour il y a 3 min »
pub fn ago_text(ts: i64, now: i64) -> String {
    format!("{} {}", tr("Mis à jour", "Updated"), age_text(ts, now))
}

/// « à l’instant » / « il y a 3 min »
pub fn age_text(ts: i64, now: i64) -> String {
    let diff = (now - ts).max(0);
    match (lang(), diff) {
        (Lang::Fr, 0..60) => "à l’instant".into(),
        (Lang::Fr, 60..3600) => format!("il y a {}\u{a0}min", diff / 60),
        (Lang::Fr, _) => format!("il y a {}\u{a0}h", diff / 3600),
        (Lang::En, 0..60) => "just now".into(),
        (Lang::En, 60..3600) => format!("{} min ago", diff / 60),
        (Lang::En, _) => format!("{}h ago", diff / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("2026-10-16T02:19:59.522843+00:00"), Some(1792117199));
        assert_eq!(parse_iso8601("2026-10-16T04:19:59+02:00"), Some(1792117199));
    }

    #[test]
    fn new_york_evening() {
        // 16 oct. 2026 12:00 UTC = 8 h à New York (heure d'été) → 20 h = 17 oct. 00:00 UTC
        let noon = parse_iso8601("2026-10-16T12:00:00Z").unwrap();
        assert_eq!(next_new_york_time(noon, 20), parse_iso8601("2026-10-17T00:00:00Z").unwrap());
        // en hiver : 20 h à New York = 01:00 UTC
        let winter = parse_iso8601("2026-12-01T12:00:00Z").unwrap();
        assert_eq!(next_new_york_time(winter, 20), parse_iso8601("2026-12-02T01:00:00Z").unwrap());
    }

    #[test]
    fn civil_roundtrip() {
        for d in [-1000, 0, 365, 20000, 20742] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
    }
}
