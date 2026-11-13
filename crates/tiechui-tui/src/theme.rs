//! Semantic styles resolved once from the terminal palette.

use ratatui_core::style::{Color, Modifier, Style};

use crate::palette::{ColorLevel, Palette, Tone};

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub syntax: Option<std::sync::Arc<syntect::highlighting::Theme>>,
    pub palette: Palette,
    pub accent: Style,
    /// Live internal state: running phases, compaction, background activity.
    pub accent2: Style,
    pub success: Style,
    pub warning: Style,
    pub error: Style,
    pub secondary: Style,
    /// The `dim` role: the tertiary tone, or SGR dim without a palette.
    pub dim: Style,
    pub separator: Style,
    pub user_background: Option<Color>,
    pub selection_background: Option<Color>,
    pub table_border: Style,
}

impl Theme {
    pub fn new(palette: Palette) -> Self {
        let tone = |tone: Tone| {
            palette
                .tone(tone)
                .map_or_else(Style::default, |c| Style::default().fg(c))
        };
        // Neutral tones need a palette; ANSI-only terminals keep the SGR dim hierarchy.
        let neutral = |tone: Tone| match palette.tone(tone) {
            Some(color) => Style::default().fg(color),
            None => Style::default().add_modifier(Modifier::DIM),
        };
        Self {
            syntax: None,
            palette,
            accent: tone(Tone::Accent),
            accent2: tone(Tone::Accent2),
            success: tone(Tone::Success),
            warning: tone(Tone::Warning),
            error: tone(Tone::Error),
            secondary: neutral(Tone::Secondary),
            dim: neutral(Tone::Tertiary),
            separator: neutral(Tone::Separator),
            user_background: palette.surface(0.05, 0.04),
            selection_background: palette.wash(Tone::Accent, 0.14),
            table_border: palette.border().map_or_else(
                || Style::default().add_modifier(Modifier::DIM),
                |c| Style::default().fg(c),
            ),
        }
    }

    /// The fixed theme used by tests: dark truecolor terminal with known colors.
    pub fn fixture(level: ColorLevel, light: bool) -> Self {
        use crate::palette::Rgb;
        let (foreground, background) = if light {
            (Rgb(0x1f, 0x23, 0x28), Rgb(0xfa, 0xfa, 0xfa))
        } else {
            (Rgb(0xd0, 0xd0, 0xd0), Rgb(0x1e, 0x1e, 0x1e))
        };
        Self::new(Palette {
            level,
            foreground: Some(foreground),
            background: Some(background),
        })
    }

    /// Composer rules: the accent while the composer has focus.
    pub fn rule(&self, focused: bool) -> Style {
        if focused && self.accent.fg.is_some() {
            self.accent
        } else {
            self.separator
        }
    }

    /// The selected row of a list.
    pub fn selection(&self) -> Style {
        self.selection_background
            .map_or_else(Style::default, |bg| Style::default().bg(bg))
    }

    pub fn user(&self) -> Style {
        self.user_background
            .map_or_else(Style::default, |bg| Style::default().bg(bg))
    }
}
