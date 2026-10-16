//! Réglages, dans `%LOCALAPPDATA%\ai-quota\settings.json`. Le lancement au
//! démarrage n'y est pas : la clé `Run` du registre fait foi.

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::i18n::{self, Lang};
use crate::model::Provider;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemePref {
    Auto,
    Light,
    Dark,
}

#[derive(Clone, Debug)]
pub struct Settings {
    pub lang: Lang,
    pub theme: ThemePref,
    /// Providers suivis, dans l'ordre de `Provider`.
    pub enabled: [bool; 3],
    pub poll_minutes: u32,
    /// Rafraîchir toutes les 2 min quand une limite dépasse 80 %.
    pub poll_faster_when_hot: bool,
}

pub const POLL_CHOICES: [u32; 4] = [2, 5, 10, 15];

impl Default for Settings {
    fn default() -> Self {
        Settings {
            lang: i18n::system_lang(),
            theme: ThemePref::Auto,
            enabled: [true; 3],
            poll_minutes: 5,
            poll_faster_when_hot: true,
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("ai-quota").join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        let mut s = Settings::default();
        let Some(json) =
            path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        else {
            return s;
        };
        match json["lang"].as_str() {
            Some("fr") => s.lang = Lang::Fr,
            Some("en") => s.lang = Lang::En,
            _ => {}
        }
        match json["theme"].as_str() {
            Some("light") => s.theme = ThemePref::Light,
            Some("dark") => s.theme = ThemePref::Dark,
            _ => {}
        }
        for (i, key) in ["claude", "codex", "amp"].into_iter().enumerate() {
            if let Some(on) = json["providers"][key].as_bool() {
                s.enabled[i] = on;
            }
        }
        if let Some(m) = json["poll_minutes"].as_u64().filter(|m| POLL_CHOICES.contains(&(*m as u32))) {
            s.poll_minutes = m as u32;
        }
        if let Some(b) = json["poll_faster_when_hot"].as_bool() {
            s.poll_faster_when_hot = b;
        }
        s
    }

    pub fn save(&self) {
        let Some(path) = path() else { return };
        let json = json!({
            "lang": if self.lang == Lang::Fr { "fr" } else { "en" },
            "theme": match self.theme { ThemePref::Auto => "auto", ThemePref::Light => "light", ThemePref::Dark => "dark" },
            "providers": { "claude": self.enabled[0], "codex": self.enabled[1], "amp": self.enabled[2] },
            "poll_minutes": self.poll_minutes,
            "poll_faster_when_hot": self.poll_faster_when_hot,
        });
        let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
        let _ = std::fs::write(path, serde_json::to_vec_pretty(&json).unwrap_or_default());
    }

    pub fn is_enabled(&self, p: Provider) -> bool {
        self.enabled[p as usize]
    }
}
