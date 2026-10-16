use std::path::Path;

use serde_json::Value;

use super::{
    Parsed, capitalize, config_dir, count, home, http_error, json_response, read_json, resolve, unless_cooling_down,
};
use crate::http;
use crate::model::{BankedResets, Limit, LimitKind, Problem, Provider, Usage};
use crate::timefmt::{now, parse_iso8601};

const FIVE_HOURS: i64 = 5 * 3600;
const WEEK: i64 = 7 * 86400;

/// Claude Code's public OAuth client (the same one `CodexBar` uses).
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

pub(super) fn fetch(forced: bool) -> Option<Usage> {
    let path = config_dir("CLAUDE_CONFIG_DIR", ".claude").join(".credentials.json");
    let creds = read_json(&path)?;
    let oauth = creds.get("claudeAiOauth")?;
    let fetched = unless_cooling_down(Provider::Claude, forced, || {
        let token = access_token(&path, oauth)?;
        let auth = format!("Bearer {token}");
        let agent = user_agent();
        let headers = [
            ("Authorization", auth.as_str()),
            ("anthropic-beta", "oauth-2025-04-20"),
            ("Accept", "application/json"),
            // Like Claude Code and CodexBar: the endpoint is meant for the CLI.
            ("User-Agent", agent.as_str()),
        ];
        // `cedar_ember` adds the inventory of granted resets. Like CodexBar,
        // retry without it if the server rejects this optional parameter.
        let host = "api.anthropic.com";
        let mut response = http::get(host, "/api/oauth/usage?cedar_ember=1", &headers);
        if let Ok(r) = &response
            && (r.status == 400 || r.status == 403 && !String::from_utf8_lossy(&r.body).contains("user:profile"))
        {
            response = http::get(host, "/api/oauth/usage", &headers);
        }
        json_response(Provider::Claude, response).map(strip_grant_ids)
    });
    let parse = |json: &Value| Parsed {
        plan: None,
        limits: parse_claude(json),
        resets: banked_resets(json, now()),
        credits: Vec::new(),
    };
    Some(resolve(Provider::Claude, plan(oauth), fetched, parse))
}

/// Grant ids are what consumes a reset: drop them, since the response is cached on disk.
fn strip_grant_ids(mut json: Value) -> Value {
    if let Some(block) = json.get_mut("cedar_ember").and_then(Value::as_object_mut) {
        block.remove("next_grant_id");
        for grant in block.get_mut("grants").and_then(Value::as_array_mut).into_iter().flatten() {
            if let Some(grant) = grant.as_object_mut() {
                grant.remove("id");
            }
        }
    }
    json
}

/// Resets granted by Claude (`cedar_ember` block): a grant carries one or more,
/// usable between `starts_at` and `ends_at` unless it is paused.
fn banked_resets(json: &Value, now: i64) -> Option<BankedResets> {
    let block = &json["cedar_ember"];
    if block["eligible"].as_bool() != Some(true) {
        return None;
    }
    let (mut total, mut next_expiry) = (0u32, None::<i64>);
    for grant in block["grants"].as_array()? {
        let left = grant["resets_left"].as_u64().map_or(0, count);
        let bound = |key: &str| grant[key].as_str().and_then(parse_iso8601);
        let started = bound("starts_at").is_none_or(|t| t <= now);
        let ends = bound("ends_at");
        if grant["paused"].as_bool() != Some(false) || left == 0 || !started || ends.is_some_and(|t| t <= now) {
            continue;
        }
        total = total.saturating_add(left);
        if let Some(t) = ends {
            next_expiry = Some(next_expiry.map_or(t, |n| n.min(t)));
        }
    }
    Some(BankedResets { count: total, next_expiry })
}

/// `claude-cli/2.1.301 (external, cli)`, from the installed version (read from
/// the native installer's versions folder, without running the CLI).
fn user_agent() -> String {
    let versions = home().join(".local").join("share").join("claude").join("versions");
    let parse = |name: &str| -> Option<Vec<u32>> { name.split('.').map(|n| n.parse().ok()).collect() };
    let latest = std::fs::read_dir(versions)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter_map(|name| Some((parse(&name)?, name)))
        .max()
        .map_or_else(|| "2.1.0".to_string(), |(_, name)| name);
    format!("claude-cli/{latest} (external, cli)")
}

