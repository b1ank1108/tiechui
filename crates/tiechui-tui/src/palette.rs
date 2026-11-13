//! Terminal color capability, the Mist palette and its blending rules.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use ratatui_core::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorLevel {
    TrueColor,
    Ansi256,
    Ansi16,
    Unknown,
}

/// The color level, with truecolor capability as the mode input.
pub fn detect_color_level(env: &dyn Fn(&str) -> Option<String>) -> ColorLevel {
    let force = env("FORCE_COLOR");
    let truthy = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.is_empty());
    let term = env("TERM").unwrap_or_default();
    if force.as_deref() == Some("0")
        || (env("NO_COLOR").is_some() && force.is_none())
        || (term == "dumb" && force.is_none())
    {
        return ColorLevel::Unknown;
    }
    match force.as_deref() {
        Some("1") => return ColorLevel::Ansi16,
        Some("2") => return ColorLevel::Ansi256,
        Some("3") => return ColorLevel::TrueColor,
        _ => {}
    }
    if (truthy(&env("WT_SESSION")) && !truthy(&force)) || truecolor(env) {
        return ColorLevel::TrueColor;
    }
    let term_hint = ["256", "direct", "kitty", "ghostty", "tmux", "screen"]
        .iter()
        .any(|hint| term.contains(hint));
    if truthy(&force) || truthy(&env("COLORTERM")) || term_hint || truthy(&env("TERM_PROGRAM")) {
        return ColorLevel::Ansi256;
    }
    if term.is_empty() {
        ColorLevel::Unknown
    } else {
        ColorLevel::Ansi16
    }
}

fn truecolor(env: &dyn Fn(&str) -> Option<String>) -> bool {
    let lower = |key: &str| env(key).unwrap_or_default().to_lowercase();
    let (program, term, colorterm) = (lower("TERM_PROGRAM"), lower("TERM"), lower("COLORTERM"));
    let hint = colorterm == "truecolor" || colorterm == "24bit" || term.ends_with("-direct");
    let set = |key: &str| env(key).is_some_and(|v| !v.is_empty());
    if set("TMUX") || term.starts_with("tmux") || term.starts_with("screen") || program == "herdr" {
        return hint;
    }
    hint || set("KITTY_WINDOW_ID")
        || set("GHOSTTY_RESOURCES_DIR")
        || set("WEZTERM_PANE")
        || set("WARP_SESSION_ID")
        || set("WARP_TERMINAL_SESSION_UUID")
        || set("ITERM_SESSION_ID")
        || set("WT_SESSION")
        || term.contains("ghostty")
        || lower("TERMINAL_EMULATOR") == "jetbrains-jediterm"
        || [
            "kitty",
            "ghostty",
            "wezterm",
            "warpterminal",
            "iterm.app",
            "alacritty",
            "vscode",
            "zed",
        ]
        .contains(&program.as_str())
}

/// Computed in f32 so truncation matches an integer conversion.
pub fn is_light(Rgb(r, g, b): Rgb) -> bool {
    0.299_f32 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b) > 128.0
}

pub fn blend(top: Rgb, base: Rgb, alpha: f32) -> Rgb {
    let channel = |x: u8, y: u8| (f32::from(x) * alpha + f32::from(y) * (1.0 - alpha)) as u8;
    Rgb(
        channel(top.0, base.0),
        channel(top.1, base.1),
        channel(top.2, base.2),
    )
}

