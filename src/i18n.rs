//! Français / anglais. Les chaînes restent à côté de leur usage :
//! `tr("Hebdomadaire", "Weekly")`.

use std::sync::atomic::{AtomicU8, Ordering};

use windows::Win32::Globalization::GetUserDefaultUILanguage;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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

/// Langue de l'interface Windows : français si c'est du français, sinon anglais.
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

/// « 17 % » (espace insécable en français) / « 17% ».
pub fn percent(p: f32) -> String {
    match lang() {
        Lang::Fr => format!("{p:.0}\u{a0}%"),
        Lang::En => format!("{p:.0}%"),
    }
}
