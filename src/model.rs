use crate::i18n::{Lang, lang, tr};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Provider {
    Claude,
    Codex,
    Amp,
}

impl Provider {
    pub const ALL: [Provider; 3] = [Provider::Claude, Provider::Codex, Provider::Amp];

    pub fn name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude Code",
            Provider::Codex => "Codex",
            Provider::Amp => "Amp",
        }
    }

    pub fn short_name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Amp => "Amp",
        }
    }

    pub fn usage_url(self) -> &'static str {
        match self {
            Provider::Claude => "https://claude.ai/settings/usage",
            Provider::Codex => "https://chatgpt.com/codex/settings/usage",
            Provider::Amp => "https://ampcode.com/settings",
        }
    }

    fn relogin(self) -> &'static str {
        match self {
            Provider::Claude => tr("Relance `claude` puis /login.", "Run `claude` then /login."),
            Provider::Codex => tr("Relance `codex login`.", "Run `codex login`."),
            Provider::Amp => {
                tr("Colle un nouveau token d’accès dans les réglages.", "Paste a new access token in Settings.")
            }
        }
    }
}

/// Nature d'une fenêtre de limite. Le libellé est calculé à l'affichage,
/// dans la langue du moment.
#[derive(Clone, Debug, PartialEq)]
pub enum LimitKind {
    Session {
        hours: i64,
    },
    Weekly,
    /// Limite hebdomadaire propre à un modèle (« Fable », « Opus »…).
    WeeklyModel(String),
    Days(i64),
    /// Quota gratuit quotidien d'Amp.
    AmpFree,
    /// Budget Agent mensuel d'Amp.
    AmpAgent,
    /// Heures d'Orbs mensuelles d'Amp, comptées dans une taille d'instance
    /// (« a1.small », « a1.xxlarge »…).
    AmpOrbs(String),
    Other(String),
}

/// Quantité restante d'une limite exprimée en dollars ou en heures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Amount {
    Dollars { left: f64, total: f64 },
    Hours { left: f64, total: f64 },
}

/// Nombre avec séparateur de milliers : « 1 000,5 » / « 1,000.5 ».
fn grouped(v: f64, decimals: usize) -> String {
    let text = format!("{v:.decimals$}");
    let (int, frac) = text.split_once('.').map_or((text.as_str(), None), |(i, f)| (i, Some(f)));
    let (sep, point) = match lang() {
        // Espace insécable : l'espace fine (U+202F) disparaît entre deux chiffres
        // dans Segoe UI à petite taille.
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

/// « 18,57 $ » / « $18.57 »
pub fn dollars(v: f64) -> String {
    let text = grouped(v, if v.fract().abs() < 0.005 { 0 } else { 2 });
    match lang() {
        Lang::Fr => format!("{text}\u{a0}$"),
        Lang::En => format!("${text}"),
    }
}

impl Amount {
    /// « 18,57 $ / 20 $ », « 732 h / 750 h » (heures d'Orbs arrondies vers le
    /// bas, « < 1 h » en dessous, comme Amp et CodexBar).
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

#[derive(Clone, Debug)]
pub struct Limit {
    /// Identifiant stable de la fenêtre (`claude.session`, `codex.604800`…),
    /// clé de l'historique.
    pub key: String,
    pub kind: LimitKind,
    pub percent: f32,
    /// Restant / total, quand la limite est en dollars ou en heures.
    pub amount: Option<Amount>,
    /// Timestamp Unix (secondes) de la réinitialisation.
    pub resets_at: Option<i64>,
    /// Durée de la fenêtre glissante, pour situer le rythme de consommation.
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
            LimitKind::AmpFree => "Amp Free".into(),
            LimitKind::AmpAgent => "Agent".into(),
            LimitKind::AmpOrbs(size) => match size.strip_prefix("a1.") {
                Some(short) => format!("Orbs ({short})"),
                None => "Orbs".into(),
            },
            LimitKind::Other(name) => name.clone(),
        }
    }

    /// Libellé court, pour le tooltip du tray.
    pub fn short(&self) -> String {
        let fr = lang() == Lang::Fr;
        match &self.kind {
            LimitKind::Session { hours } if fr => format!("{hours}\u{a0}h"),
            LimitKind::Session { hours } => format!("{hours}h"),
            LimitKind::Weekly => tr("sem.", "wk").into(),
            LimitKind::WeeklyModel(model) => model.clone(),
            LimitKind::Days(days) if fr => format!("{days}\u{a0}j"),
            LimitKind::Days(days) => format!("{days}d"),
            LimitKind::AmpFree => "free".into(),
            LimitKind::AmpAgent => "agent".into(),
            LimitKind::AmpOrbs(_) => "orbs".into(),
            LimitKind::Other(name) => name.clone(),
        }
    }

    /// Part de la fenêtre déjà écoulée (0..1) : en consommant à rythme
    /// constant, on en serait là.
    pub fn pace(&self, now: i64) -> Option<f32> {
        let (at, window) = (self.resets_at?, self.window_secs?);
        let p = 1.0 - (at - now) as f32 / window as f32;
        (0.0..1.0).contains(&p).then_some(p)
    }
}

/// Ce qui empêche d'afficher les quotas d'un provider.
#[derive(Clone, Debug, PartialEq)]
pub enum Problem {
    NotLoggedIn(Provider),
    /// Codex connecté par clé API : pas de quota d'abonnement.
    ApiKeyOnly,
    SessionExpired(Provider),
    RateLimited {
        until: i64,
    },
    Offline(String),
    ServerError(u32),
    Unexpected(u32),
    Unreadable,
    /// Message d'erreur renvoyé tel quel par le service.
    Message(String),
}

impl Problem {
    /// Panne passagère : on garde alors les dernières données connues.
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

/// Resets de limite offerts et pas encore utilisés (« Reset for free »).
#[derive(Clone, Debug, PartialEq)]
pub struct BankedResets {
    pub count: u32,
    /// Expiration du prochain à expirer, si connue.
    pub next_expiry: Option<i64>,
}

/// Solde de crédits payants (Amp) : personnel ou d'un workspace.
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
    /// Données conservées d'un fetch précédent (timestamp de ce fetch).
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
    use super::*;
    use crate::i18n::set_lang;

    #[test]
    fn formats_amounts_with_thousands() {
        set_lang(Lang::En);
        assert_eq!(dollars(1234.5), "$1,234.50");
        assert_eq!(Amount::Hours { left: 991.9, total: 1000.0 }.text(), "991h / 1,000h");
        set_lang(Lang::Fr);
        assert_eq!(dollars(200.0), "200\u{a0}$");
        assert_eq!(dollars(1234.5), "1\u{a0}234,50\u{a0}$");
    }
}
