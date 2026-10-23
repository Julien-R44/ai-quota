use std::fmt;
use std::ops::{Index, IndexMut};

use crate::i18n::{Lang, lang, tr};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Claude,
    Codex,
    Amp,
    Opencode,
}

impl Provider {
    pub const COUNT: usize = 4;
    pub const ALL: [Provider; Provider::COUNT] = [Provider::Claude, Provider::Codex, Provider::Amp, Provider::Opencode];

    /// Slot in [`PerProvider`]. A new variant does not compile until it gets one here.
    const fn index(self) -> usize {
        match self {
            Provider::Claude => 0,
            Provider::Codex => 1,
            Provider::Amp => 2,
            Provider::Opencode => 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex",
            Provider::Amp => "Amp",
            Provider::Opencode => "OpenCode",
        }
    }

    pub fn short_name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Amp => "Amp",
            Provider::Opencode => "OpenCode",
        }
    }

    pub fn usage_url(self) -> &'static str {
        match self {
            Provider::Claude => "https://claude.ai/settings/usage",
            Provider::Codex => "https://chatgpt.com/codex/settings/usage",
            Provider::Amp => "https://ampcode.com/settings",
            Provider::Opencode => "https://opencode.ai/auth",
        }
    }

    fn relogin(self) -> &'static str {
        match self {
            Provider::Claude => tr("Relance `claude` puis /login.", "Run `claude` then /login."),
            Provider::Codex => tr("Relance `codex login`.", "Run `codex login`."),
            Provider::Amp => {
                tr("Colle un nouveau token d’accès dans les réglages.", "Paste a new access token in Settings.")
            }
            Provider::Opencode => tr(
                "Relance `opencode auth login`, ou colle une clé API dans les réglages.",
                "Run `opencode auth login`, or paste an API key in Settings.",
            ),
        }
    }
}

/// One value per provider, looked up by provider rather than by position: `enabled[Provider::Amp]`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PerProvider<T>([T; Provider::COUNT]);

impl<T> PerProvider<T> {
    pub fn from_fn(f: impl FnMut(Provider) -> T) -> Self {
        PerProvider(Provider::ALL.map(f))
    }

    /// Like [`PerProvider::from_fn`], stopping at the first error.
    pub fn try_from_fn<E>(f: impl FnMut(Provider) -> Result<T, E>) -> Result<Self, E> {
        let [claude, codex, amp, opencode] = Provider::ALL.map(f);
        Ok(PerProvider([claude?, codex?, amp?, opencode?]))
    }
}

impl<T> Index<Provider> for PerProvider<T> {
    type Output = T;

    fn index(&self, p: Provider) -> &T {
        &self.0[p.index()]
    }
}

impl<T> IndexMut<Provider> for PerProvider<T> {
    fn index_mut(&mut self, p: Provider) -> &mut T {
        &mut self.0[p.index()]
    }
}

/// The label is computed at display time, in the current language.
#[derive(Clone, Debug, PartialEq)]
pub enum LimitKind {
    Session {
        hours: i64,
    },
    Weekly,
    /// Per-model weekly limit ("Fable", "Opus"…).
    WeeklyModel(String),
    Days(i64),
    /// A monthly window (`OpenCode` Go).
    Monthly,
    /// Amp's daily free quota.
    AmpFree,
    /// Amp's monthly Agent budget.
    AmpAgent,
    /// Amp's monthly Orbs hours, counted per instance size ("a1.small", "a1.xxlarge"…).
    AmpOrbs(String),
    Other(String),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Amount {
    Dollars { left: f64, total: f64 },
    Hours { left: f64, total: f64 },
}

/// Number with thousands separators: "1 000,5" / "1,000.5".
fn grouped(v: f64, decimals: usize) -> String {
    let text = format!("{v:.decimals$}");
    let (int, frac) = text.split_once('.').map_or((text.as_str(), None), |(i, f)| (i, Some(f)));
    let (sep, point) = match lang() {
        // No-break space: the narrow one (U+202F) vanishes between digits in Segoe UI at small sizes.
        Lang::Fr => ('\u{a0}', ','),
        Lang::En => (',', '.'),
    };
    let digits: Vec<char> = int.chars().collect();
    let mut out = String::new();
    for (i, c) in digits.iter().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) && c.is_ascii_digit() {
            out.push(sep);
        }
        out.push(*c);
    }
    if let Some(f) = frac {
        out.push(point);
        out.push_str(f);
    }
    out
}

/// "18,57 $" / "$18.57"
pub fn dollars(v: f64) -> String {
    let text = grouped(v, if v.fract().abs() < 0.005 { 0 } else { 2 });
    match lang() {
        Lang::Fr => format!("{text}\u{a0}$"),
        Lang::En => format!("${text}"),
    }
}

