//! Quota fetching. Only local files plus one HTTPS call per provider: the CLIs
//! are never spawned (a Node process on every refresh is exactly what this app
//! avoids).

mod amp;
mod claude;
mod codex;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::http::{Http, Response, WinHttp};
use crate::model::{BankedResets, Credit, Limit, LimitKind, PerProvider, Problem, Provider, Snapshot, Status, Usage};
use crate::secret;
use crate::timefmt::now;

/// After a 429, leave the API alone for at least this long.
const MIN_COOLDOWN: i64 = 5 * 60;
/// Past this age, a cached response is too old to be shown.
const CACHE_MAX_AGE: i64 = 12 * 3600;

/// Everything the providers touch outside the process: the network, the CLI credential files,
/// the Amp token and the directory holding cooldowns and cached responses. [`Ctx::system`] wires
/// the real machine; tests build one on a temporary directory with a fake [`Http`].
pub struct Ctx<'a> {
    pub http: &'a dyn Http,
    /// Cooldowns and cached responses (`%LOCALAPPDATA%\ai-quota`). `None` disables persistence.
    pub state_dir: Option<PathBuf>,
    /// Holds Claude Code's `.credentials.json`.
    pub claude_dir: PathBuf,
    /// Holds Codex's `auth.json`.
    pub codex_dir: PathBuf,
    pub amp_token: Option<String>,
}

impl Ctx<'static> {
    /// The real machine: `WinHTTP`, `%LOCALAPPDATA%`, `CLAUDE_CONFIG_DIR` / `CODEX_HOME` (or their
    /// default folders), and the Amp token from `AMP_API_KEY` or the encrypted settings file.
    pub fn system() -> Self {
        let config_dir =
            |env: &str, default: &str| std::env::var_os(env).map_or_else(|| home().join(default), PathBuf::from);
        Ctx {
            http: &WinHttp,
            state_dir: std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("ai-quota")),
            claude_dir: config_dir("CLAUDE_CONFIG_DIR", ".claude"),
            codex_dir: config_dir("CODEX_HOME", ".codex"),
            amp_token: std::env::var("AMP_API_KEY")
                .ok()
                .filter(|t| !t.trim().is_empty())
                .or_else(secret::load_amp_token),
        }
    }
}

/// `forced`: refresh requested by the user, which overrides the pause imposed by a 429.
pub fn fetch_all(forced: bool, enabled: PerProvider<bool>) -> Snapshot {
    fetch_all_in(&Ctx::system(), forced, enabled)
}

fn fetch_all_in(ctx: &Ctx, forced: bool, enabled: PerProvider<bool>) -> Snapshot {
    let usages = std::thread::scope(|s| {
        let threads: Vec<_> =
            Provider::ALL.into_iter().filter(|&p| enabled[p]).map(|p| s.spawn(move || fetch(ctx, p, forced))).collect();
        threads.into_iter().filter_map(|t| t.join().ok().flatten()).collect()
    });
    Snapshot { usages, fetched_at: now() }
}

/// `None` when the provider's CLI is not signed in on this machine.
fn fetch(ctx: &Ctx, p: Provider, forced: bool) -> Option<Usage> {
    match p {
        Provider::Claude => claude::fetch(ctx, forced),
        Provider::Codex => codex::fetch(ctx, forced),
        Provider::Amp => amp::fetch(ctx, forced),
    }
}

fn home() -> PathBuf {
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_default()
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// `<state_dir>\<provider>.<ext>`: survives restarts, so relaunching the app does not hit the API
/// again.
fn state_file(ctx: &Ctx, p: Provider, ext: &str) -> Option<PathBuf> {
    let dir = ctx.state_dir.as_ref()?;
    let _ = std::fs::create_dir_all(dir);
    Some(dir.join(format!("{}.{ext}", p.short_name().to_lowercase())))
}

fn cooldown_until(ctx: &Ctx, p: Provider) -> i64 {
    let text = state_file(ctx, p, "cooldown").and_then(|f| std::fs::read_to_string(f).ok());
    text.and_then(|t| t.trim().parse().ok()).unwrap_or(0)
}

/// Runs `fetch` unless the provider is still paused after a 429.
fn unless_cooling_down(
    ctx: &Ctx,
    p: Provider,
    forced: bool,
    fetch: impl FnOnce() -> Result<Value, Problem>,
) -> Result<Value, Problem> {
    let until = cooldown_until(ctx, p);
    if !forced && until > now() { Err(Problem::RateLimited { until }) } else { fetch() }
}

/// Returns the end of the pause, even if it could not be persisted.
fn start_cooldown(ctx: &Ctx, p: Provider, r: &Response) -> i64 {
    let retry_after = r.retry_after.map_or(0, i64::from);
    let until = (now() + retry_after.max(MIN_COOLDOWN)).max(cooldown_until(ctx, p));
    if let Some(f) = state_file(ctx, p, "cooldown") {
        let _ = std::fs::write(f, until.to_string());
    }
    until
}

/// Last valid API response, with its timestamp.
fn save_cache(ctx: &Ctx, p: Provider, json: &Value) {
    if let Some(f) = state_file(ctx, p, "json") {
        let _ = std::fs::write(f, serde_json::json!({ "fetched_at": now(), "body": json }).to_string());
    }
    if let Some(f) = state_file(ctx, p, "cooldown") {
        let _ = std::fs::remove_file(f);
    }
}

fn load_cache(ctx: &Ctx, p: Provider) -> Option<(i64, Value)> {
    let mut cache = read_json(&state_file(ctx, p, "json")?)?;
    let at = cache["fetched_at"].as_i64()?;
    (now() - at < CACHE_MAX_AGE).then(|| (at, cache["body"].take()))
}

/// Problem for a non-200 response. Only 401/403 invalidate the known data.
fn http_error(ctx: &Ctx, p: Provider, r: &Response) -> Problem {
    match r.status {
        401 | 403 => Problem::SessionExpired(p),
        429 => Problem::RateLimited { until: start_cooldown(ctx, p, r) },
        500..=599 => Problem::ServerError(r.status),
        s => Problem::Unexpected(s),
    }
}

fn fetch_json(ctx: &Ctx, p: Provider, host: &str, path: &str, headers: &[(&str, &str)]) -> Result<Value, Problem> {
    json_response(ctx, p, ctx.http.get(host, path, headers))
}

fn json_response(ctx: &Ctx, p: Provider, response: Result<Response, String>) -> Result<Value, Problem> {
    match response {
        Err(e) => Err(Problem::Offline(e)),
        Ok(r) if r.status != 200 => Err(http_error(ctx, p, &r)),
        Ok(r) => serde_json::from_slice(&r.body).map_err(|_| Problem::Unreadable),
    }
}

fn failed(provider: Provider, plan: Option<String>, problem: Problem) -> Usage {
    Usage { provider, plan, status: Status::Error(problem), resets: None, credits: Vec::new(), stale_since: None }
}

/// Builds the result. On a transient failure (network, 429, 5xx), the last
/// known response is shown, marked with its age, rather than an error.
fn resolve(
    ctx: &Ctx,
    p: Provider,
    plan: Option<String>,
    fetched: Result<Value, Problem>,
    parse: fn(&Value) -> Parsed,
) -> Usage {
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
            save_cache(ctx, p, &json);
            usage(&json, None)
        }
        Err(problem) if problem.is_transient() => match load_cache(ctx, p) {
            Some((at, json)) => usage(&json, Some(at)),
            None => failed(p, plan, problem),
        },
        Err(problem) => failed(p, plan, problem),
    }
}

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

fn count(n: u64) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}
