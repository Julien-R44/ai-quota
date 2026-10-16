//! Lecture des quotas. Uniquement des fichiers locaux + un appel HTTPS par
//! provider : on ne lance jamais les CLI (un process Node à chaque refresh,
//! c'est exactement ce qu'on veut éviter).

use std::path::{Path, PathBuf};

use regex_lite::Regex;
use serde_json::Value;

use crate::http::{self, Response};
use crate::model::{Amount, BankedResets, Credit, Limit, LimitKind, Problem, Provider, Snapshot, Status, Usage};
use crate::timefmt::{now, parse_iso8601};
use crate::{secret, timefmt};

const FIVE_HOURS: i64 = 5 * 3600;
const WEEK: i64 = 7 * 86400;

/// Après un 429, on laisse l'API tranquille au moins ce temps-là.
const MIN_COOLDOWN: i64 = 5 * 60;
/// Au-delà, une réponse en cache est trop vieille pour être ressortie.
const CACHE_MAX_AGE: i64 = 12 * 3600;

/// `forced` : actualisation demandée par l'utilisateur, qui passe outre la
/// pause imposée par un 429. `enabled` : providers suivis (ordre de `Provider`).
pub fn fetch_all(forced: bool, enabled: [bool; 3]) -> Snapshot {
    let fetchers: [fn(bool) -> Option<Usage>; 3] = [claude, codex, amp];
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
    std::env::var_os(env).map(PathBuf::from).unwrap_or_else(|| home().join(default))
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

// ------------------------------------------- état persistant (pause, cache)

/// `%LOCALAPPDATA%\ai-quota\<provider>.<ext>` : survit aux redémarrages,
/// pour ne pas re-solliciter l'API à chaque relance de l'app.
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

fn start_cooldown(p: Provider, r: &Response) {
    let until = now() + (r.retry_after.unwrap_or(0) as i64).max(MIN_COOLDOWN);
    if let Some(f) = state_file(p, "cooldown") {
        let _ = std::fs::write(f, until.max(cooldown_until(p)).to_string());
    }
}

/// Dernière réponse valide de l'API, avec son heure.
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

/// Problème d'une réponse non-200. Seuls 401/403 invalident les données connues.
fn http_error(p: Provider, r: &Response) -> Problem {
    match r.status {
        401 | 403 => Problem::SessionExpired(p),
        429 => {
            start_cooldown(p, r);
            Problem::RateLimited { until: cooldown_until(p) }
        }
        500..=599 => Problem::ServerError(r.status),
        s => Problem::Unexpected(s),
    }
}

/// Réponse JSON d'un appel d'usage, ou le problème rencontré.
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

/// Assemble le résultat. Sur une panne passagère (réseau, 429, 5xx), on
/// ressort la dernière réponse connue, marquée avec son âge, plutôt qu'une
/// erreur.
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
    let failed = |problem| Usage {
        provider: p,
        plan: plan.clone(),
        status: Status::Error(problem),
        resets: None,
        credits: Vec::new(),
        stale_since: None,
    };
    match fetched {
        Ok(json) => {
            save_cache(p, &json);
            usage(&json, None)
        }
        Err(problem) if problem.is_transient() => match load_cache(p) {
            Some((at, json)) => usage(&json, Some(at)),
            None => failed(problem),
        },
        Err(problem) => failed(problem),
    }
}

/// Ce qu'on extrait d'une réponse d'usage.
struct Parsed {
    plan: Option<String>,
    limits: Vec<Limit>,
    resets: Option<BankedResets>,
    credits: Vec<Credit>,
}

fn window_kind(seconds: i64) -> LimitKind {
    match (seconds + 1800) / 3600 {
        168 => LimitKind::Weekly,
        hours if hours < 24 => LimitKind::Session { hours },
        hours => LimitKind::Days(hours / 24),
    }
}

// ---------------------------------------------------------------- Claude Code

/// Client OAuth public de Claude Code (le même que CodexBar).
const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

