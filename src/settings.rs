//! Settings, in `%LOCALAPPDATA%\ai-quota\settings.json`. Start-with-Windows is not stored here:
//! the registry `Run` key is the source of truth.

use std::path::PathBuf;

use serde::{Deserialize, Deserializer, Serialize};

use crate::i18n::{self, Lang};
use crate::model::{PerProvider, Provider};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    Auto,
    Light,
    Dark,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub lang: Lang,
    pub theme: ThemePref,
    pub enabled: PerProvider<bool>,
    pub poll_minutes: u32,
    /// Poll every 2 min while a limit is above 80 %.
    pub poll_faster_when_hot: bool,
}

pub const POLL_CHOICES: [u32; 4] = [2, 5, 10, 15];

impl Default for Settings {
    fn default() -> Self {
        Settings {
            lang: i18n::system_lang(),
            theme: ThemePref::Auto,
            enabled: PerProvider::from_fn(|_| true),
            poll_minutes: 5,
            poll_faster_when_hot: true,
        }
    }
}

/// On-disk shape. Every field is parsed on its own, so a missing or invalid value falls back to
/// its default instead of discarding the whole file.
#[derive(Serialize, Deserialize, Default)]
struct Stored {
    #[serde(default, deserialize_with = "lenient")]
    lang: Option<Lang>,
    #[serde(default, deserialize_with = "lenient")]
    theme: Option<ThemePref>,
    #[serde(default, deserialize_with = "lenient")]
    providers: Option<Providers>,
    #[serde(default, deserialize_with = "lenient")]
    poll_minutes: Option<u32>,
    #[serde(default, deserialize_with = "lenient")]
    poll_faster_when_hot: Option<bool>,
}

#[derive(Serialize, Deserialize, Default)]
struct Providers {
    #[serde(default, deserialize_with = "lenient")]
    claude: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    codex: Option<bool>,
    #[serde(default, deserialize_with = "lenient")]
    amp: Option<bool>,
}

/// Only sound on an in-memory `serde_json::Value`: a streaming deserializer could be left
/// mid-value after a type error.
// Serde's deserialize_with requires a Result even though malformed fields intentionally become None.
#[allow(clippy::unnecessary_wraps)]
fn lenient<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<T>, D::Error> {
    Ok(T::deserialize(d).ok())
}

fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("ai-quota").join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        path().and_then(|p| std::fs::read(p).ok()).map_or_else(Settings::default, |b| Settings::from_json(&b))
    }

    pub fn save(&self) {
        let Some(path) = path() else { return };
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
        let _ = std::fs::write(path, self.to_json());
    }

    fn from_json(bytes: &[u8]) -> Self {
        let stored = serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|v| Stored::deserialize(v).ok())
            .unwrap_or_default();
        let d = Settings::default();
        let p = stored.providers.unwrap_or_default();
        Settings {
            lang: stored.lang.unwrap_or(d.lang),
            theme: stored.theme.unwrap_or(d.theme),
            enabled: PerProvider::from_fn(|provider| {
                match provider {
                    Provider::Claude => p.claude,
                    Provider::Codex => p.codex,
                    Provider::Amp => p.amp,
                }
                .unwrap_or(true)
            }),
            poll_minutes: stored.poll_minutes.filter(|m| POLL_CHOICES.contains(m)).unwrap_or(d.poll_minutes),
            poll_faster_when_hot: stored.poll_faster_when_hot.unwrap_or(d.poll_faster_when_hot),
        }
    }

    fn to_json(&self) -> Vec<u8> {
        let stored = Stored {
            lang: Some(self.lang),
            theme: Some(self.theme),
            providers: Some(Providers {
                claude: Some(self.enabled[Provider::Claude]),
                codex: Some(self.enabled[Provider::Codex]),
                amp: Some(self.enabled[Provider::Amp]),
            }),
            poll_minutes: Some(self.poll_minutes),
            poll_faster_when_hot: Some(self.poll_faster_when_hot),
        };
        serde_json::to_vec_pretty(&stored).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{Lang, PerProvider, Provider, Settings, ThemePref};

    #[test]
    fn json_roundtrip() {
        let s = Settings {
            lang: Lang::En,
            theme: ThemePref::Dark,
            enabled: PerProvider::from_fn(|p| p != Provider::Codex),
            poll_minutes: 10,
            poll_faster_when_hot: false,
        };
        let json = s.to_json();
        let expected = r#"{
  "lang": "en",
  "theme": "dark",
  "providers": {
    "claude": true,
    "codex": false,
    "amp": true
  },
  "poll_minutes": 10,
  "poll_faster_when_hot": false
}"#;
        assert_eq!(std::str::from_utf8(&json).unwrap(), expected);
        assert_eq!(Settings::from_json(&json), s);
    }

    #[test]
    fn invalid_fields_fall_back_to_defaults() {
        let d = Settings::default();
        assert_eq!(Settings::from_json(b"not json"), d);
        assert_eq!(Settings::from_json(b"[1, 2]"), d);
        let s = Settings::from_json(
            br#"{"lang": "de", "theme": "neon", "providers": {"codex": false, "amp": "yes"},
                 "poll_minutes": 7, "poll_faster_when_hot": 1, "unknown": null}"#,
        );
        assert_eq!(s, Settings { enabled: PerProvider::from_fn(|p| p != Provider::Codex), ..d.clone() });
        let s = Settings::from_json(br#"{"lang": "fr", "theme": "light", "providers": 3, "poll_minutes": 2}"#);
        assert_eq!(s, Settings { lang: Lang::Fr, theme: ThemePref::Light, poll_minutes: 2, ..d });
    }
}
