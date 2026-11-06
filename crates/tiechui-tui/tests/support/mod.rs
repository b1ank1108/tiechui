//! Shared test helpers.

use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::text::Line;

pub fn color(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        Color::Indexed(index) => format!("@{index}"),
        other => format!("{other:?}"),
    }
}

/// Text with style annotations, so attribute and color regressions show in snapshots.
pub fn dump(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| {
                    let style = span.style;
                    let mut tags = Vec::new();
                    for (modifier, tag) in [
                        (Modifier::BOLD, "b"),
                        (Modifier::DIM, "d"),
                        (Modifier::ITALIC, "i"),
                        (Modifier::UNDERLINED, "u"),
                        (Modifier::CROSSED_OUT, "s"),
                    ] {
                        if style.add_modifier.contains(modifier) {
                            tags.push(tag.to_owned());
                        }
                    }
                    if let Some(fg) = style.fg {
                        tags.push(format!("fg{}", color(fg)));
                    }
                    if let Some(bg) = style.bg {
                        tags.push(format!("bg{}", color(bg)));
                    }
                    if style == Style::default() || tags.is_empty() {
                        format!("[-]{}", span.content)
                    } else {
                        format!("[{}]{}", tags.join(","), span.content)
                    }
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