fn lab(Rgb(r, g, b): Rgb) -> [f64; 3] {
    let linear = |v: u8| {
        let n = f64::from(v) / 255.0;
        if n <= 0.04045 {
            n / 12.92
        } else {
            ((n + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (linear(r), linear(g), linear(b));
    let pivot = |n: f64| {
        if n > 0.008856 {
            n.cbrt()
        } else {
            7.787 * n + 16.0 / 116.0
        }
    };
    let x = pivot((r * 0.4124 + g * 0.3576 + b * 0.1805) / 0.95047);
    let y = pivot(r * 0.2126 + g * 0.7152 + b * 0.0722);
    let z = pivot((r * 0.0193 + g * 0.1192 + b * 0.9505) / 1.08883);
    [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
}

/// Indices 16–255 only: 0–15 belong to the terminal's own theme.
static FIXED: LazyLock<Vec<(u8, [f64; 3])>> = LazyLock::new(|| {
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];
    (16u8..=255)
        .map(|index| {
            let n = usize::from(index - 16);
            let rgb = if index >= 232 {
                let gray = 8 + (index - 232) * 10;
                Rgb(gray, gray, gray)
            } else {
                Rgb(CUBE[n / 36], CUBE[(n / 6) % 6], CUBE[n % 6])
            };
            (index, lab(rgb))
        })
        .collect()
});
static NEAREST: LazyLock<Mutex<HashMap<Rgb, u8>>> = LazyLock::new(Mutex::default);

fn nearest(rgb: Rgb) -> u8 {
    if let Some(index) = NEAREST
        .lock()
        .ok()
        .and_then(|cache| cache.get(&rgb).copied())
    {
        return index;
    }
    let target = lab(rgb);
    let distance = |candidate: &[f64; 3]| {
        candidate
            .iter()
            .zip(target)
            .map(|(a, b)| (a - b).powi(2))
            .sum::<f64>()
    };
    let index = FIXED
        .iter()
        .min_by(|a, b| distance(&a.1).total_cmp(&distance(&b.1)))
        .map_or(16, |candidate| candidate.0);
    if let Ok(mut cache) = NEAREST.lock() {
        if cache.len() >= 2048 {
            cache.clear();
        }
        cache.insert(rgb, index);
    }
    index
}

pub fn best_color(rgb: Rgb, level: ColorLevel) -> Option<Color> {
    match level {
        ColorLevel::TrueColor => Some(Color::Rgb(rgb.0, rgb.1, rgb.2)),
        ColorLevel::Ansi256 => Some(Color::Indexed(nearest(rgb))),
        ColorLevel::Ansi16 | ColorLevel::Unknown => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Text,
    Secondary,
    Tertiary,
    Separator,
    Accent,
    Accent2,
    Success,
    Warning,
    Error,
}

fn hex(value: u32) -> Rgb {
    Rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

pub fn mist_rgb(tone: Tone, light: bool) -> Rgb {
    hex(match (tone, light) {
        (Tone::Text, false) => 0xd6d9df,
        (Tone::Secondary, false) => 0x9aa0aa,
        (Tone::Tertiary, false) => 0x666c77,
        (Tone::Separator, false) => 0x33363d,
        (Tone::Accent, false) => 0x8fa8c9,
        (Tone::Accent2, false) => 0xa99ccf,
        (Tone::Success, false) => 0x8fb996,
        (Tone::Warning, false) => 0xd6b07a,
        (Tone::Error, false) => 0xd48a8a,
        (Tone::Text, true) => 0x2b2f36,
        (Tone::Secondary, true) => 0x5f6672,
        (Tone::Tertiary, true) => 0x9aa1ac,
        (Tone::Separator, true) => 0xdde0e5,
        (Tone::Accent, true) => 0x4f6f9a,
        (Tone::Accent2, true) => 0x7a68a8,
        (Tone::Success, true) => 0x4f8a5b,
        (Tone::Warning, true) => 0xa87a2f,
        (Tone::Error, true) => 0xb05656,
    })
}

/// Terminals without a palette keep their own ANSI slots; neutral tones keep the default foreground.
fn ansi_fallback(tone: Tone) -> Option<Color> {
    match tone {
        Tone::Accent => Some(Color::Indexed(6)),
        Tone::Accent2 => Some(Color::Indexed(5)),
        Tone::Success => Some(Color::Indexed(2)),
        Tone::Warning => Some(Color::Indexed(3)),
        Tone::Error => Some(Color::Indexed(1)),
        _ => None,
    }
}

/// What the terminal reported; `None` means unknown, which is not the same as black.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub level: ColorLevel,
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
}

impl Palette {
    pub fn light(&self) -> bool {
        self.background.is_some_and(is_light)
    }

    pub fn tone(&self, tone: Tone) -> Option<Color> {
        best_color(mist_rgb(tone, self.light()), self.level).or_else(|| ansi_fallback(tone))
    }

    /// A soft surface: white over dark backgrounds, black over light ones.
    pub fn surface(&self, dark: f32, light: f32) -> Option<Color> {
        let background = self.background?;
        let bright = is_light(background);
        let top = if bright { 0 } else { 255 };
        best_color(
            blend(
                Rgb(top, top, top),
                background,
                if bright { light } else { dark },
            ),
            self.level,
        )
    }

    /// `alpha` of a tone over the background.
    pub fn wash(&self, tone: Tone, alpha: f32) -> Option<Color> {
        best_color(
            blend(mist_rgb(tone, self.light()), self.background?, alpha),
            self.level,
        )
    }

    /// Table borders: 20% foreground over background.
    pub fn border(&self) -> Option<Color> {
        best_color(blend(self.foreground?, self.background?, 0.2), self.level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(pairs: &[(&str, &str)]) -> ColorLevel {
        detect_color_level(&|key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        })
    }

    #[test]
    fn capability_fallback_and_cie76_quantization_keep_fixed_surfaces_neutral() {
        assert_eq!(
            level(&[("FORCE_COLOR", "1"), ("COLORTERM", "truecolor")]),
            ColorLevel::Ansi16
        );
        assert_eq!(
            level(&[("FORCE_COLOR", "0"), ("COLORTERM", "truecolor")]),
            ColorLevel::Unknown
        );
        assert_eq!(
            level(&[("NO_COLOR", ""), ("COLORTERM", "truecolor")]),
            ColorLevel::Unknown
        );
        assert_eq!(level(&[("TERM", "xterm-256color")]), ColorLevel::Ansi256);
        assert_eq!(level(&[("TERM", "vt100")]), ColorLevel::Ansi16);
        assert_eq!(level(&[]), ColorLevel::Unknown);
        assert_eq!(level(&[("COLORTERM", "truecolor")]), ColorLevel::TrueColor);
        assert_eq!(
            best_color(Rgb(48, 52, 65), ColorLevel::Ansi256),
            Some(Color::Indexed(236))
        );
        assert_eq!(
            best_color(Rgb(240, 235, 225), ColorLevel::Ansi256),
            Some(Color::Indexed(255))
        );
        assert_eq!(
            best_color(Rgb(0, 0, 0), ColorLevel::Ansi256),
            Some(Color::Indexed(16))
        );
        assert_eq!(best_color(Rgb(48, 52, 65), ColorLevel::Ansi16), None);
        assert_eq!(
            blend(Rgb(255, 255, 255), Rgb(20, 25, 40), 0.12),
            Rgb(48, 52, 65)
        );
    }

    #[test]
    fn missing_background_stays_transparent_while_reported_black_is_real() {
        let mut palette = Palette {
            level: ColorLevel::TrueColor,
            foreground: Some(Rgb(220, 225, 235)),
            background: None,
        };
        assert_eq!(palette.surface(0.05, 0.04), None);
        palette.background = Some(Rgb(0, 0, 0));
        assert_eq!(palette.surface(0.05, 0.04), Some(Color::Rgb(12, 12, 12)));
    }

    #[test]
    fn mist_tones_follow_background_and_fall_back_to_ansi_slots() {
        let dark = Palette {
            level: ColorLevel::TrueColor,
            foreground: None,
            background: Some(Rgb(20, 25, 40)),
        };
        assert_eq!(dark.tone(Tone::Accent), Some(Color::Rgb(0x8f, 0xa8, 0xc9)));
        let light = Palette {
            background: Some(Rgb(250, 245, 235)),
            ..dark
        };
        assert_eq!(light.tone(Tone::Accent), Some(Color::Rgb(0x4f, 0x6f, 0x9a)));
        let plain = Palette {
            level: ColorLevel::Ansi16,
            ..dark
        };
        assert_eq!(plain.tone(Tone::Error), Some(Color::Indexed(1)));
        assert_eq!(plain.tone(Tone::Tertiary), None);
    }
}