fn claude(forced: bool) -> Option<Usage> {
    let path = config_dir("CLAUDE_CONFIG_DIR", ".claude").join(".credentials.json");
    let creds = read_json(&path)?;
    let oauth = creds.get("claudeAiOauth")?;
    let fetched = in_cooldown(Provider::Claude, forced).map_or_else(
        || {
            let token = claude_access_token(&path, oauth)?;
            let auth = format!("Bearer {token}");
            let agent = claude_user_agent();
            let headers = [
                ("Authorization", auth.as_str()),
                ("anthropic-beta", "oauth-2025-04-20"),
                ("Accept", "application/json"),
                // Comme Claude Code et CodexBar : l'endpoint est fait pour le CLI.
                ("User-Agent", agent.as_str()),
            ];
            // `cedar_ember` ajoute l'inventaire des resets offerts. Comme CodexBar,
            // on réessaie sans si le serveur refuse ce paramètre optionnel.
            let host = "api.anthropic.com";
            let mut response = http::get(host, "/api/oauth/usage?cedar_ember=1", &headers);
            if let Ok(r) = &response
                && (r.status == 400 || r.status == 403 && !String::from_utf8_lossy(&r.body).contains("user:profile"))
            {
                response = http::get(host, "/api/oauth/usage", &headers);
            }
            json_response(Provider::Claude, response).map(strip_grant_ids)
        },
        Err,
    );
    let parse = |json: &Value| Parsed {
        plan: None,
        limits: parse_claude(json),
        resets: claude_resets(json, now()),
        credits: Vec::new(),
    };
    Some(resolve(Provider::Claude, claude_plan(oauth), fetched, parse))
}

/// Les identifiants de grants servent à consommer un reset : on ne les garde
/// pas (la réponse est mise en cache sur disque).
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

/// Resets offerts par Claude (bloc `cedar_ember`) : un « grant » en porte un
/// ou plusieurs, utilisables entre `starts_at` et `ends_at` s'il n'est pas en pause.
fn claude_resets(json: &Value, now: i64) -> Option<BankedResets> {
    let block = &json["cedar_ember"];
    if block["eligible"].as_bool() != Some(true) {
        return None;
    }
    let (mut count, mut next_expiry) = (0u32, None::<i64>);
    for grant in block["grants"].as_array()? {
        let left = grant["resets_left"].as_u64().unwrap_or(0) as u32;
        let bound = |key: &str| grant[key].as_str().and_then(parse_iso8601);
        let started = bound("starts_at").is_none_or(|t| t <= now);
        let ends = bound("ends_at");
        if grant["paused"].as_bool() != Some(false) || left == 0 || !started || ends.is_some_and(|t| t <= now) {
            continue;
        }
        count += left;
        if let Some(t) = ends {
            next_expiry = Some(next_expiry.map_or(t, |n| n.min(t)));
        }
    }
    Some(BankedResets { count, next_expiry })
}

