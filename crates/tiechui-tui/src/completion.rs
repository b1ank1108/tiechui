//! Shared presentation for command and resource completion.

use crate::{theme::Theme, wrap};
use ratatui_core::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_segmentation::UnicodeSegmentation;

pub struct Row {
    pub label: String,
    pub description: String,
    pub source: String,
    pub indices: Vec<usize>,
}

fn middle(text: &str, width: usize) -> String {
    if wrap::width(text) <= width {
        return text.into();
    }
    if width == 0 {
        return String::new();
    }
    let mut start = String::new();
    let mut end = String::new();
    for part in text.graphemes(true) {
        if wrap::width(&start) + wrap::width(part) > width / 3 {
            break;
        }
        start.push_str(part);
    }
    for part in text.graphemes(true).rev() {
        if wrap::width(&start) + wrap::width(&end) + wrap::width(part) + 1 > width {
            break;
        }
        end.insert_str(0, part);
    }
    format!("{start}…{end}")
}

#[allow(clippy::too_many_arguments)]
pub fn popup(
    title: &str,
    rows: &[Row],
    selected: usize,
    status: &str,
    hint: &str,
    width: usize,
    height: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    if height >= 4 {
        lines.push(wrap::fit(
            vec![Span::styled(format!("  {title}"), theme.dim)],
            width,
            true,
        ));
    }
    let count = height.saturating_sub(3).clamp(1, 8);
    let first = selected.saturating_sub(count - 1);
    let label_width = rows
        .iter()
        .map(|r| wrap::width(&r.label))
        .max()
        .unwrap_or(0)
        .min((width / 2).min(32));
    let source_width = if width >= 60 {
        rows.iter()
            .map(|r| wrap::width(&r.source))
            .max()
            .unwrap_or(0)
            .min(18)
    } else {
        0
    };
    for (index, row) in rows.iter().enumerate().skip(first).take(count) {
        let chosen = index == selected;
        let style = if chosen {
            theme.accent.add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let label = middle(&row.label, label_width);
        let mut spans = vec![Span::styled(if chosen { "> " } else { "  " }, style)];
        spans.extend(label.chars().enumerate().map(|(i, ch)| {
            Span::styled(
                ch.to_string(),
                if label == row.label && row.indices.contains(&i) {
                    style.add_modifier(Modifier::BOLD)
                } else {
                    style
                },
            )
        }));
        spans.push(Span::raw(
            " ".repeat(label_width.saturating_sub(wrap::width(&label)) + 2),
        ));
        let room = width.saturating_sub(label_width + source_width + 6);
        let description = middle(
            &row.description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            room,
        );
        spans.push(Span::styled(description.clone(), theme.dim));
        if source_width > 0 {
            spans.push(Span::raw(
                " ".repeat(room.saturating_sub(wrap::width(&description)) + 2),
            ));
            spans.push(Span::styled(middle(&row.source, source_width), theme.dim));
        }
        lines.push(wrap::fit(spans, width, false));
    }
    if rows.is_empty() {
        lines.push(wrap::fit(
            vec![Span::styled(format!("  {status}"), theme.dim)],
            width,
            true,
        ));
    }
    if height >= 2 && !rows.is_empty() {
        let source = rows
            .get(selected)
            .filter(|_| source_width == 0)
            .map(|row| row.source.as_str())
            .unwrap_or("");
        let status = if source.is_empty() {
            status.to_owned()
        } else {
            format!("{source} · {status}")
        };
        lines.push(wrap::fit(
            vec![Span::styled(format!("  {status}"), theme.dim)],
            width,
            true,
        ));
    }
    if height >= 3 {
        lines.push(wrap::fit(
            vec![Span::styled(format!("  {hint}"), theme.dim)],
            width,
            true,
        ));
    }
    lines.truncate(height);
    lines
}
