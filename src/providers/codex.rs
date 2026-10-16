use serde_json::Value;

use super::{
    Parsed, capitalize, config_dir, count, failed, fetch_json, read_json, resolve, unless_cooling_down, window_kind,
};
use crate::model::{BankedResets, Limit, Problem, Provider, Usage};
use crate::timefmt::now;

pub(super) fn fetch(forced: bool) -> Option<Usage> {
    let auth = read_json(&config_dir("CODEX_HOME", ".codex").join("auth.json"))?;
    let tokens = &auth["tokens"];
    let (Some(token), Some(account)) = (tokens["access_token"].as_str(), tokens["account_id"].as_str()) else {
        let problem = if auth["OPENAI_API_KEY"].is_string() {
            Problem::ApiKeyOnly
        } else {
            Problem::NotLoggedIn(Provider::Codex)
        };
        return Some(failed(Provider::Codex, None, problem));
    };
    let fetched = unless_cooling_down(Provider::Codex, forced, || {
        let bearer = format!("Bearer {token}");
        let headers =
            [("Authorization", bearer.as_str()), ("ChatGPT-Account-Id", account), ("Accept", "application/json")];
        fetch_json(Provider::Codex, "chatgpt.com", "/backend-api/wham/usage", &headers)
    });
    let parse = |json: &Value| Parsed {
        plan: json["plan_type"].as_str().map(plan),
        limits: parse_codex(json),
        resets: json["rate_limit_reset_credits"]["available_count"]
            .as_u64()
            .map(|n| BankedResets { count: count(n), next_expiry: None }),
        credits: Vec::new(),
    };
    Some(resolve(Provider::Codex, None, fetched, parse))
}

fn plan(plan: &str) -> String {
    match plan {
        "prolite" => "Pro Lite".into(),
        "pro" => "Pro".into(),
        "plus" => "Plus".into(),
        "team" => "Team".into(),
        "business" => "Business".into(),
        "enterprise" => "Enterprise".into(),
        "edu" => "Edu".into(),
        "free" => "Free".into(),
        other => capitalize(other),
    }
}

fn parse_codex(json: &Value) -> Vec<Limit> {
    let mut windows: Vec<(i64, Limit)> = ["primary_window", "secondary_window"]
        .iter()
        .filter_map(|key| {
            let w = json["rate_limit"].get(*key)?;
            let seconds = w["limit_window_seconds"].as_i64()?;
            let kind = window_kind(seconds);
            let resets_at =
                w["reset_at"].as_i64().or_else(|| w["reset_after_seconds"].as_i64().map(|s| now().saturating_add(s)));
            let percent = w["used_percent"].as_f64()? as f32;
            let key = format!("codex.{seconds}");
            Some((seconds, Limit { key, kind, percent, amount: None, resets_at, window_secs: Some(seconds) }))
        })
        .collect();
    windows.sort_by_key(|(s, _)| *s);
    windows.into_iter().map(|(_, l)| l).collect()
}
