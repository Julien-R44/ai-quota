use std::sync::LazyLock;

use regex_lite::Regex;
use serde_json::Value;

use super::{Parsed, json_response, resolve, unless_cooling_down};
use crate::model::{Amount, Credit, Limit, LimitKind, Problem, Provider, Usage};
use crate::timefmt::{self, now, parse_iso8601};
use crate::{http, secret};

/// Amp access token: `AMP_API_KEY`, otherwise the one pasted in the settings.
fn token() -> Option<String> {
    std::env::var("AMP_API_KEY").ok().filter(|t| !t.trim().is_empty()).or_else(secret::load_amp_token)
}

pub(super) fn fetch(forced: bool) -> Option<Usage> {
    let token = token()?;
    let fetched = unless_cooling_down(Provider::Amp, forced, || {
        let auth = format!("Bearer {token}");
        let headers =
            [("Authorization", auth.as_str()), ("Accept", "application/json"), ("Content-Type", "application/json")];
        let body = br#"{"method":"userDisplayBalanceInfo","params":{}}"#;
        let response = http::post("ampcode.com", "/api/internal?userDisplayBalanceInfo", &headers, body);
        let json = json_response(Provider::Amp, response)?;
        match (json["ok"].as_bool(), json["error"]["code"].as_str()) {
            // Without the display text there is nothing to parse: do not cache it as a success.
            (Some(true), _) if json["result"]["displayText"].is_string() => Ok(json),
            (Some(true), _) => Err(Problem::Unreadable),
            (_, Some("auth-required")) => Err(Problem::SessionExpired(Provider::Amp)),
            _ => Err(json["error"]["message"].as_str().map_or(Problem::Unreadable, |m| Problem::Message(m.into()))),
        }
    });
    let parse = |json: &Value| parse_amp(json["result"]["displayText"].as_str().unwrap_or_default(), now());
    Some(resolve(Provider::Amp, None, fetched, parse))
}

/// Number as displayed by Amp: `1,500`, `18.57`.
macro_rules! num {
    () => {
        r"([0-9][0-9,]*(?:\.[0-9]+)?)"
    };
}

/// Renewal delay: `resets upon renewal in 27 days / 1 month`.
macro_rules! renewal {
    () => {
        r"resets\s+upon\s+renewal\s+in\s+([0-9][0-9,]*)\s+(days?|months?)"
    };
}

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("Amp parser patterns are valid constants")
}

// Same patterns as CodexBar.
static ANSI: LazyLock<Regex> = LazyLock::new(|| re(r"\x1b\[[0-9;]*[A-Za-z]"));
/// Current subscription: Agent usage in $ plus Orb hours.
static TIER: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?im)^\s*Amp\s+([^\r\n]+?)\s+Tier:\s*agent\s+usage\s+\$",
        num!(),
        r"\s+of\s+\$",
        num!(),
        r"\s+remaining\b([^\r\n]*?)",
        renewal!(),
        r"\b"
    ))
});
/// Hours counted in an instance size: a1.small, a1.xxlarge (Gigawatt)...
static ORBS: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?i)\borb\s+usage\s+",
        num!(),
        r"h\s+of\s+",
        num!(),
        r"h\s+(a1\.[a-z0-9]+)\s+orb\s+hours\s+remaining\b"
    ))
});
static PERIOD: LazyLock<Regex> = LazyLock::new(|| re(r"\bperiod\s+(\d{4}-\d{2}-\d{2})\s+to\s+(\d{4}-\d{2}-\d{2})\b"));
/// Legacy subscription, in %, under two possible headings.
static LEGACY: LazyLock<[Regex; 2]> = LazyLock::new(|| {
    macro_rules! legacy {
        ($prefix:literal) => {
            re(concat!(
                $prefix,
                r"\s*",
                num!(),
                r"\s*%\s+other\s+usage\s+and\s+",
                num!(),
                r"\s*%\s+orb\s+usage\s+remaining\s*-\s*",
                renewal!()
            ))
        };
    }
    [legacy!(r"(?im)^\s*Subscription\s+(.+?):"), legacy!(r"(?im)^\s*Amp\s+(.+?)\s+Subscription:")]
});
/// Amp Free in dollars that refill hourly.
static FREE_DOLLARS: LazyLock<Regex> = LazyLock::new(|| {
    re(concat!(
        r"(?im)^\s*Amp Free:\s*\$?",
        num!(),
        r"\s*/\s*\$?",
        num!(),
        r"\s+remaining(?:\s*\(replenishes\s*\+\$?",
        num!(),
        r"\s*/\s*hour\))?"
    ))
});
/// Amp Free as a daily %.
static FREE_PERCENT: LazyLock<Regex> =
    LazyLock::new(|| re(concat!(r"(?im)^\s*Amp Free:\s*", num!(), r"\s*%\s+remaining")));
static INDIVIDUAL: LazyLock<Regex> =
    LazyLock::new(|| re(concat!(r"(?im)^\s*Individual credits:\s*\$?", num!(), r"\s+remaining")));
