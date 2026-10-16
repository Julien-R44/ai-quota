//! French / English. Strings live next to where they are used: `tr("Hebdomadaire", "Weekly")`.

use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};
use windows::Win32::Globalization::GetUserDefaultUILanguage;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Fr,
    En,
}

static LANG: AtomicU8 = AtomicU8::new(0);

pub fn lang() -> Lang {
    if LANG.load(Ordering::Relaxed) == 1 { Lang::En } else { Lang::Fr }
}

pub fn set_lang(l: Lang) {
    LANG.store(if l == Lang::En { 1 } else { 0 }, Ordering::Relaxed);
}

/// French if the Windows UI language is French, English otherwise.
pub fn system_lang() -> Lang {
    const LANG_FRENCH: u16 = 0x0C;
    if unsafe { GetUserDefaultUILanguage() } & 0x3FF == LANG_FRENCH { Lang::Fr } else { Lang::En }
}

pub fn tr(fr: &'static str, en: &'static str) -> &'static str {
    match lang() {
        Lang::Fr => fr,
        Lang::En => en,
    }
}

/// "17 %" (no-break space in French) / "17%".
pub fn percent(p: f32) -> String {
    match lang() {
        Lang::Fr => format!("{p:.0}\u{a0}%"),
        Lang::En => format!("{p:.0}%"),
    }
}

/// The language is process-global and tests run in parallel: every test that formats text holds
/// this guard, which serializes them and restores the previous language on drop.
#[cfg(test)]
pub struct LangGuard {
    previous: Lang,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl LangGuard {
    pub fn new(l: Lang) -> Self {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = lang();
        set_lang(l);
        LangGuard { previous, _lock: lock }
    }

    pub fn set(&self, l: Lang) {
        set_lang(l);
    }
}

#[cfg(test)]
impl Drop for LangGuard {
    fn drop(&mut self) {
        set_lang(self.previous);
    }
}