impl Amount {
    /// "18,57 $ / 20 $", "732 h / 750 h". Orbs hours are rounded down, with "< 1 h" below one hour,
    /// as Amp and `CodexBar` do.
    pub fn text(&self) -> String {
        let hours = |v: f64| match (lang(), v) {
            (Lang::Fr, v) if v > 0.0 && v < 1.0 => "<\u{a0}1\u{a0}h".to_string(),
            (Lang::En, v) if v > 0.0 && v < 1.0 => "<1h".to_string(),
            (Lang::Fr, v) => format!("{}\u{a0}h", grouped(v.floor(), 0)),
            (Lang::En, v) => format!("{}h", grouped(v.floor(), 0)),
        };
        match self {
            Amount::Dollars { left, total } => format!("{} / {}", dollars(*left), dollars(*total)),
            Amount::Hours { left, total } => format!("{} / {}", hours(*left), hours(*total)),
        }
    }
}

/// Identifies a limit window across fetches. Its text form names the series in `history.tsv`, so
/// it must not change between versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LimitKey {
    ClaudeSession,
    ClaudeWeekly,
    /// Weekly limit of one model, by display name.
    ClaudeWeeklyModel(String),
    /// Window kind not known yet, by its API name.
    ClaudeOther(String),
    /// Codex identifies its windows by their length.
    CodexWindow {
        seconds: i64,
    },
    /// `OpenCode` Go identifies its windows by their length too.
    OpencodeWindow {
        seconds: i64,
    },
    AmpAgent,
    AmpOrbs,
    AmpFree,
}

impl fmt::Display for LimitKey {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            LimitKey::ClaudeSession => f.write_str("claude.session"),
            LimitKey::ClaudeWeekly => f.write_str("claude.weekly"),
            LimitKey::ClaudeWeeklyModel(model) => write!(f, "claude.weekly.{}", model.to_lowercase()),
            LimitKey::ClaudeOther(kind) => write!(f, "claude.{kind}"),
            LimitKey::CodexWindow { seconds } => write!(f, "codex.{seconds}"),
            LimitKey::OpencodeWindow { seconds } => write!(f, "opencode.{seconds}"),
            LimitKey::AmpAgent => f.write_str("amp.agent"),
            LimitKey::AmpOrbs => f.write_str("amp.orbs"),
            LimitKey::AmpFree => f.write_str("amp.free"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Limit {
    pub key: LimitKey,
    pub kind: LimitKind,
    pub percent: f32,
    /// Left / total, when the limit is in dollars or hours.
    pub amount: Option<Amount>,
    /// Unix timestamp (seconds) of the reset.
    pub resets_at: Option<i64>,
    /// Length of the rolling window, to gauge the consumption pace.
    pub window_secs: Option<i64>,
}

impl Limit {
    pub fn label(&self) -> String {
        let fr = lang() == Lang::Fr;
        match &self.kind {
            LimitKind::Session { hours } if fr => format!("Session ({hours}\u{a0}h)"),
            LimitKind::Session { hours } => format!("Session ({hours}h)"),
            LimitKind::Weekly => tr("Hebdomadaire", "Weekly").into(),
            LimitKind::WeeklyModel(model) => format!("{} · {model}", tr("Hebdo", "Weekly")),
            LimitKind::Days(days) if fr => format!("Fenêtre de {days}\u{a0}jours"),
            LimitKind::Days(days) => format!("{days}-day window"),
            LimitKind::Monthly => tr("Mensuel", "Monthly").into(),
            LimitKind::AmpFree => "Amp Free".into(),
            LimitKind::AmpAgent => "Agent".into(),
            LimitKind::AmpOrbs(size) => match size.strip_prefix("a1.") {
                Some(short) => format!("Orbs ({short})"),
                None => "Orbs".into(),
            },
            LimitKind::Other(name) => name.clone(),
        }
    }

    /// Short label, for the tray tooltip.
    pub fn short(&self) -> String {
        let fr = lang() == Lang::Fr;
        match &self.kind {
            LimitKind::Session { hours } if fr => format!("{hours}\u{a0}h"),
            LimitKind::Session { hours } => format!("{hours}h"),
            LimitKind::Weekly => tr("sem.", "wk").into(),
            LimitKind::WeeklyModel(model) => model.clone(),
            LimitKind::Days(days) if fr => format!("{days}\u{a0}j"),
            LimitKind::Days(days) => format!("{days}d"),
            LimitKind::Monthly => tr("mois", "mo").into(),
            LimitKind::AmpFree => "free".into(),
            LimitKind::AmpAgent => "agent".into(),
            LimitKind::AmpOrbs(_) => "orbs".into(),
            LimitKind::Other(name) => name.clone(),
        }
    }

    /// Fraction of the window already elapsed (0..1): where usage would be at a constant pace.
    pub fn pace(&self, now: i64) -> Option<f32> {
        let (at, window) = (self.resets_at?, self.window_secs?);
        let p = 1.0 - (at - now) as f32 / window as f32;
        (0.0..1.0).contains(&p).then_some(p)
    }
}

/// What prevents showing a provider's quotas.
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    NotLoggedIn(Provider),
    /// Codex signed in with an API key: no subscription quota.
    ApiKeyOnly,
    SessionExpired(Provider),
    RateLimited {
        until: i64,
    },
    Offline(String),
    ServerError(u32),
    Unexpected(u32),
    Unreadable,
    /// Error message returned verbatim by the service.
    Message(String),
}

impl Problem {
    /// Temporary outage: the last known data is kept meanwhile.
    pub fn is_transient(&self) -> bool {
        !matches!(self, Problem::NotLoggedIn(_) | Problem::ApiKeyOnly | Problem::SessionExpired(_))
    }