static WORKSPACE: LazyLock<Regex> =
    LazyLock::new(|| re(concat!(r"(?im)^\s*Workspace\s+(.+?):\s*\$?", num!(), r"\s+remaining")));

fn number(s: &str) -> Option<f64> {
    s.replace(',', "").parse().ok()
}

fn captures(re: &Regex, text: &str) -> Option<Vec<String>> {
    let caps = re.captures(text)?;
    Some(caps.iter().map(|m| m.map_or(String::new(), |m| m.as_str().trim().to_string())).collect())
}

fn renewal(value: &str, unit: &str, now: i64) -> Option<i64> {
    let n: i64 = value.replace(',', "").parse().ok()?;
    let days = if unit.to_lowercase().starts_with("month") { n.checked_mul(30)? } else { n };
    days.checked_mul(86400)?.checked_add(now)
}

/// `period 2026-09-13 to 2026-10-13`: start and end of the billing period (UTC).
fn period(text: &str) -> Option<(i64, i64)> {
    let c = captures(&PERIOD, text)?;
    let day = |d: &str| parse_iso8601(&format!("{d}T00:00:00Z"));
    let (start, end) = (day(&c[1])?, day(&c[2])?);
    (end > start).then_some((start, end))
}

/// Parses the text shown by the API (and by `amp usage`): "Tier" subscription,
/// legacy % format, Amp Free, credits.
fn parse_amp(display: &str, now: i64) -> Parsed {
    let text = ANSI.replace_all(display, "").replace("**", "");
    let mut limits = Vec::new();
    let mut plan = None;
    let used = |left: f64, total: f64| ((total - left) / total * 100.0).clamp(0.0, 100.0) as f32;

    if let Some(c) = captures(&TIER, &text) {
        plan = Some(c[1].clone());
        let period = period(&c[4]);
        let resets_at = period.map(|(_, end)| end).or_else(|| renewal(&c[5], &c[6], now));
        let window_secs = period.map(|(start, end)| end - start);
        if let (Some(left), Some(total)) = (number(&c[2]), number(&c[3]))
            && total > 0.0
        {
            limits.push(Limit {
                key: "amp.agent".into(),
                kind: LimitKind::AmpAgent,
                percent: used(left, total),
                amount: Some(Amount::Dollars { left, total }),
                resets_at,
                window_secs,
            });
        }
        if let Some(o) = captures(&ORBS, &c[4])
            && let (Some(left), Some(total)) = (number(&o[1]), number(&o[2]))
            && total > 0.0
        {
            limits.push(Limit {
                key: "amp.orbs".into(),
                kind: LimitKind::AmpOrbs(o[3].to_lowercase()),
                percent: used(left, total),
                amount: Some(Amount::Hours { left, total }),
                resets_at,
                window_secs,
            });
        }
    } else if let Some(c) = LEGACY.iter().find_map(|re| captures(re, &text)) {
        plan = Some(c[1].clone());
        let resets_at = renewal(&c[4], &c[5], now);
        for (key, kind, remaining) in
            [("amp.agent", LimitKind::AmpAgent, &c[2]), ("amp.orbs", LimitKind::AmpOrbs(String::new()), &c[3])]
        {
            if let Some(left) = number(remaining) {
                let percent = (100.0 - left).clamp(0.0, 100.0) as f32;
                limits.push(Limit { key: key.into(), kind, percent, amount: None, resets_at, window_secs: None });
            }
        }
    }

    if let Some(c) = captures(&FREE_DOLLARS, &text)
        && let (Some(left), Some(total)) = (number(&c[1]), number(&c[2]))
        && total > 0.0
    {
        // Hourly refill: show when the quota will be full again.
        let rate = number(&c[3]).unwrap_or(0.0);
        let resets_at =
            (rate > 0.0 && left < total).then(|| now.saturating_add(((total - left) / rate * 3600.0) as i64));
        limits.push(Limit {
            key: "amp.free".into(),
            kind: LimitKind::AmpFree,
            percent: used(left, total),
            amount: Some(Amount::Dollars { left, total }),
            resets_at,
            window_secs: None,
        });
    } else if let Some(c) = captures(&FREE_PERCENT, &text)
        && let Some(left) = number(&c[1])
    {
        // Resets every day at 8 pm New York time.
        limits.push(Limit {
            key: "amp.free".into(),
            kind: LimitKind::AmpFree,
            percent: (100.0 - left).clamp(0.0, 100.0) as f32,
            amount: None,
            resets_at: Some(timefmt::next_new_york_time(now, 20)),
            window_secs: Some(86400),
        });
    }
    if plan.is_none() && limits.iter().any(|l| l.kind == LimitKind::AmpFree) {
        plan = Some("Free".into());
    }

    let mut credits = Vec::new();
    if let Some(c) = captures(&INDIVIDUAL, &text)
        && let Some(dollars) = number(&c[1])
    {
        credits.push(Credit { workspace: None, dollars });
    }
    for c in WORKSPACE.captures_iter(&text) {
        if let Some(dollars) = c.get(2).and_then(|m| number(m.as_str())) {
            let name = c.get(1).map(|m| m.as_str().trim().to_string());
            credits.push(Credit { workspace: name, dollars });
        }
    }
    Parsed { plan, limits, resets: None, credits }
}

