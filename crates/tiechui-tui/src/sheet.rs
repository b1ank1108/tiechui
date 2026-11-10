//! The shared panel shape: a titled rule, the body, a plain rule, then the key hints on the
//! status line's row.

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};

use crate::theme::Theme;
use crate::{syntax, wrap};

type Spans = Vec<Span<'static>>;

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| wrap::width(&span.content)).sum()
}

/// `── title ─── meta ──`, trying each title with each meta, then each title alone; the last
/// title is cut when nothing fits. Without titles, a plain rule.
pub fn rule(tone: Style, titles: &[Spans], metas: &[Spans], width: usize) -> Line<'static> {
    let dashes = |n: usize| Span::styled("─".repeat(n), tone);
    let Some(shortest) = titles.last() else {
        return Line::from(dashes(width));
    };
    let left = |title: &Spans| {
        let mut spans = vec![Span::styled("── ", tone)];
        spans.extend(title.iter().cloned());
        spans.push(Span::raw(" "));
        spans
    };
    let candidates = titles
        .iter()
        .flat_map(|title| metas.iter().map(move |meta| (title, Some(meta))))
        .chain(titles.iter().map(|title| (title, None)));
    for (title, meta) in candidates {
        let mut spans = left(title);
        let right = match meta {
            Some(meta) => {
                let mut right = vec![Span::raw(" ")];
                right.extend(meta.iter().cloned());
                right.push(Span::styled(" ──", tone));
                right
            }
            None => vec![Span::styled("──", tone)],
        };
        let used = spans_width(&spans) + spans_width(&right);
        if used + 3 <= width {
            spans.push(dashes(width - used));
            spans.extend(right);
            return Line::from(spans);
        }
    }
    let mut line = wrap::fit(left(shortest), width.saturating_sub(3), true);
    let used = wrap::line_width(&line);
    line.spans.push(dashes(width.saturating_sub(used)));
    line
}

/// `left` with `right` flush right; `right` goes when it would leave `left` under 8 columns.
pub fn split(left: Spans, right: Spans, width: usize) -> Line<'static> {
    let room = spans_width(&right);
    if room == 0 || room + 10 > width {
        return wrap::fit(left, width, true);
    }
    let mut line = wrap::fit(left, width - room - 2, true);
    let used = wrap::line_width(&line);
    line.spans.push(Span::raw(" ".repeat(width - room - used)));
    line.spans.extend(right);
    line
}

/// `key action` pairs with the position flush right. Narrow rows drop pairs from the second
/// last, so the primary action and the way out stay, then the position.
pub fn hints(
    theme: &Theme,
    pairs: &[(&str, &str)],
    position: Option<&str>,
    width: usize,
) -> Line<'static> {
    let build = |pairs: &[(&str, &str)]| -> Spans {
        let mut spans = vec![Span::raw("  ")];
        for (index, (key, action)) in pairs.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", theme.dim));
            }
            spans.push(Span::styled(key.to_string(), theme.secondary));
            if !action.is_empty() {
                spans.push(Span::styled(format!(" {action}"), theme.dim));
            }
        }
        spans
    };
    let mut pairs = pairs.to_vec();
    let mut position = position;
    let need = |pairs: &[(&str, &str)], position: Option<&str>| {
        spans_width(&build(pairs)) + position.map_or(0, |p| wrap::width(p) + 3)
    };
    while need(&pairs, position) > width && pairs.len() > 2 {
        pairs.remove(pairs.len() - 2);
    }
    if need(&pairs, position) > width {
        position = None;
    }
    while need(&pairs, position) > width && pairs.len() > 1 {
        pairs.remove(pairs.len() - 2);
    }
    let right = position.map_or_else(Vec::new, |p| vec![Span::styled(p.to_owned(), theme.dim)]);
    split(build(&pairs), right, width)
}

/// A panel in the composer's place: the titled rule, `body`, a plain rule and the hints row.
pub fn frame(
    tone: Style,
    title: Line<'static>,
    body: Vec<Line<'static>>,
    hints: Line<'static>,
    width: usize,
) -> Vec<Line<'static>> {
    let mut lines = vec![title];
    lines.extend(body);
    lines.push(rule(tone, &[], &[], width));
    lines.push(hints);
    lines
}

