//! Shared presentation for command and resource completion.

use crate::{sheet, theme::Theme, wrap};
use ratatui_core::{
    style::{Modifier, Style},
    text::{Line, Span},
};

pub struct Row {
    pub label: String,
    pub description: String,
    pub source: String,
    pub indices: Vec<usize>,
}

/// The title row with `status` flush right, then up to eight rows: matched characters bold,
/// the description cut at its end and the source right-aligned on wide terminals.
pub fn popup(
    title: Vec<Span<'static>>,
    rows: &[Row],
    selected: usize,
    status: &str,
    width: usize,
    height: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    if height == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    if height >= 2 {
        let mut left = vec![Span::raw("  ")];
        left.extend(title);
        let room = width.saturating_sub(wrap::line_width(&Line::from(left.clone())) + 4);
        let right = wrap::fit(vec![Span::styled(status.to_owned(), theme.dim)], room, true);
        lines.push(sheet::split(left, right.spans, width));
    }
    let count = height.saturating_sub(lines.len()).clamp(1, 8);
    let first = selected.saturating_sub(count - 1);
    let label_width = rows
        .iter()
        .map(|r| wrap::width(&r.label))
        .max()
        .unwrap_or(0)
        .min((width / 2).min(32));
    let wide = width >= 60;
    let accent = theme.accent.add_modifier(Modifier::BOLD);
    for (index, row) in rows.iter().enumerate().skip(first).take(count) {
        let chosen = index == selected;
        let mut left = vec![
            Span::raw("  "),
            if chosen {
                Span::styled("› ", accent)
            } else {
                Span::raw("  ")
            },
        ];
        let label = wrap::fit(vec![Span::raw(row.label.clone())], label_width, true);
        let label: String = label.spans.iter().map(|s| s.content.as_ref()).collect();
        left.extend(label.chars().enumerate().map(|(i, ch)| {
            let style = if chosen {
                accent
            } else if row.indices.contains(&i) {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Span::styled(ch.to_string(), style)
        }));
        left.push(Span::raw(
            " ".repeat(label_width.saturating_sub(wrap::width(&label)) + 3),
        ));
        let description = row
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        left.push(Span::styled(description, theme.dim));
        let right = if wide {
            vec![Span::styled(row.source.clone(), theme.dim)]
        } else {
            Vec::new()
        };
        lines.push(sheet::split(left, right, width));
    }
    lines.truncate(height);
    lines
}
