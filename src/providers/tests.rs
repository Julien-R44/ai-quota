//! End-to-end provider tests: a scripted fake server stands in for the network, and a temporary
//! directory stands in for `%LOCALAPPDATA%` and the CLI credential folders.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use super::{Ctx, amp, claude, codex, fetch_all_in, opencode};
use crate::http::{Http, Response};
use crate::model::{LimitKind, PerProvider, Problem, Provider, Status, Usage};
use crate::timefmt::{now, parse_iso8601};

/// Fake server: answers with scripted responses in order and records every request.
struct FakeHttp {
    responses: Mutex<VecDeque<Result<Response, String>>>,
    calls: Mutex<Vec<String>>,
}

impl FakeHttp {
    fn new(responses: Vec<Result<Response, String>>) -> Self {
        FakeHttp { responses: Mutex::new(responses.into()), calls: Mutex::new(Vec::new()) }
    }

    /// `"GET host/path (Bearer token)"`, one entry per request.
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn answer(&self, method: &str, host: &str, path: &str, headers: &[(&str, &str)]) -> Result<Response, String> {
        let auth = headers.iter().find(|(k, _)| *k == "Authorization").map_or("", |(_, v)| v);
        self.calls.lock().unwrap().push(format!("{method} {host}{path} ({auth})"));
        self.responses.lock().unwrap().pop_front().unwrap_or_else(|| Err("no scripted response".into()))
    }
}

impl Http for FakeHttp {
    fn get(&self, host: &str, path: &str, headers: &[(&str, &str)]) -> Result<Response, String> {
        self.answer("GET", host, path, headers)
    }

    fn post(&self, host: &str, path: &str, headers: &[(&str, &str)], _body: &[u8]) -> Result<Response, String> {
        self.answer("POST", host, path, headers)
    }
}

fn json(body: &str) -> Response {
    Response { status: 200, body: body.into(), retry_after: None }
}

fn status(code: u32, retry_after: Option<u32>) -> Response {
    Response { status: code, body: Vec::new(), retry_after }
}

/// Unique temporary directory, deleted on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ai-quota-providers-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.0.join(relative)).unwrap_or_default()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn ctx<'a>(http: &'a FakeHttp, dir: &Path) -> Ctx<'a> {
    Ctx {
        http,
        state_dir: Some(dir.join("state")),
        claude_dir: dir.join("claude"),
        codex_dir: dir.join("codex"),
        opencode_dir: dir.join("opencode"),
        amp_token: Some("amp-token".into()),
        opencode_token: None,
    }
}

const CODEX_AUTH: &str = r#"{"tokens": {"access_token": "codex-token", "account_id": "acc"}}"#;

fn codex_usage(percent: u32) -> String {
    format!(
        r#"{{"plan_type": "plus", "rate_limit": {{"primary_window":
            {{"used_percent": {percent}, "limit_window_seconds": 604800, "reset_at": {}}}}}}}"#,
        now() + 86400
    )
}

fn percent(usage: &Usage) -> f32 {
    match &usage.status {
        Status::Ok(limits) => limits[0].percent,
        Status::Error(p) => panic!("expected limits, got {p:?}"),
    }
}

fn problem(usage: &Usage) -> Problem {
    match &usage.status {
        Status::Error(p) => p.clone(),
        Status::Ok(_) => panic!("expected an error"),
    }
}

#[test]
fn success_is_parsed_and_cached() {
    let dir = TempDir::new();
    dir.write("codex/auth.json", CODEX_AUTH);
    let http = FakeHttp::new(vec![Ok(json(&codex_usage(42)))]);

    let usage = codex::fetch(&ctx(&http, &dir.0), false).unwrap();

    assert!((percent(&usage) - 42.0).abs() < f32::EPSILON);
    assert_eq!(usage.plan.as_deref(), Some("Plus"));
    assert!(usage.stale_since.is_none());
    assert_eq!(http.calls(), ["GET chatgpt.com/backend-api/wham/usage (Bearer codex-token)"]);
    assert!(dir.read("state/codex.json").contains("used_percent"));
}

