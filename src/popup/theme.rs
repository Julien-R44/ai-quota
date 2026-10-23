use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;

use crate::model::Provider;
use crate::settings::ThemePref;
use crate::{app, sys};

pub(super) fn rgb(hex: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xFF) as f32 / 255.0,
        g: ((hex >> 8) & 0xFF) as f32 / 255.0,
        b: (hex & 0xFF) as f32 / 255.0,
        a,
    }
}

pub(super) struct Theme {
    pub(super) dark: bool,
    pub(super) text1: D2D1_COLOR_F,
    pub(super) text2: D2D1_COLOR_F,
    pub(super) text3: D2D1_COLOR_F,
    pub(super) card: D2D1_COLOR_F,
    pub(super) card_border: D2D1_COLOR_F,
    pub(super) track: D2D1_COLOR_F,
    pub(super) button_hover: D2D1_COLOR_F,
    pub(super) pace: D2D1_COLOR_F,
    pub(super) warn: D2D1_COLOR_F,
    pub(super) crit: D2D1_COLOR_F,
}

impl Theme {
    pub(super) fn current() -> Self {
        let dark = match app().settings.borrow().theme {
            ThemePref::Auto => sys::apps_dark(),
            ThemePref::Light => false,
            ThemePref::Dark => true,
        };
        if dark {
            Theme {
                dark: true,
                text1: rgb(0xFFFFFF, 0.94),
                text2: rgb(0xFFFFFF, 0.62),
                text3: rgb(0xFFFFFF, 0.42),
                card: rgb(0xFFFFFF, 0.045),
                card_border: rgb(0xFFFFFF, 0.07),
                track: rgb(0xFFFFFF, 0.10),
                button_hover: rgb(0xFFFFFF, 0.07),
                pace: rgb(0xFFFFFF, 0.55),
                warn: rgb(0xF2B035, 1.0),
                crit: rgb(0xFF6B6B, 1.0),
            }
        } else {
            Theme {
                dark: false,
                text1: rgb(0x000000, 0.90),
                text2: rgb(0x000000, 0.60),
                text3: rgb(0x000000, 0.44),
                card: rgb(0xFFFFFF, 0.55),
                card_border: rgb(0x000000, 0.06),
                track: rgb(0x000000, 0.08),
                button_hover: rgb(0x000000, 0.05),
                pace: rgb(0x000000, 0.45),
                warn: rgb(0xB87800, 1.0),
                crit: rgb(0xD13438, 1.0),
            }
        }
    }

    /// (main color, text color of the plan pill)
    pub(super) fn brand(&self, p: Provider) -> (D2D1_COLOR_F, D2D1_COLOR_F) {
        match (p, self.dark) {
            (Provider::Claude, true) => (rgb(0xD97757, 1.0), rgb(0xF2A386, 1.0)),
            (Provider::Claude, false) => (rgb(0xD97757, 1.0), rgb(0xB4532F, 1.0)),
            (Provider::Codex, true) => (rgb(0x8C9EFF, 1.0), rgb(0xB3BFFF, 1.0)),
            (Provider::Codex, false) => (rgb(0x4A5CE0, 1.0), rgb(0x3B4CCF, 1.0)),
            // Amp identity: very dark green and cream (#DFDFC1).
            (Provider::Amp, true) => (rgb(0xDFDFC1, 1.0), rgb(0xDFDFC1, 1.0)),
            (Provider::Amp, false) => (rgb(0x2F4A2C, 1.0), rgb(0x2C3A2A, 1.0)),
            // OpenCode identity: the black and white of its logo.
            (Provider::Opencode, true) => (rgb(0xE4E4E7, 1.0), rgb(0xE4E4E7, 1.0)),
            (Provider::Opencode, false) => (rgb(0x18181B, 1.0), rgb(0x3F3F46, 1.0)),
        }
    }

    /// App accent (toggles, selection): the purple of the icon. The Windows accent can be grey, which makes an
    /// enabled toggle indistinguishable from a disabled one.
    pub(super) fn accent(&self) -> D2D1_COLOR_F {
        if self.dark { rgb(0xA48BFF, 1.0) } else { rgb(0x6B4EFF, 1.0) }
    }

    /// Text drawn on the accent color.
    pub(super) fn on_accent(&self) -> D2D1_COLOR_F {
        if self.dark { rgb(0x000000, 0.9) } else { rgb(0xFFFFFF, 1.0) }
    }

    pub(super) fn level(&self, percent: f32, normal: D2D1_COLOR_F) -> D2D1_COLOR_F {
        match percent {
            p if p >= 95.0 => self.crit,
            p if p >= 80.0 => self.warn,
            _ => normal,
        }
    }
}