/// `claude-cli/2.1.301 (external, cli)`, d'après la version installée (sans
/// lancer le CLI : on lit le dossier des versions de l'installeur natif).
fn claude_user_agent() -> String {
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

/// Token d'accès valide, rafraîchi si besoin. Quand Claude Code tourne, il le
/// rafraîchit lui-même quelques minutes avant l'expiration : on ne prend le
/// relais qu'une fois le token réellement expiré.
fn claude_access_token(path: &Path, oauth: &Value) -> Result<String, Problem> {
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

    let body =
        format!("grant_type=refresh_token&refresh_token={}&client_id={CLAUDE_CLIENT_ID}", http::form_encode(refresh));
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
    let expires_ms = (now() + json["expires_in"].as_i64().unwrap_or(3600)) * 1000;
    save_claude_tokens(path, refresh, access, new_refresh, expires_ms);
    Ok(access.into())
}

/// Réécrit le fichier de Claude Code en ne touchant qu'aux champs du token :
/// le refresh token peut être à usage unique, Claude Code doit donc repartir
/// du nouveau. Écriture atomique (fichier temporaire + renommage).
fn save_claude_tokens(path: &Path, used_refresh: &str, access: &str, refresh: &str, expires_ms: i64) {
    let Some(mut creds) = read_json(path) else { return };
    let oauth = &mut creds["claudeAiOauth"];
    // Claude Code a rafraîchi entre-temps : son jeton fait foi.
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

fn claude_plan(oauth: &Value) -> Option<String> {
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
    // Format récent : un tableau `limits` déjà ordonné.
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
    // Ancien format : un objet par fenêtre.
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

// ---------------------------------------------------------------------- Codex

fn codex(forced: bool) -> Option<Usage> {
    let auth = read_json(&config_dir("CODEX_HOME", ".codex").join("auth.json"))?;
    let tokens = &auth["tokens"];
    let (Some(token), Some(account)) = (tokens["access_token"].as_str(), tokens["account_id"].as_str()) else {
        let problem = if auth["OPENAI_API_KEY"].is_string() {
            Problem::ApiKeyOnly
        } else {
            Problem::NotLoggedIn(Provider::Codex)
        };
        return Some(Usage {
            provider: Provider::Codex,
            plan: None,
            status: Status::Error(problem),
            resets: None,
            credits: Vec::new(),
            stale_since: None,
        });
    };
    let fetched = in_cooldown(Provider::Codex, forced).map_or_else(
        || {
            let bearer = format!("Bearer {token}");
            let headers =
                [("Authorization", bearer.as_str()), ("ChatGPT-Account-Id", account), ("Accept", "application/json")];
            fetch_json(Provider::Codex, "chatgpt.com", "/backend-api/wham/usage", &headers)
        },
        Err,
    );
    let parse = |json: &Value| Parsed {
        plan: json["plan_type"].as_str().map(codex_plan),
        limits: parse_codex(json),
        resets: json["rate_limit_reset_credits"]["available_count"]
            .as_u64()
            .map(|n| BankedResets { count: n as u32, next_expiry: None }),
        credits: Vec::new(),
    };
    Some(resolve(Provider::Codex, None, fetched, parse))
}

fn codex_plan(plan: &str) -> String {
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
            let resets_at = w["reset_at"].as_i64().or_else(|| w["reset_after_seconds"].as_i64().map(|s| now() + s));
            let percent = w["used_percent"].as_f64()? as f32;
            let key = format!("codex.{seconds}");
            Some((seconds, Limit { key, kind, percent, amount: None, resets_at, window_secs: Some(seconds) }))
        })
        .collect();
    windows.sort_by_key(|(s, _)| *s);
    windows.into_iter().map(|(_, l)| l).collect()
}

// ------------------------------------------------------------------------ Amp

/// Token d'accès Amp : `AMP_API_KEY`, sinon celui collé dans les réglages.
fn amp_token() -> Option<String> {
    std::env::var("AMP_API_KEY").ok().filter(|t| !t.trim().is_empty()).or_else(secret::load_amp_token)
}

fn amp(forced: bool) -> Option<Usage> {
    let token = amp_token()?;
    let fetched = in_cooldown(Provider::Amp, forced).map_or_else(
        || {
            let auth = format!("Bearer {token}");
            let headers = [
                ("Authorization", auth.as_str()),
                ("Accept", "application/json"),
                ("Content-Type", "application/json"),
            ];
            let body = br#"{"method":"userDisplayBalanceInfo","params":{}}"#;
            let response = http::post("ampcode.com", "/api/internal?userDisplayBalanceInfo", &headers, body);
            let json = json_response(Provider::Amp, response)?;
            match (json["ok"].as_bool(), json["error"]["code"].as_str()) {
                (Some(true), _) => Ok(json),
                (_, Some("auth-required")) => Err(Problem::SessionExpired(Provider::Amp)),
                _ => Err(json["error"]["message"].as_str().map_or(Problem::Unreadable, |m| Problem::Message(m.into()))),
            }
        },
        Err,
    );
    let parse = |json: &Value| parse_amp(json["result"]["displayText"].as_str().unwrap_or_default(), now());
    Some(resolve(Provider::Amp, None, fetched, parse))
}

/// Nombre affiché par Amp : « 1,500 », « 18.57 ».
fn amp_number(s: &str) -> Option<f64> {
    s.replace(',', "").parse().ok()
}

fn captures(pattern: &str, text: &str) -> Option<Vec<String>> {
    let caps = Regex::new(pattern).ok()?.captures(text)?;
    Some(caps.iter().map(|m| m.map_or(String::new(), |m| m.as_str().trim().to_string())).collect())
}

/// Échéance « resets upon renewal in 27 days / 1 month ».
fn amp_renewal(value: &str, unit: &str, now: i64) -> Option<i64> {
    let n: i64 = value.replace(',', "").parse().ok()?;
    let days = if unit.to_lowercase().starts_with("month") { n * 30 } else { n };
    Some(now + days * 86400)
}

/// « period 2026-09-13 to 2026-10-13 » : début et fin de la période facturée (UTC).
fn amp_period(text: &str) -> Option<(i64, i64)> {
    let c = captures(r"\bperiod\s+(\d{4}-\d{2}-\d{2})\s+to\s+(\d{4}-\d{2}-\d{2})\b", text)?;
    let day = |d: &str| parse_iso8601(&format!("{d}T00:00:00Z"));
    let (start, end) = (day(&c[1])?, day(&c[2])?);
    (end > start).then_some((start, end))
}

/// Le texte que l'API (et `amp usage`) affiche, découpé avec les mêmes motifs
/// que CodexBar : abonnement « Tier » (Agent en $ + heures d'Orbs), ancien
/// format en %, Amp Free, crédits.
fn parse_amp(display: &str, now: i64) -> Parsed {
    let ansi = Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").expect("motif valide");
    let text = ansi.replace_all(display, "").replace("**", "");
    const N: &str = r"([0-9][0-9,]*(?:\.[0-9]+)?)";
    let mut limits = Vec::new();
    let mut plan = None;
    let used = |left: f64, total: f64| ((total - left) / total * 100.0).clamp(0.0, 100.0) as f32;

    let tier = [
        r"(?im)^\s*Amp\s+([^\r\n]+?)\s+Tier:\s*agent\s+usage\s+\$",
        N,
        r"\s+of\s+\$",
        N,
        r"\s+remaining\b([^\r\n]*?)resets\s+upon\s+renewal\s+in\s+([0-9][0-9,]*)\s+(days?|months?)\b",
    ]
    .concat();
    let legacy_suffix = [
        r"\s*",
        N,
        r"\s*%\s+other\s+usage\s+and\s+",
        N,
        r"\s*%\s+orb\s+usage\s+remaining\s*-\s*resets\s+upon\s+renewal\s+in\s+([0-9][0-9,]*)\s+(days?|months?)",
    ]
    .concat();
    let legacy = [r"(?im)^\s*Subscription\s+(.+?):", r"(?im)^\s*Amp\s+(.+?)\s+Subscription:"]
        .iter()
        .find_map(|prefix| captures(&[prefix, legacy_suffix.as_str()].concat(), &text));

    if let Some(c) = captures(&tier, &text) {
        plan = Some(c[1].clone());
        let period = amp_period(&c[4]);
        let resets_at = period.map(|(_, end)| end).or_else(|| amp_renewal(&c[5], &c[6], now));
        let window_secs = period.map(|(start, end)| end - start);
        if let (Some(left), Some(total)) = (amp_number(&c[2]), amp_number(&c[3]))
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
        // Heures comptées dans une taille d'instance : a1.small, a1.xxlarge (Gigawatt)…
        let orbs =
            [r"(?i)\borb\s+usage\s+", N, r"h\s+of\s+", N, r"h\s+(a1\.[a-z0-9]+)\s+orb\s+hours\s+remaining\b"].concat();
        if let Some(o) = captures(&orbs, &c[4])
            && let (Some(left), Some(total)) = (amp_number(&o[1]), amp_number(&o[2]))
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
    } else if let Some(c) = legacy {
        plan = Some(c[1].clone());
        let resets_at = amp_renewal(&c[4], &c[5], now);
        for (key, kind, remaining) in
            [("amp.agent", LimitKind::AmpAgent, &c[2]), ("amp.orbs", LimitKind::AmpOrbs(String::new()), &c[3])]
        {
            if let Some(left) = amp_number(remaining) {
                let percent = (100.0 - left).clamp(0.0, 100.0) as f32;
                limits.push(Limit { key: key.into(), kind, percent, amount: None, resets_at, window_secs: None });
            }
        }
    }

    // Amp Free : en dollars qui se rechargent à l'heure, ou en % du jour.
    let free_dollars = [
        r"(?im)^\s*Amp Free:\s*\$?",
        N,
        r"\s*/\s*\$?",
        N,
        r"\s+remaining(?:\s*\(replenishes\s*\+\$?",
        N,
        r"\s*/\s*hour\))?",
    ]
    .concat();
    let free_percent = [r"(?im)^\s*Amp Free:\s*", N, r"\s*%\s+remaining"].concat();
    if let Some(c) = captures(&free_dollars, &text)
        && let (Some(left), Some(total)) = (amp_number(&c[1]), amp_number(&c[2]))
        && total > 0.0
    {
        // Recharge horaire : on affiche quand le quota sera de nouveau plein.
        let rate = amp_number(&c[3]).unwrap_or(0.0);
        let resets_at = (rate > 0.0 && left < total).then(|| now + ((total - left) / rate * 3600.0) as i64);
        limits.push(Limit {
            key: "amp.free".into(),
            kind: LimitKind::AmpFree,
            percent: used(left, total),
            amount: Some(Amount::Dollars { left, total }),
            resets_at,
            window_secs: None,
        });
    } else if let Some(c) = captures(&free_percent, &text)
        && let Some(left) = amp_number(&c[1])
    {
        // Remis à zéro chaque jour à 20 h, heure de New York.
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
    if let Some(c) = captures(&[r"(?im)^\s*Individual credits:\s*\$?", N, r"\s+remaining"].concat(), &text)
        && let Some(dollars) = amp_number(&c[1])
    {
        credits.push(Credit { workspace: None, dollars });
    }
    if let Ok(workspaces) = Regex::new(&[r"(?im)^\s*Workspace\s+(.+?):\s*\$?", N, r"\s+remaining"].concat()) {
        for c in workspaces.captures_iter(&text) {
            if let Some(dollars) = c.get(2).and_then(|m| amp_number(m.as_str())) {
                let name = c.get(1).map(|m| m.as_str().trim().to_string());
                credits.push(Credit { workspace: name, dollars });
            }
        }
    }
    Parsed { plan, limits, resets: None, credits }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        save_claude_tokens(&path, "old-refresh", "new-access", "new-refresh", 42_000);
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
        save_claude_tokens(&path, "another-refresh", "new-access", "new-refresh", 42_000);
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
        let resets = claude_resets(&json, now).unwrap();
        assert_eq!(resets.count, 3);
        assert_eq!(resets.next_expiry, parse_iso8601("2026-10-20T00:00:00Z"));
        assert!(claude_resets(&serde_json::json!({ "cedar_ember": { "eligible": false } }), now).is_none());
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
        // Réponse réelle (plan Gigawatt) : heures d'Orbs en a1.xxlarge.
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
        // (10 - 4.71) / 0.42 h ≈ 12,6 h avant d'être plein
        assert_eq!(free.limits[0].resets_at, Some(1000 + (5.29 / 0.42 * 3600.0) as i64));

        let legacy = parse_amp(
            "Subscription Pro: 62% other usage and 45% orb usage remaining - resets upon renewal in 18 days",
            0,
        );
        assert_eq!(legacy.plan.as_deref(), Some("Pro"));
        assert_eq!(legacy.limits.iter().map(|l| l.percent).collect::<Vec<_>>(), vec![38.0, 55.0]);
        assert_eq!(legacy.limits[1].resets_at, Some(18 * 86400));
    }

    #[test]
    fn form_encoding() {
        assert_eq!(http::form_encode("sk-ant_ort01.a~b"), "sk-ant_ort01.a~b");
        assert_eq!(http::form_encode("a+b/c=d"), "a%2Bb%2Fc%3Dd");
    }
}