/// Valid access token, refreshed if needed. A running Claude Code refreshes it
/// itself a few minutes before expiry, so only take over once it has actually
/// expired.
fn access_token(path: &Path, oauth: &Value) -> Result<String, Problem> {
    let Some(token) = oauth["accessToken"].as_str() else {
        return Err(Problem::NotLoggedIn(Provider::Claude));
    };
    let expires_at = oauth["expiresAt"].as_i64().map_or(i64::MAX, |ms| ms / 1000);
    if expires_at > now() + 30 {
        return Ok(token.into());
    }
    let Some(refresh) = oauth["refreshToken"].as_str() else {
        return Err(Problem::SessionExpired(Provider::Claude));
    };

    let body = format!("grant_type=refresh_token&refresh_token={}&client_id={CLIENT_ID}", http::form_encode(refresh));
    let headers = [("Content-Type", "application/x-www-form-urlencoded"), ("Accept", "application/json")];
    let r =
        http::post("platform.claude.com", "/v1/oauth/token", &headers, body.as_bytes()).map_err(Problem::Offline)?;
    match r.status {
        200 => {}
        400 | 401 => return Err(Problem::SessionExpired(Provider::Claude)),
        _ => return Err(http_error(Provider::Claude, &r)),
    }
    let json: Value = serde_json::from_slice(&r.body).map_err(|_| Problem::Unreadable)?;
    let access = json["access_token"].as_str().ok_or(Problem::Unreadable)?;
    let new_refresh = json["refresh_token"].as_str().unwrap_or(refresh);
    let expires_in = json["expires_in"].as_i64().unwrap_or(3600);
    let expires_ms = now().saturating_add(expires_in).saturating_mul(1000);
    save_tokens(path, refresh, access, new_refresh, expires_ms);
    Ok(access.into())
}

