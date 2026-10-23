use serde_json::Value;

use super::{Ctx, Parsed, fetch_json, read_json, resolve, unless_cooling_down};
use crate::model::{Limit, LimitKey, LimitKind, Problem, Provider, Usage};
use crate::timefmt::{now, parse_iso8601};

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;
const ROLLING: i64 = 5 * HOUR;
const WEEKLY: i64 = 7 * DAY;
const MONTHLY: i64 = 30 * DAY;

/// The Go subscription's three windows, in the order the API names them.
const WINDOWS: [(&str, i64, LimitKind); 3] = [
    ("rolling", ROLLING, LimitKind::Session { hours: 5 }),
    ("weekly", WEEKLY, LimitKind::Weekly),
    ("monthly", MONTHLY, LimitKind::Monthly),
];

pub(super) fn fetch(ctx: &Ctx, forced: bool) -> Option<Usage> {
    let token = ctx.opencode_token.clone().or_else(|| auth_key(ctx))?;
    let fetched = unless_cooling_down(ctx, Provider::Opencode, forced, || {
        let bearer = format!("Bearer {token}");
        let headers = [("Authorization", bearer.as_str()), ("Accept", "application/json")];
        let json = fetch_json(ctx, Provider::Opencode, "opencode.ai", "/zen/go/v1/usage", &headers)?;
        // Without the rolling window this is not a usage payload: do not cache it as a success.
        if json["usage"]["rolling"].is_object() { Ok(json) } else { Err(Problem::Unreadable) }
    });
    Some(resolve(ctx, Provider::Opencode, None, fetched, parse_opencode))
}

/// API key stored by `opencode auth login`. Recent versions keep credentials in their database,
/// so this only covers the older `auth.json`.
fn auth_key(ctx: &Ctx) -> Option<String> {
    let auth = read_json(&ctx.opencode_dir.join("auth.json"))?;
    ["opencode", "opencode-go"].iter().find_map(|id| auth.get(*id)?["key"].as_str()).map(str::to_string)
}

fn parse_opencode(json: &Value) -> Parsed {
    let usage = &json["usage"];
    let limits = WINDOWS
        .iter()
        .filter_map(|(name, seconds, kind)| {
            let window = usage.get(*name)?;
            let percent = window["percent"].as_f64()?.clamp(0.0, 100.0) as f32;
            let resets_at = window["resetsAt"]
                .as_str()
                .and_then(parse_iso8601)
                .or_else(|| window["resetInSec"].as_i64().map(|s| now().saturating_add(s)));
            Some(Limit {
                key: LimitKey::OpencodeWindow { seconds: *seconds },
                kind: kind.clone(),
                percent,
                amount: None,
                resets_at,
                window_secs: Some(*seconds),
            })
        })
        .collect();
    Parsed { plan: None, limits, resets: None, credits: Vec::new() }
}