    pub fn text(&self) -> String {
        match self {
            Problem::NotLoggedIn(p) => format!("{} {}", tr("Pas connecté.", "Not signed in."), p.relogin()),
            Problem::ApiKeyOnly => tr(
                "Connecté par clé API : pas de quota d’abonnement à suivre.",
                "Signed in with an API key: no subscription quota to track.",
            )
            .into(),
            Problem::SessionExpired(p) => {
                format!("{} {}", tr("Session expirée ou révoquée.", "Session expired or revoked."), p.relogin())
            }
            Problem::RateLimited { until } => {
                let minutes = ((until - crate::timefmt::now()).max(0) + 59) / 60;
                match lang() {
                    Lang::Fr => format!("Trop de requêtes, nouvel essai dans {minutes}\u{a0}min."),
                    Lang::En => format!("Too many requests, retrying in {minutes} min."),
                }
            }
            Problem::Offline(e) => format!("{} ({e}).", tr("Hors ligne", "Offline")),
            Problem::ServerError(code) => match lang() {
                Lang::Fr => format!("Le service ne répond pas (HTTP {code})."),
                Lang::En => format!("The service is not responding (HTTP {code})."),
            },
            Problem::Unexpected(code) => match lang() {
                Lang::Fr => format!("Réponse inattendue (HTTP {code})."),
                Lang::En => format!("Unexpected response (HTTP {code})."),
            },
            Problem::Unreadable => tr("Réponse illisible.", "Unreadable response.").into(),
            Problem::Message(m) => m.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Status {
    Ok(Vec<Limit>),
    Error(Problem),
}

/// Granted limit resets not used yet ("Reset for free").
#[derive(Clone, Debug, PartialEq)]
pub struct BankedResets {
    pub count: u32,
    /// Expiry of the next one to expire, if known.
    pub next_expiry: Option<i64>,
}

/// Paid credit balance (Amp): personal or a workspace's.
#[derive(Clone, Debug, PartialEq)]
pub struct Credit {
    pub workspace: Option<String>,
    pub dollars: f64,
}

#[derive(Clone, Debug)]
pub struct Usage {
    pub provider: Provider,
    pub plan: Option<String>,
    pub status: Status,
    pub resets: Option<BankedResets>,
    pub credits: Vec<Credit>,
    /// Data kept from a previous fetch (that fetch's timestamp).
    pub stale_since: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub usages: Vec<Usage>,
    pub fetched_at: i64,
}

impl Snapshot {
    pub fn max_percent(&self) -> f32 {
        self.usages
            .iter()
            .filter_map(|u| match &u.status {
                Status::Ok(limits) => limits.iter().map(|l| l.percent).reduce(f32::max),
                Status::Error(_) => None,
            })
            .fold(0.0, f32::max)
    }
}

#[cfg(test)]
mod tests {
    use super::{Amount, Lang, LimitKey, PerProvider, Provider, dollars};
    use crate::i18n::LangGuard;

    #[test]
    fn limit_keys_keep_their_history_names() {
        let names = [
            (LimitKey::ClaudeSession, "claude.session"),
            (LimitKey::ClaudeWeekly, "claude.weekly"),
            (LimitKey::ClaudeWeeklyModel("Opus".into()), "claude.weekly.opus"),
            (LimitKey::ClaudeOther("weekly_oauth_apps".into()), "claude.weekly_oauth_apps"),
            (LimitKey::CodexWindow { seconds: 18000 }, "codex.18000"),
            (LimitKey::OpencodeWindow { seconds: 18000 }, "opencode.18000"),
            (LimitKey::AmpAgent, "amp.agent"),
            (LimitKey::AmpOrbs, "amp.orbs"),
            (LimitKey::AmpFree, "amp.free"),
        ];
        for (key, name) in names {
            assert_eq!(key.to_string(), name);
        }
    }

    #[test]
    fn per_provider_slots_follow_all() {
        for (i, p) in Provider::ALL.into_iter().enumerate() {
            assert_eq!(p.index(), i);
        }
        let mut names = PerProvider::from_fn(Provider::short_name);
        assert_eq!(names[Provider::Amp], "Amp");
        names[Provider::Codex] = "changed";
        assert_eq!(names[Provider::Codex], "changed");
        assert_eq!(names[Provider::Claude], "Claude");
    }

    #[test]
    fn formats_amounts_with_thousands() {
        let lang = LangGuard::new(Lang::En);
        assert_eq!(dollars(1234.5), "$1,234.50");
        assert_eq!(Amount::Hours { left: 991.9, total: 1000.0 }.text(), "991h / 1,000h");
        lang.set(Lang::Fr);
        assert_eq!(dollars(200.0), "200\u{a0}$");
        assert_eq!(dollars(1234.5), "1\u{a0}234,50\u{a0}$");
    }
}