/// Rewrites Claude Code's file, touching only the token fields: the refresh
/// token may be single-use, so Claude Code must continue from the new one.
/// Atomic write (temporary file + rename).
fn save_tokens(path: &Path, used_refresh: &str, access: &str, refresh: &str, expires_ms: i64) {
    let Some(mut creds) = read_json(path) else { return };
    let oauth = &mut creds["claudeAiOauth"];
    // Claude Code refreshed in the meantime: its token wins.
    if oauth["refreshToken"].as_str() != Some(used_refresh) {
        return;
    }
    oauth["accessToken"] = access.into();
    oauth["refreshToken"] = refresh.into();
    oauth["expiresAt"] = expires_ms.into();
    let Ok(bytes) = serde_json::to_vec(&creds) else { return };
    let tmp = path.with_extension("json.ai-quota.tmp");
    if std::fs::write(&tmp, bytes).is_err() || std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

fn plan(oauth: &Value) -> Option<String> {
    let tier = oauth["rateLimitTier"].as_str().unwrap_or_default();
    let mult = ["20x", "5x"].into_iter().find(|m| tier.ends_with(m));
    let base = match oauth["subscriptionType"].as_str()? {
        "max" => "Max",
        "pro" => "Pro",
        "team" => "Team",
        "enterprise" => "Enterprise",
        "free" => "Free",
        other => return Some(capitalize(other)),
    };
    Some(match mult {
        Some(m) => format!("{base} {m}"),
        None => base.into(),
    })
}

fn parse_claude(json: &Value) -> Vec<Limit> {
    // Current format: an already ordered `limits` array.
    if let Some(limits) = json["limits"].as_array() {
        let parsed: Vec<Limit> = limits
            .iter()
            .filter_map(|l| {
                let percent = l["percent"].as_f64()? as f32;
                let (key, kind, window) = match l["kind"].as_str()? {
                    "session" => ("claude.session".into(), LimitKind::Session { hours: 5 }, Some(FIVE_HOURS)),
                    "weekly_all" => ("claude.weekly".into(), LimitKind::Weekly, Some(WEEK)),
                    "weekly_scoped" => {
                        let model = l["scope"]["model"]["display_name"].as_str().unwrap_or("?");
                        let key = format!("claude.weekly.{}", model.to_lowercase());
                        (key, LimitKind::WeeklyModel(model.into()), Some(WEEK))
                    }
                    other => (format!("claude.{other}"), LimitKind::Other(capitalize(&other.replace('_', " "))), None),
                };
                Some(Limit {
                    key,
                    kind,
                    percent,
                    amount: None,
                    resets_at: l["resets_at"].as_str().and_then(parse_iso8601),
                    window_secs: window,
                })
            })
            .collect();
        if !parsed.is_empty() {
            return parsed;
        }
    }
    // Legacy format: one object per window.
    [
        ("five_hour", "claude.session", LimitKind::Session { hours: 5 }, FIVE_HOURS),
        ("seven_day", "claude.weekly", LimitKind::Weekly, WEEK),
        ("seven_day_opus", "claude.weekly.opus", LimitKind::WeeklyModel("Opus".into()), WEEK),
        ("seven_day_sonnet", "claude.weekly.sonnet", LimitKind::WeeklyModel("Sonnet".into()), WEEK),
    ]
    .into_iter()
    .filter_map(|(field, key, kind, window)| {
        let w = json.get(field)?;
        Some(Limit {
            key: key.into(),
            kind,
            amount: None,
            percent: w["utilization"].as_f64()? as f32,
            resets_at: w["resets_at"].as_str().and_then(parse_iso8601),
            window_secs: Some(window),
        })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{banked_resets, parse_iso8601, save_tokens};

    fn temp_creds(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ai-quota-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        std::fs::write(&path, content).unwrap();
        path
    }

    const CREDS: &str = r#"{"claudeAiOauth":{"accessToken":"old-access","refreshToken":"old-refresh","expiresAt":1,"scopes":["user:profile"],"subscriptionType":"max"},"mcpOAuth":{"x":{"token":"keep"}}}"#;

    #[test]
    fn save_tokens_only_touches_token_fields() {
        let path = temp_creds("save", CREDS);
        save_tokens(&path, "old-refresh", "new-access", "new-refresh", 42_000);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            saved,
            r#"{"claudeAiOauth":{"accessToken":"new-access","refreshToken":"new-refresh","expiresAt":42000,"scopes":["user:profile"],"subscriptionType":"max"},"mcpOAuth":{"x":{"token":"keep"}}}"#
        );
        assert!(!path.with_extension("json.ai-quota.tmp").exists());
    }

    #[test]
    fn save_tokens_yields_to_concurrent_refresh() {
        let path = temp_creds("race", CREDS);
        save_tokens(&path, "another-refresh", "new-access", "new-refresh", 42_000);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CREDS);
    }

    #[test]
    fn counts_available_claude_resets() {
        let json = serde_json::json!({ "cedar_ember": { "eligible": true, "grants": [
            { "resets_left": 2, "resets_total": 3, "ends_at": "2026-11-01T00:00:00Z", "paused": false },
            { "resets_left": 1, "ends_at": "2026-10-20T00:00:00Z", "paused": false },
            { "resets_left": 1, "paused": true },
            { "resets_left": 1, "starts_at": "2030-01-01T00:00:00Z", "paused": false },
            { "resets_left": 1, "ends_at": "2020-01-01T00:00:00Z", "paused": false },
        ]}});
        let now = parse_iso8601("2026-10-16T00:00:00Z").unwrap();
        let resets = banked_resets(&json, now).unwrap();
        assert_eq!(resets.count, 3);
        assert_eq!(resets.next_expiry, parse_iso8601("2026-10-20T00:00:00Z"));
        assert!(banked_resets(&serde_json::json!({ "cedar_ember": { "eligible": false } }), now).is_none());
    }
}