/// `$ command`, highlighted as shell, wrapped under its first character.
pub fn command(command: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let rows = syntax::highlight(command, "bash", theme).unwrap_or_else(|| {
        command
            .split('\n')
            .map(|line| vec![Span::raw(line.to_owned())])
            .collect()
    });
    rows.into_iter()
        .enumerate()
        .flat_map(|(index, row)| {
            let mut spans = vec![Span::styled(
                if index == 0 { "$ " } else { "  " },
                theme.dim,
            )];
            spans.extend(row);
            wrap::wrap(&spans, width, &[Span::raw("  ")], &[Span::raw("    ")])
        })
        .collect()
}

/// Bold text for a rule's title.
pub fn title(text: impl Into<String>) -> Spans {
    vec![Span::styled(
        text.into(),
        Style::default().add_modifier(Modifier::BOLD),
    )]
}

pub struct Row {
    pub label: Spans,
    pub detail: Spans,
    pub current: bool,
}

impl Row {
    pub fn new(label: impl Into<String>, detail: impl Into<String>, theme: &Theme) -> Self {
        let detail = detail.into();
        Self {
            label: vec![Span::raw(label.into())],
            detail: if detail.is_empty() {
                Vec::new()
            } else {
                vec![Span::styled(detail, theme.dim)]
            },
            current: false,
        }
    }
}

/// Option rows: `› N. label ✓   detail`, numbered when there are at most nine.
pub struct List {
    numbered: bool,
    /// Label cells before the detail column.
    column: usize,
    /// Details wrap in their column, or under the label when narrow; otherwise they are cut.
    wrap: bool,
}

impl List {
    pub fn new(rows: &[Row], count: usize, width: usize, wrap: bool) -> Self {
        let column = rows
            .iter()
            .map(|row| spans_width(&row.label) + if row.current { 2 } else { 0 })
            .max()
            .unwrap_or(0)
            .min(width * 45 / 100);
        Self {
            numbered: count <= 9,
            column,
            wrap,
        }
    }

    pub fn row(
        &self,
        index: usize,
        row: &Row,
        selected: bool,
        width: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let accent = theme.accent.add_modifier(Modifier::BOLD);
        let mut lead = vec![
            Span::raw("  "),
            if selected {
                Span::styled("› ", accent)
            } else {
                Span::raw("  ")
            },
        ];
        if self.numbered {
            lead.push(Span::styled(
                format!("{}. ", index + 1),
                if selected { accent } else { theme.dim },
            ));
        }
        let head = spans_width(&lead);
        let mut label: Spans = if selected {
            row.label
                .iter()
                .map(|span| Span::styled(span.content.clone(), accent))
                .collect()
        } else {
            row.label.clone()
        };
        if row.current {
            label.push(Span::styled(" ✓", theme.success));
        }
        let indent = [Span::raw(" ".repeat(head))];
        if row.detail.is_empty() {
            return wrap::wrap(&label, width, &lead, &indent);
        }
        let column = head + self.column + 3;
        if self.wrap && width < column + 30 {
            let mut spans = lead;
            spans.extend(label);
            let mut lines = vec![wrap::fit(spans, width, true)];
            lines.extend(wrap::wrap(&row.detail, width, &indent, &indent));
            return lines;
        }
        let mut first = lead;
        first.extend(wrap::fit(label, self.column, true).spans);
        first.push(Span::raw(" ".repeat(column - spans_width(&first))));
        if self.wrap {
            return wrap::wrap(&row.detail, width, &first, &[Span::raw(" ".repeat(column))]);
        }
        first.extend(row.detail.iter().cloned());
        vec![wrap::fit(first, width, true)]
    }
}

/// Every row of a short option list.
pub fn options(
    rows: &[Row],
    selected: usize,
    width: usize,
    wrap: bool,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let list = List::new(rows, rows.len(), width, wrap);
    rows.iter()
        .enumerate()
        .flat_map(|(index, row)| list.row(index, row, index == selected, width, theme))
        .collect()
}
