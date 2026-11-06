//! Syntax highlighting with syntect, two-face grammars and the vendored Catppuccin themes.

use std::io::Cursor;
use std::sync::OnceLock;

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Highlighter, Theme as SyntaxTheme, ThemeSet};
use syntect::parsing::{Scope, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::palette::{Rgb, best_color};
use crate::theme::Theme;

const MAX_BYTES: usize = 512 * 1024;
const MAX_LINES: usize = 10_000;
const MAX_LINE_BYTES: usize = 4 * 1024;

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

fn load(bytes: &'static [u8]) -> Option<SyntaxTheme> {
    ThemeSet::load_from_reader(&mut Cursor::new(bytes)).ok()
}

/// Catppuccin Mocha on dark terminals, Latte on light ones.
pub(crate) fn theme(light: bool) -> Option<&'static SyntaxTheme> {
    static MOCHA: OnceLock<Option<SyntaxTheme>> = OnceLock::new();
    static LATTE: OnceLock<Option<SyntaxTheme>> = OnceLock::new();
    if light {
        LATTE
            .get_or_init(|| load(include_bytes!("../assets/themes/Catppuccin Latte.tmTheme")))
            .as_ref()
    } else {
        MOCHA
            .get_or_init(|| load(include_bytes!("../assets/themes/Catppuccin Mocha.tmTheme")))
            .as_ref()
    }
}

/// The grammar for a fence language or a file path (extension or well-known file name).
fn syntax(language: &str) -> Option<&'static SyntaxReference> {
    let set = syntaxes();
    let name = language.rsplit('/').next().unwrap_or(language);
    set.find_syntax_by_token(name)
        .or_else(|| {
            name.rsplit_once('.')
                .and_then(|(_, extension)| set.find_syntax_by_extension(extension))
        })
        .or_else(|| set.find_syntax_by_extension(name))
}

/// The foreground of the first scope the theme colors, selector ancestry included.
pub fn scope_foreground(scopes: &[&str], selected: &Theme) -> Option<Rgb> {
    let highlighter = Highlighter::new(
        selected
            .syntax
            .as_deref()
            .or_else(|| theme(selected.palette.light()))?,
    );
    scopes.iter().find_map(|name| {
        let scope = Scope::new(name).ok()?;
        let fg = highlighter.style_mod_for_stack(&[scope]).foreground?;
        Some(Rgb(fg.r, fg.g, fg.b))
    })
}

/// Highlighted rows, one per source line, or `None` for unknown languages, oversized input,
/// or terminals without enough colors (callers keep their plain styling).
pub fn highlight(code: &str, language: &str, theme: &Theme) -> Option<Vec<Vec<Span<'static>>>> {
    if code.len() > MAX_BYTES
        || code.lines().count() > MAX_LINES
        || code.lines().any(|line| line.len() > MAX_LINE_BYTES)
    {
        return None;
    }
    let level = theme.palette.level;
    best_color(Rgb(0, 0, 0), level)?;
    let syntax = syntax(language)?;
    let mut lines = HighlightLines::new(
        syntax,
        theme
            .syntax
            .as_deref()
            .or_else(|| self::theme(theme.palette.light()))?,
    );
    let mut rows = Vec::new();
    for line in LinesWithEndings::from(code) {
        let ranges = lines.highlight_line(line, syntaxes()).ok()?;
        let row = ranges
            .into_iter()
            .filter_map(|(style, text)| {
                let text = text.trim_end_matches(['\n', '\r']);
                if text.is_empty() {
                    return None;
                }
                let mut span = Style::default();
                let fg = style.foreground;
                if let Some(color) = best_color(Rgb(fg.r, fg.g, fg.b), level) {
                    span = span.fg(color);
                }
                for (flag, modifier) in [
                    (FontStyle::BOLD, Modifier::BOLD),
                    (FontStyle::ITALIC, Modifier::ITALIC),
                    (FontStyle::UNDERLINE, Modifier::UNDERLINED),
                ] {
                    if style.font_style.contains(flag) {
                        span = span.add_modifier(modifier);
                    }
                }
                Some(Span::styled(text.to_owned(), span))
            })
            .collect();
        rows.push(row);
    }
    if code.ends_with('\n') || code.is_empty() {
        rows.push(Vec::new());
    }
    Some(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::ColorLevel;

    #[test]
    fn known_languages_highlight_and_limits_fall_back() {
        let theme = Theme::fixture(ColorLevel::TrueColor, false);
        let rows = highlight("fn main() {}\nlet x = 1;", "rs", &theme).unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].iter().any(|s| s.style.fg.is_some()));
        assert!(highlight("x", "src/lib.rs", &theme).is_some());
        assert!(highlight("[a]\nb = 1", "Cargo.toml", &theme).is_some());
        assert!(highlight("x", "no-such-language", &theme).is_none());
        assert!(highlight(&"x".repeat(MAX_LINE_BYTES + 1), "rs", &theme).is_none());
        assert!(highlight("x", "rs", &Theme::fixture(ColorLevel::Ansi16, false)).is_none());
        assert!(highlight("x", "rs", &Theme::fixture(ColorLevel::TrueColor, true)).is_some());
    }
}
