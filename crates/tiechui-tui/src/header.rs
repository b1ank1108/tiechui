//! The session header: the pixel logo beside the session details, framed in a rounded box.

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};

use crate::palette::Tone;
use crate::theme::Theme;
use crate::wrap;

/// What the header shows; fixed when it is printed.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub cwd: String,
}

/// A cat's face with the eyes and nose cut out, drawn with half blocks in four rows.
const CAT: [&str; 8] = [
    "..X.......X..",
    ".XXX.....XXX.",
    ".XXXXXXXXXXX.",
    "XXXXXXXXXXXXX",
    "XXXXXXXXXXXXX",
    "XX...XXX...XX",
    "XXXXXX.XXXXXX",
    ".XXXXXXXXXXX.",
];

const SHORTCUTS: [(&str, &str); 4] = [
    ("/", "commands"),
    ("@", "files"),
    ("ctrl+o", "details"),
    ("/resume", "past sessions"),
];

fn logo(theme: &Theme) -> Vec<Vec<Span<'static>>> {
    let on = |x: usize, y: usize| CAT[y].as_bytes()[x] == b'X';
    let columns = CAT[0].len();
    let tone = |tone| {
        theme
            .palette
            .tone(tone)
            .map_or_else(Style::default, |color| Style::default().fg(color))
    };
    let (left, right) = (tone(Tone::Accent), tone(Tone::Accent2));
    (0..4)
        .map(|row| {
            (0..columns)
                .map(|x| {
                    let glyph = match (on(x, row * 2), on(x, row * 2 + 1)) {
                        (true, true) => "█",
                        (true, false) => "▀",
                        (false, true) => "▄",
                        (false, false) => return Span::raw(" "),
                    };
                    Span::styled(glyph, if x * 2 < columns { left } else { right })
                })
                .collect()
        })
        .collect()
}

pub fn render(header: &Header, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    if width < 5 {
        return vec![wrap::fit(vec![Span::styled("chui", bold)], width, false)];
    }
    let model = header.model.as_deref().unwrap_or("no model");
    let info = [
        vec![
            Span::styled("Tiechui", bold),
            Span::styled(concat!("  v", env!("CARGO_PKG_VERSION")), theme.dim),
        ],
        vec![Span::styled(
            match &header.thinking {
                Some(level) => format!("{model} · {level}"),
                None => model.to_owned(),
            },
            theme.secondary,
        )],
        vec![Span::styled(header.cwd.clone(), theme.dim)],
    ];
    let size = width.min(78);
    let inside = size - 4;
    // The logo and shortcut row need room beside the session details.
    let rows: Vec<Vec<Span<'static>>> = if size < 60 {
        info.into()
    } else {
        let mut info = info.into_iter();
        let mut rows = vec![Vec::new()];
        for mut row in logo(theme) {
            row.push(Span::raw("    "));
            row.extend(info.next().unwrap_or_default());
            rows.push(row);
        }
        rows.push(Vec::new());
        let mut shortcuts = Vec::new();
        for (index, (key, label)) in SHORTCUTS.into_iter().enumerate() {
            if index > 0 {
                shortcuts.push(Span::raw("   "));
            }
            shortcuts.push(Span::styled(key, theme.dim));
            shortcuts.push(Span::raw(format!(" {label}")));
        }
        rows.push(shortcuts);
        rows.push(Vec::new());
        rows
    };
    let border = |text: String| Span::styled(text, theme.separator);
    let mut lines = vec![Line::from(border(format!("╭{}╮", "─".repeat(size - 2))))];
    for row in rows {
        let mut content = wrap::fit(row, inside, true);
        let pad = inside.saturating_sub(wrap::line_width(&content));
        content.spans.insert(0, border("│ ".into()));
        content.spans.push(Span::raw(" ".repeat(pad)));
        content.spans.push(border(" │".into()));
        lines.push(content);
    }
    lines.push(Line::from(border(format!("╰{}╯", "─".repeat(size - 2)))));
    lines
}
