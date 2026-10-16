//! Quota fetching. Only local files plus one HTTPS call per provider: the CLIs
//! are never spawned (a Node process on every refresh is exactly what this app
//! avoids).

mod amp;
mod claude;
mod codex;

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::http::{self, Response};
use crate::model::{BankedResets, Credit, Limit, LimitKind, Problem, Provider, Snapshot, Status, Usage};
use crate::timefmt::now;

/// After a 429, leave the API alone for at least this long.
const MIN_COOLDOWN: i64 = 5 * 60;
/// Past this age, a cached response is too old to be shown.
const CACHE_MAX_AGE: i64 = 12 * 3600;

/// `forced`: refresh requested by the user, which overrides the pause imposed
/// by a 429. `enabled`: tracked providers, in `Provider` order.
pub fn fetch_all(forced: bool, enabled: [bool; 3]) -> Snapshot {
    let fetchers: [fn(bool) -> Option<Usage>; 3] = [claude::fetch, codex::fetch, amp::fetch];
    let usages = std::thread::scope(|s| {
        let threads: Vec<_> = fetchers
            .into_iter()
            .zip(enabled)
            .map(|(fetch, on)| s.spawn(move || if on { fetch(forced) } else { None }))
            .collect();
        threads.into_iter().filter_map(|t| t.join().ok().flatten()).collect()
    });
    Snapshot { usages, fetched_at: now() }
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default()
}

fn config_dir(env: &str, default: &str) -> PathBuf {
    std::env::var_os(env).map_or_else(|| home().join(default), PathBuf::from)
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

// ------------------------------------------- persisted state (cooldown, cache)

/// `%LOCALAPPDATA%\ai-quota\<provider>.<ext>`: survives restarts, so relaunching
/// the app does not hit the API again.
fn state_file(p: Provider, ext: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("ai-quota");
    let _ = std::fs::create_dir_all(&dir);
    Some(dir.join(format!("{}.{ext}", p.short_name().to_lowercase())))
}

fn cooldown_until(p: Provider) -> i64 {
    let text = state_file(p, "cooldown").and_then(|f| std::fs::read_to_string(f).ok());
    text.and_then(|t| t.trim().parse().ok()).unwrap_or(0)
}

fn in_cooldown(p: Provider, forced: bool) -> Option<Problem> {
    let until = cooldown_until(p);
    (!forced && until > now()).then_some(Problem::RateLimited { until })
}

/// Runs `fetch` unless the provider is still paused after a 429.
fn unless_cooling_down(
    p: Provider,
    forced: bool,
    fetch: impl FnOnce() -> Result<Value, Problem>,
) -> Result<Value, Problem> {
    in_cooldown(p, forced).map_or_else(fetch, Err)
}

/// Returns the end of the pause, even if it could not be persisted.
fn start_cooldown(p: Provider, r: &Response) -> i64 {
    let retry_after = r.retry_after.map_or(0, i64::from);
    let until = (now() + retry_after.max(MIN_COOLDOWN)).max(cooldown_until(p));
    if let Some(f) = state_file(p, "cooldown") {
        let _ = std::fs::write(f, until.to_string());
    }
    until
}

/// Last valid API response, with its timestamp.
fn save_cache(p: Provider, json: &Value) {
    if let Some(f) = state_file(p, "json") {
        let _ = std::fs::write(f, serde_json::json!({ "fetched_at": now(), "body": json }).to_string());
    }
    if let Some(f) = state_file(p, "cooldown") {
        let _ = std::fs::remove_file(f);
    }
}

fn load_cache(p: Provider) -> Option<(i64, Value)> {
    let mut cache = read_json(&state_file(p, "json")?)?;
    let at = cache["fetched_at"].as_i64()?;
    (now() - at < CACHE_MAX_AGE).then(|| (at, cache["body"].take()))
}

/// Problem for a non-200 response. Only 401/403 invalidate the known data.
fn http_error(p: Provider, r: &Response) -> Problem {
    match r.status {
        401 | 403 => Problem::SessionExpired(p),
        429 => Problem::RateLimited { until: start_cooldown(p, r) },
        500..=599 => Problem::ServerError(r.status),
        s => Problem::Unexpected(s),
    }
}

fn fetch_json(p: Provider, host: &str, path: &str, headers: &[(&str, &str)]) -> Result<Value, Problem> {
    json_response(p, http::get(host, path, headers))
}

fn json_response(p: Provider, response: Result<Response, String>) -> Result<Value, Problem> {
    match response {
        Err(e) => Err(Problem::Offline(e)),
        Ok(r) if r.status != 200 => Err(http_error(p, &r)),
        Ok(r) => serde_json::from_slice(&r.body).map_err(|_| Problem::Unreadable),
    }
}

fn failed(provider: Provider, plan: Option<String>, problem: Problem) -> Usage {
    Usage { provider, plan, status: Status::Error(problem), resets: None, credits: Vec::new(), stale_since: None }
}

/// Builds the result. On a transient failure (network, 429, 5xx), the last
/// known response is shown, marked with its age, rather than an error.
fn resolve(p: Provider, plan: Option<String>, fetched: Result<Value, Problem>, parse: fn(&Value) -> Parsed) -> Usage {
    let usage = |json: &Value, stale_since| {
        let parsed = parse(json);
        Usage {
            provider: p,
            plan: parsed.plan.or_else(|| plan.clone()),
            status: Status::Ok(parsed.limits),
            resets: parsed.resets.filter(|r| r.count > 0),
            credits: parsed.credits,
            stale_since,
        }
    };
    match fetched {
        Ok(json) => {
            save_cache(p, &json);
            usage(&json, None)
        }
        Err(problem) if problem.is_transient() => match load_cache(p) {
            Some((at, json)) => usage(&json, Some(at)),
            None => failed(p, plan, problem),
        },
        Err(problem) => failed(p, plan, problem),
    }
}

/// What is extracted from a usage response.
struct Parsed {
    plan: Option<String>,
    limits: Vec<Limit>,
    resets: Option<BankedResets>,
    credits: Vec<Credit>,
}

fn window_kind(seconds: i64) -> LimitKind {
    match seconds.saturating_add(1800) / 3600 {
        168 => LimitKind::Weekly,
        hours if hours < 24 => LimitKind::Session { hours },
        hours => LimitKind::Days(hours / 24),
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// Saturating conversion for counts read from JSON.
fn count(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
