//! Edit diffs in numbered display form: line numbers, signs, syntax foreground layered
//! over full-width add/remove tints.

use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::text::{Line, Span};

use crate::palette::ColorLevel;
use crate::syntax;
use crate::theme::Theme;
use crate::wrap::{self, wrap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sign {
    Insert,
    Delete,
    Context,
}

enum Row<'a> {
    Meta(&'a str),
    Omitted,
    Code {
        sign: Sign,
        number: &'a str,
        content: &'a str,
    },
}

fn parse(line: &str) -> Row<'_> {
    if ["@@", "diff ", "index ", "--- ", "+++ "]
        .iter()
        .any(|prefix| line.starts_with(prefix))
    {
        return Row::Meta(line);
    }
    let sign = match line.chars().next() {
        Some('+') => Sign::Insert,
        Some('-') => Sign::Delete,
        Some(' ') => Sign::Context,
        _ => return Row::Meta(line),
    };
    let rest = &line[1..];
    let digits = rest.trim_start();
    let count = digits.chars().take_while(char::is_ascii_digit).count();
    let (number, content) = if count > 0 {
        let after = &digits[count..];
        (&digits[..count], after.strip_prefix(' ').unwrap_or(after))
    } else {
        ("", rest.strip_prefix(' ').unwrap_or(rest))
    };
    if number.is_empty() && content.trim() == "..." {
        return Row::Omitted;
    }
    Row::Code {
        sign,
        number,
        content,
    }
}

/// Line tints; ANSI-16 terminals get colored text only.
fn background(sign: Sign, theme: &Theme) -> Option<Color> {
    let light = theme.palette.light();
    match (theme.palette.level, sign, light) {
        (_, Sign::Context, _) => None,
        (ColorLevel::TrueColor, Sign::Insert, false) => Some(Color::Rgb(33, 58, 43)),
        (ColorLevel::TrueColor, Sign::Delete, false) => Some(Color::Rgb(74, 34, 29)),
        (ColorLevel::TrueColor, Sign::Insert, true) => Some(Color::Rgb(218, 251, 225)),
        (ColorLevel::TrueColor, Sign::Delete, true) => Some(Color::Rgb(255, 235, 233)),
        (ColorLevel::Ansi256, Sign::Insert, false) => Some(Color::Indexed(22)),
        (ColorLevel::Ansi256, Sign::Delete, false) => Some(Color::Indexed(52)),
        (ColorLevel::Ansi256, Sign::Insert, true) => Some(Color::Indexed(194)),
        (ColorLevel::Ansi256, Sign::Delete, true) => Some(Color::Indexed(224)),
        _ => None,
    }
}

/// Light terminals give the gutter an opaque, stronger tint so numbers stay readable.
fn gutter(sign: Sign, theme: &Theme) -> Style {
    if !theme.palette.light() || sign == Sign::Context {
        return Style::default().add_modifier(Modifier::DIM);
    }
    let insert = sign == Sign::Insert;
    match theme.palette.level {
        ColorLevel::TrueColor => Style::default().fg(Color::Rgb(31, 35, 40)).bg(if insert {
            Color::Rgb(172, 238, 187)
        } else {
            Color::Rgb(255, 206, 203)
        }),
        ColorLevel::Ansi256 => Style::default()
            .fg(Color::Indexed(236))
            .bg(Color::Indexed(if insert { 157 } else { 217 })),
        _ => Style::default().fg(Color::Black),
    }
}

fn sign_color(sign: Sign) -> Style {
    match sign {
        Sign::Insert => Style::default().fg(Color::Green),
        Sign::Delete => Style::default().fg(Color::Red),
        Sign::Context => Style::default(),
    }
}

