use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::Time::{
    DYNAMIC_TIME_ZONE_INFORMATION, EnumDynamicTimeZoneInformation, SystemTimeToTzSpecificLocalTime,
    SystemTimeToTzSpecificLocalTimeEx, TzSpecificLocalTimeToSystemTimeEx,
};

use crate::i18n::{Lang, lang, tr};

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
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

/// `2026-10-16T02:19:59.522843+00:00` or `...Z` → Unix timestamp.
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

/// `2026-10-16T02:19:59Z`
pub fn utc_iso8601(ts: i64) -> String {
    let t = system_time(ts);
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn system_time(ts: i64) -> SYSTEMTIME {
    let (y, m, d) = civil_from_days(ts.div_euclid(86400));
    let secs = ts.rem_euclid(86400);
    SYSTEMTIME {
        wYear: y as u16,
        wMonth: m as u16,
        wDay: d as u16,
        wHour: (secs / 3600) as u16,
        wMinute: (secs % 3600 / 60) as u16,
        wSecond: (secs % 60) as u16,
        ..Default::default()
    }
}

fn unix_time(t: &SYSTEMTIME) -> i64 {
    days_from_civil(i64::from(t.wYear), i64::from(t.wMonth), i64::from(t.wDay)) * 86400
        + i64::from(t.wHour) * 3600
        + i64::from(t.wMinute) * 60
        + i64::from(t.wSecond)
}

/// Used if the zone is missing from the registry: standard time, UTC-5.
const NEW_YORK_FALLBACK: i64 = -5 * 3600;

/// Windows' own "Eastern Standard Time" zone, DST rules included. Looked up once.
fn eastern_zone() -> Option<&'static DYNAMIC_TIME_ZONE_INFORMATION> {
    static ZONE: OnceLock<Option<DYNAMIC_TIME_ZONE_INFORMATION>> = OnceLock::new();
    ZONE.get_or_init(|| {
        (0..)
            .map_while(|i| {
                let mut zone = DYNAMIC_TIME_ZONE_INFORMATION::default();
                // SAFETY: zone is a writable, initialized output struct; the API validates i.
                (unsafe { EnumDynamicTimeZoneInformation(i, &raw mut zone) } == 0).then_some(zone)
            })
            .find(|z| {
                z.TimeZoneKeyName.iter().take_while(|&&c| c != 0).copied().eq("Eastern Standard Time".encode_utf16())
            })
    })
    .as_ref()
}

/// Next `hour`:00 in New York (Amp Free's daily reset).
pub fn next_new_york_time(now: i64, hour: i64) -> i64 {
    let zone = eastern_zone();
    let to_local = |ts: i64| {
        let mut local = SYSTEMTIME::default();
        if let Some(z) = zone {
            // SAFETY: z comes from successful zone enumeration; both SYSTEMTIME allocations
            // stay live for this synchronous call. Unsupported dates return an error.
            if unsafe { SystemTimeToTzSpecificLocalTimeEx(Some(z), &system_time(ts), &raw mut local) }.is_ok() {
                return unix_time(&local);
            }
        }
        ts.saturating_add(NEW_YORK_FALLBACK)
    };
    let to_utc = |local: i64| {
        let mut utc = SYSTEMTIME::default();
        if let Some(z) = zone {
            // SAFETY: z comes from successful zone enumeration; both SYSTEMTIME allocations
            // stay live for this synchronous call. Unsupported dates return an error.
            if unsafe { TzSpecificLocalTimeToSystemTimeEx(Some(z), &system_time(local), &raw mut utc) }.is_ok() {
                return unix_time(&utc);
            }
        }
        local.saturating_sub(NEW_YORK_FALLBACK)
    };
    // Each candidate day is converted on its own: tomorrow may be across a DST change.
    let today = to_local(now).div_euclid(86400);
    (today..today + 3).map(|day| to_utc(day * 86400 + hour * 3600)).find(|&t| t > now).unwrap_or(now + 86400)
}

struct LocalTime {
    weekday: usize,
    day: u16,
    month: u16,
    hour: u16,
    minute: u16,
}