#[cfg(test)]
mod tests {
    use super::{ANSI, FREE_DOLLARS, FREE_PERCENT, INDIVIDUAL, LEGACY, ORBS, PERIOD, TIER, WORKSPACE, parse_amp};
    use crate::model::{Amount, Credit, LimitKind};
    use crate::timefmt::parse_iso8601;
    use std::sync::LazyLock;

    #[test]
    fn patterns_compile() {
        for re in [&ANSI, &TIER, &ORBS, &PERIOD, &FREE_DOLLARS, &FREE_PERCENT, &INDIVIDUAL, &WORKSPACE] {
            LazyLock::force(re);
        }
        LazyLock::force(&LEGACY);
    }

    #[test]
    fn parses_amp_tier_with_orbs() {
        let text = "\x1b[2mSigned in as user@example.com\x1b[0m\n\
            **Amp Megawatt Tier:** agent usage $18.57 of $20 remaining (93%), orb usage 732.8h of 1,500h a1.small orb hours remaining (98%) - period 2026-09-13 to 2026-10-13, resets upon renewal in 27 days\n\
            Individual credits: $20 remaining (replenishes automatically) - https://ampcode.com/settings\n\
            Workspace Test Team: $7.25 remaining";
        let p = parse_amp(text, 0);
        assert_eq!(p.plan.as_deref(), Some("Megawatt"));
        let agent = &p.limits[0];
        assert_eq!(agent.kind, LimitKind::AmpAgent);
        assert_eq!(agent.amount, Some(Amount::Dollars { left: 18.57, total: 20.0 }));
        assert!((agent.percent - 7.15).abs() < 0.01);
        assert_eq!(agent.resets_at, parse_iso8601("2026-10-13T00:00:00Z"));
        assert_eq!(agent.window_secs, Some(30 * 86400));
        let orbs = &p.limits[1];
        assert_eq!(orbs.kind, LimitKind::AmpOrbs("a1.small".into()));
        assert_eq!(orbs.amount, Some(Amount::Hours { left: 732.8, total: 1500.0 }));
        assert_eq!(
            p.credits,
            vec![
                Credit { workspace: None, dollars: 20.0 },
                Credit { workspace: Some("Test Team".into()), dollars: 7.25 },
            ]
        );
    }

    #[test]
    fn parses_real_gigawatt_response() {
        // Real response (Gigawatt plan): Orb hours on a1.xxlarge.
        let text = "Signed in as someone@example.com\n\
            Amp Gigawatt Tier: agent usage $199.92 of $200 remaining (100%), orb usage 991.9h of 1,000h a1.xxlarge orb hours remaining (99%) - period 2026-10-15 to 2026-11-15, resets upon renewal in 1 month\n\
            Individual credits: $10.40 remaining (set up auto-reload to avoid running out) - https://ampcode.com/settings\n\
            Workspace acme: $0.24 remaining (set up auto-reload to avoid running out) - https://ampcode.com/workspaces/acme";
        let p = parse_amp(text, 0);
        assert_eq!(p.plan.as_deref(), Some("Gigawatt"));
        assert_eq!(p.limits.len(), 2);
        let orbs = &p.limits[1];
        assert_eq!(orbs.kind, LimitKind::AmpOrbs("a1.xxlarge".into()));
        assert_eq!(orbs.label(), "Orbs (xxlarge)");
        assert_eq!(orbs.amount, Some(Amount::Hours { left: 991.9, total: 1000.0 }));
        assert_eq!(orbs.resets_at, parse_iso8601("2026-11-15T00:00:00Z"));
        assert_eq!(p.credits.len(), 2);
    }

    #[test]
    fn parses_amp_free_and_legacy_subscription() {
        let free = parse_amp(
            "Amp Free: $4.71/$10 remaining (replenishes +$0.42/hour) - https://ampcode.com/settings#amp-free",
            1000,
        );
        assert_eq!(free.plan.as_deref(), Some("Free"));
        assert_eq!(free.limits[0].amount, Some(Amount::Dollars { left: 4.71, total: 10.0 }));
        // (10 - 4.71) / 0.42 h, about 12.6 h until full
        assert_eq!(free.limits[0].resets_at, Some(1000 + (5.29 / 0.42 * 3600.0) as i64));

        let legacy = parse_amp(
            "Subscription Pro: 62% other usage and 45% orb usage remaining - resets upon renewal in 18 days",
            0,
        );
        assert_eq!(legacy.plan.as_deref(), Some("Pro"));
        assert_eq!(legacy.limits.iter().map(|l| l.percent).collect::<Vec<_>>(), vec![38.0, 55.0]);
        assert_eq!(legacy.limits[1].resets_at, Some(18 * 86400));
    }
}