/// Rows of one contiguous block; the old and new sides are highlighted separately so each
/// keeps its own parser state.
fn block(
    rows: &[(Sign, &str, &str)],
    language: Option<&str>,
    number_width: usize,
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
) {
    let side = |skip: Sign| {
        let code: Vec<&str> = rows
            .iter()
            .filter(|(sign, ..)| *sign != skip)
            .map(|(_, _, content)| *content)
            .collect();
        language.and_then(|language| syntax::highlight(&code.join("\n"), language, theme))
    };
    let (old, new) = (side(Sign::Insert), side(Sign::Delete));
    let (mut old_at, mut new_at) = (0, 0);
    let content_width = width.saturating_sub(number_width + 3).max(1);
    for (sign, number, content) in rows {
        let highlighted = match sign {
            Sign::Delete => {
                old_at += 1;
                old.as_ref().and_then(|rows| rows.get(old_at - 1))
            }
            Sign::Insert => {
                new_at += 1;
                new.as_ref().and_then(|rows| rows.get(new_at - 1))
            }
            Sign::Context => {
                old_at += 1;
                new_at += 1;
                new.as_ref().and_then(|rows| rows.get(new_at - 1))
            }
        };
        let bg = background(*sign, theme);
        let dark_colored = !theme.palette.light() && *sign != Sign::Context;
        let base = match bg {
            Some(bg) => Style::default().bg(bg),
            None => Style::default(),
        };
        let spans: Vec<Span<'static>> = match highlighted {
            Some(spans) => spans
                .iter()
                .map(|span| Span::styled(span.content.clone(), base.patch(span.style)))
                .collect(),
            None if dark_colored || bg.is_none() => {
                vec![Span::styled(
                    (*content).to_owned(),
                    base.patch(sign_color(*sign)),
                )]
            }
            None => vec![Span::styled((*content).to_owned(), base)],
        };
        let mark = match sign {
            Sign::Insert => "+",
            Sign::Delete => "-",
            Sign::Context => " ",
        };
        let sign_style = if theme.palette.light() {
            sign_color(*sign)
        } else {
            base.patch(sign_color(*sign))
        };
        let first = [
            Span::styled(format!("{number:>number_width$} "), gutter(*sign, theme)),
            Span::styled(format!("{mark} "), base.patch(sign_style)),
        ];
        let rest = [
            Span::styled(" ".repeat(number_width + 1), gutter(*sign, theme)),
            Span::styled("  ", base),
        ];
        for mut line in wrap(&spans, content_width + number_width + 3, &first, &rest) {
            let used = wrap::line_width(&line);
            if let Some(bg) = bg
                && used < width
            {
                line.spans.push(Span::styled(
                    " ".repeat(width - used),
                    Style::default().bg(bg),
                ));
            }
            out.push(line);
        }
    }
}

/// Renders `text` at `width`; `path` picks the grammar.
pub fn lines(text: &str, path: Option<&str>, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let rows: Vec<Row<'_>> = text.split('\n').map(parse).collect();
    let number_width = rows
        .iter()
        .map(|row| match row {
            Row::Code { number, .. } => number.len(),
            _ => 0,
        })
        .max()
        .unwrap_or(0)
        .max(1);
    let mut out = Vec::new();
    let mut chunk: Vec<(Sign, &str, &str)> = Vec::new();
    for row in &rows {
        match row {
            Row::Code {
                sign,
                number,
                content,
            } => chunk.push((*sign, number, content)),
            Row::Omitted | Row::Meta(_) => {
                block(&chunk, path, number_width, width, theme, &mut out);
                chunk.clear();
                let text = match row {
                    Row::Meta(text) => (*text).to_owned(),
                    _ => format!("{:>number_width$} ⋮", ""),
                };
                out.extend(wrap(&[Span::styled(text, theme.dim)], width, &[], &[]));
            }
        }
    }
    block(&chunk, path, number_width, width, theme, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "  1 fn main() {\n- 2     old();\n+ 2     new();\n    ...\n 10 }";

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn rows_keep_numbers_signs_and_omissions() {
        let theme = Theme::fixture(ColorLevel::TrueColor, false);
        assert_eq!(
            text(&lines(DIFF, Some("src/main.rs"), 30, &theme)),
            [
                " 1   fn main() {",
                " 2 -     old();",
                " 2 +     new();",
                "   ⋮",
                "10   }"
            ]
        );
    }

    #[test]
    fn changed_rows_fill_the_width_with_their_tint_under_syntax_colors() {
        let theme = Theme::fixture(ColorLevel::TrueColor, false);
        let rendered = lines(DIFF, Some("src/main.rs"), 30, &theme);
        let added = &rendered[2];
        assert_eq!(wrap::line_width(added), 30);
        assert!(
            added.spans[1..]
                .iter()
                .all(|s| s.style.bg == Some(Color::Rgb(33, 58, 43)))
        );
        assert!(
            added
                .spans
                .iter()
                .any(|s| matches!(s.style.fg, Some(Color::Rgb(..))) && s.content.contains("new"))
        );
        assert!(rendered[0].spans.iter().all(|s| s.style.bg.is_none()));
        let plain = lines(
            DIFF,
            Some("src/main.rs"),
            30,
            &Theme::fixture(ColorLevel::Ansi16, false),
        );
        assert!(plain[2].spans.iter().all(|s| s.style.bg.is_none()));
        assert_eq!(plain[2].spans[2].style.fg, Some(Color::Green));
    }
}