fn local_time(ts: i64) -> Option<LocalTime> {
    let mut local = SYSTEMTIME::default();
    // SAFETY: Input and output are distinct, initialized SYSTEMTIME allocations. A null zone
    // selects the current system zone; invalid dates are reported as an error.
    unsafe { SystemTimeToTzSpecificLocalTime(None, &system_time(ts), &raw mut local).ok()? };
    let days = days_from_civil(i64::from(local.wYear), i64::from(local.wMonth), i64::from(local.wDay));
    Some(LocalTime {
        // 1970-01-01 was a Thursday
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

/// "40 min", "3 h 20" / "3h 20m".
fn duration(secs: i64) -> String {
    match (lang(), secs) {
        (_, ..3600) => format!("{}\u{a0}min", secs / 60),
        (Lang::Fr, _) => format!("{}\u{a0}h\u{a0}{:02}", secs / 3600, secs % 3600 / 60),
        (Lang::En, _) => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

/// Axis tick: "14:30", or "jeu. 22" for multi-day windows.
pub fn axis_label(ts: i64, with_day: bool) -> String {
    let Some(t) = local_time(ts + 30) else { return String::new() };
    if with_day { format!("{} {}", WEEKDAYS_SHORT[l()][t.weekday], t.day) } else { hm(&t) }
}

/// "jeudi 22 oct." / "Thursday, Oct 22" (short: "jeu. 22 oct." / "Thu, Oct 22"): a bare weekday
/// is ambiguous as soon as the window spans a week.
fn day_date(t: &LocalTime, short: bool) -> String {
    let day = if short { WEEKDAYS_SHORT[l()][t.weekday] } else { WEEKDAYS[l()][t.weekday] };
    let month = MONTHS[l()][(t.month as usize).clamp(1, 12) - 1];
    match lang() {
        Lang::Fr => format!("{day} {}\u{a0}{month}", t.day),
        Lang::En => format!("{day}, {month} {}", t.day),
    }
}

/// "Renouvellement samedi 31 oct.": date only. Amp only gives a day (in UTC), so it is read in
/// UTC, without conversion.
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

/// "jeu. 22 oct."
pub fn date_text(ts: i64) -> String {
    local_time(ts + 30).map(|t| day_date(&t, true)).unwrap_or_default()
}

/// "Réinitialisation dans 2 h 13" / "Réinitialisation jeudi 22 oct. à 10:54"
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
    // The API returns 11:59:59.5: round to the nearest minute.
    let Some(t) = local_time(resets_at + 30) else { return String::new() };
    if fr {
        format!("Réinitialisation {} à {}", day_date(&t, false), hm(&t))
    } else {
        format!("Resets {} at {}", day_date(&t, false), hm(&t))
    }
}

/// Future deadline: "dans 40 min", "dans 3 h 20", "lun. 19 oct. vers 04:00"
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

pub fn ago_text(ts: i64, now: i64) -> String {
    format!("{} {}", tr("Mis à jour", "Updated"), age_text(ts, now))
}

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
    use super::{
        age_text, civil_from_days, days_from_civil, next_new_york_time, parse_iso8601, renewal_text, reset_text,
        utc_iso8601,
    };
    use crate::i18n::{Lang, LangGuard};

    #[test]
    fn parses_iso() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("2026-10-16T02:19:59.522843+00:00"), Some(1792117199));
        assert_eq!(parse_iso8601("2026-10-16T04:19:59+02:00"), Some(1792117199));
        assert_eq!(utc_iso8601(1792117199), "2026-10-16T02:19:59Z");
    }

    #[test]
    fn new_york_evening() {
        // Oct 16, 2026 12:00 UTC = 8 am in New York (DST) → 8 pm = Oct 17 00:00 UTC
        let noon = parse_iso8601("2026-10-16T12:00:00Z").unwrap();
        assert_eq!(next_new_york_time(noon, 20), parse_iso8601("2026-10-17T00:00:00Z").unwrap());
        // winter: 8 pm in New York = 01:00 UTC
        let winter = parse_iso8601("2026-12-01T12:00:00Z").unwrap();
        assert_eq!(next_new_york_time(winter, 20), parse_iso8601("2026-12-02T01:00:00Z").unwrap());
        // already past 8 pm, and DST ends overnight (Nov 1, 2026)
        let late = parse_iso8601("2026-11-01T01:30:00Z").unwrap();
        assert_eq!(next_new_york_time(late, 20), parse_iso8601("2026-11-02T01:00:00Z").unwrap());
    }

    #[test]
    fn civil_roundtrip() {
        for d in [-1000, 0, 365, 20000, 20742] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
    }

    #[test]
    fn formats_in_both_languages() {
        let lang = LangGuard::new(Lang::Fr);
        let renewal = parse_iso8601("2026-10-31T00:00:00Z").unwrap();
        assert_eq!(renewal_text(renewal), "Renouvellement samedi 31\u{a0}oct.");
        assert_eq!(reset_text(8000, 0), "Réinitialisation dans 2\u{a0}h\u{a0}13");
        assert_eq!(age_text(0, 180), "il y a 3\u{a0}min");
        lang.set(Lang::En);
        assert_eq!(renewal_text(renewal), "Renews Saturday, Oct 31");
        assert_eq!(reset_text(8000, 0), "Resets in 2h 13m");
        assert_eq!(age_text(0, 180), "3 min ago");
    }
}