#[test]
fn rate_limit_shows_the_cache_and_pauses() {
    let dir = TempDir::new();
    dir.write("codex/auth.json", CODEX_AUTH);
    let http = FakeHttp::new(vec![Ok(json(&codex_usage(42))), Ok(status(429, Some(900)))]);
    let ctx = ctx(&http, &dir.0);

    codex::fetch(&ctx, false).unwrap();
    let usage = codex::fetch(&ctx, false).unwrap();

    // The 429 does not reach the user: the last good data is shown, marked as stale.
    assert!((percent(&usage) - 42.0).abs() < f32::EPSILON);
    assert!(usage.stale_since.is_some());
    // The pause honors Retry-After (15 min, more than the 5 min minimum).
    let until: i64 = dir.read("state/codex.cooldown").trim().parse().unwrap();
    assert!(until >= now() + 890);
}

#[test]
fn a_pause_skips_the_network_unless_forced() {
    let dir = TempDir::new();
    dir.write("codex/auth.json", CODEX_AUTH);
    dir.write("state/codex.cooldown", &(now() + 300).to_string());
    let http = FakeHttp::new(vec![Ok(json(&codex_usage(7)))]);
    let ctx = ctx(&http, &dir.0);

    let paused = codex::fetch(&ctx, false).unwrap();
    assert_eq!(http.calls().len(), 0, "no request during the pause");
    assert!(matches!(problem(&paused), Problem::RateLimited { .. }));

    let forced = codex::fetch(&ctx, true).unwrap();
    assert_eq!(http.calls().len(), 1, "a manual refresh goes through");
    assert!((percent(&forced) - 7.0).abs() < f32::EPSILON);
    assert!(dir.read("state/codex.cooldown").is_empty(), "a success clears the pause");
}

#[test]
fn an_expired_session_is_not_hidden_by_the_cache() {
    let dir = TempDir::new();
    dir.write("codex/auth.json", CODEX_AUTH);
    let http = FakeHttp::new(vec![Ok(json(&codex_usage(42))), Ok(status(401, None))]);
    let ctx = ctx(&http, &dir.0);

    codex::fetch(&ctx, false).unwrap();
    let usage = codex::fetch(&ctx, false).unwrap();

    assert_eq!(problem(&usage), Problem::SessionExpired(Provider::Codex));
}

#[test]
fn offline_without_cache_reports_offline() {
    let dir = TempDir::new();
    dir.write("codex/auth.json", CODEX_AUTH);
    let http = FakeHttp::new(vec![Err("timeout".into())]);

    let usage = codex::fetch(&ctx(&http, &dir.0), false).unwrap();

    assert_eq!(problem(&usage), Problem::Offline("timeout".into()));
}

#[test]
fn missing_credentials_hide_the_provider() {
    let dir = TempDir::new();
    let http = FakeHttp::new(vec![]);

    assert!(codex::fetch(&ctx(&http, &dir.0), false).is_none());
    assert!(claude::fetch(&ctx(&http, &dir.0), false).is_none());
    assert_eq!(http.calls().len(), 0);
}

#[test]
fn an_expired_claude_token_is_refreshed_and_written_back() {
    let dir = TempDir::new();
    let expired_ms = (now() - 60) * 1000;
    dir.write(
        "claude/.credentials.json",
        &format!(
            r#"{{"claudeAiOauth": {{"accessToken": "old", "refreshToken": "r1", "expiresAt": {expired_ms},
                "subscriptionType": "max"}}, "mcpOAuth": {{"keep": true}}}}"#
        ),
    );
    let http = FakeHttp::new(vec![
        Ok(json(r#"{"access_token": "new", "refresh_token": "r2", "expires_in": 28800}"#)),
        Ok(json(r#"{"limits": [{"kind": "session", "percent": 30, "resets_at": "2030-01-01T00:00:00Z"}]}"#)),
    ]);

    let usage = claude::fetch(&ctx(&http, &dir.0), false).unwrap();

    assert_eq!(
        http.calls(),
        [
            "POST platform.claude.com/v1/oauth/token ()",
            "GET api.anthropic.com/api/oauth/usage?cedar_ember=1 (Bearer new)",
        ]
    );
    assert!((percent(&usage) - 30.0).abs() < f32::EPSILON);
    let creds = dir.read("claude/.credentials.json");
    assert!(creds.contains(r#""accessToken":"new""#) && creds.contains(r#""refreshToken":"r2""#));
    assert!(creds.contains(r#""mcpOAuth":{"keep":true}"#), "other fields are left untouched");
}

#[test]
fn amp_auth_required_means_session_expired() {
    let dir = TempDir::new();
    let http = FakeHttp::new(vec![Ok(json(r#"{"ok": false, "error": {"code": "auth-required"}}"#))]);

    let usage = amp::fetch(&ctx(&http, &dir.0), false).unwrap();

    assert_eq!(problem(&usage), Problem::SessionExpired(Provider::Amp));
    assert_eq!(http.calls(), ["POST ampcode.com/api/internal?userDisplayBalanceInfo (Bearer amp-token)"]);
}

#[test]
fn disabled_providers_are_not_fetched() {
    let dir = TempDir::new();
    dir.write("codex/auth.json", CODEX_AUTH);
    dir.write("claude/.credentials.json", r#"{"claudeAiOauth": {"accessToken": "claude-token"}}"#);
    let http = FakeHttp::new(vec![Ok(json(&codex_usage(42)))]);
    let only_codex = PerProvider::from_fn(|p| p == Provider::Codex);

    let snapshot = fetch_all_in(&ctx(&http, &dir.0), false, only_codex);

    assert_eq!(http.calls(), ["GET chatgpt.com/backend-api/wham/usage (Bearer codex-token)"]);
    let providers: Vec<_> = snapshot.usages.iter().map(|u| u.provider).collect();
    assert_eq!(providers, [Provider::Codex]);
}

const OPENCODE_AUTH: &str = r#"{"opencode": {"type": "api", "key": "go-token"}}"#;

fn opencode_usage() -> String {
    r#"{"usage": {
        "rolling": {"status": "ok", "percent": 12, "resetsAt": "2030-01-01T05:00:00.000Z"},
        "weekly": {"status": "ok", "percent": 34, "resetsAt": "2030-01-07T00:00:00.000Z"},
        "monthly": {"status": "ok", "percent": 56, "resetInSec": 86400}}}"#
        .into()
}

#[test]
fn opencode_reads_the_cli_key_and_parses_the_go_windows() {
    let dir = TempDir::new();
    dir.write("opencode/auth.json", OPENCODE_AUTH);
    let http = FakeHttp::new(vec![Ok(json(&opencode_usage()))]);

    let usage = opencode::fetch(&ctx(&http, &dir.0), false).unwrap();

    assert_eq!(http.calls(), ["GET opencode.ai/zen/go/v1/usage (Bearer go-token)"]);
    assert!(usage.stale_since.is_none());
    let Status::Ok(limits) = &usage.status else { panic!("expected limits, got {:?}", usage.status) };
    assert_eq!(limits.len(), 3);
    assert_eq!(limits[0].kind, LimitKind::Session { hours: 5 });
    assert_eq!(limits[1].kind, LimitKind::Weekly);
    assert_eq!(limits[2].kind, LimitKind::Monthly);
    assert!((limits[0].percent - 12.0).abs() < f32::EPSILON);
    assert_eq!(limits[0].resets_at, parse_iso8601("2030-01-01T05:00:00Z"));
    // A relative reset counts from now.
    assert_eq!(limits[2].resets_at, Some(now() + 86400));
    assert_eq!(limits[0].window_secs, Some(5 * 3600));
    assert!(dir.read("state/opencode.json").contains("rolling"));
}

#[test]
fn opencode_reads_the_encrypted_token_before_the_cli_file() {
    let dir = TempDir::new();
    dir.write("opencode/auth.json", OPENCODE_AUTH);
    let http = FakeHttp::new(vec![Ok(json(&opencode_usage()))]);
    let ctx = Ctx { opencode_token: Some("pasted-token".into()), ..ctx(&http, &dir.0) };

    opencode::fetch(&ctx, false).unwrap();

    assert_eq!(http.calls(), ["GET opencode.ai/zen/go/v1/usage (Bearer pasted-token)"]);
}

#[test]
fn opencode_without_credentials_is_hidden() {
    let dir = TempDir::new();
    let http = FakeHttp::new(vec![]);

    assert!(opencode::fetch(&ctx(&http, &dir.0), false).is_none());
    assert_eq!(http.calls().len(), 0);
}

#[test]
fn opencode_unauthorized_means_session_expired() {
    let dir = TempDir::new();
    dir.write("opencode/auth.json", OPENCODE_AUTH);
    let http = FakeHttp::new(vec![Ok(status(401, None))]);

    let usage = opencode::fetch(&ctx(&http, &dir.0), false).unwrap();

    assert_eq!(problem(&usage), Problem::SessionExpired(Provider::Opencode));
}
