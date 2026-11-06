//! Markdown to styled lines with the Mist roles.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};

use crate::theme::Theme;
use crate::wrap::{self, wrap};

enum Container {
    Quote,
    List(Option<u64>),
    Item { marker: String, emitted: bool },
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<Vec<Span<'static>>>>,
    header_rows: usize,
}

struct Renderer<'t> {
    theme: &'t Theme,
    width: usize,
    out: Vec<Line<'static>>,
    containers: Vec<Container>,
    styles: Vec<Style>,
    /// Logical lines of the open block; source newlines stay line breaks.
    pending: Vec<Vec<Span<'static>>>,
    code: Option<(String, String)>,
    table: Option<Table>,
    link: Option<String>,
    /// A block was emitted at this depth, so the next one is separated by a blank line.
    separate: bool,
    after_paragraph: bool,
}

pub fn render(source: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut renderer = Renderer {
        theme,
        width: width.max(1),
        out: Vec::new(),
        containers: Vec::new(),
        styles: Vec::new(),
        pending: Vec::new(),
        code: None,
        table: None,
        link: None,
        separate: false,
        after_paragraph: false,
    };
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(source, options) {
        renderer.event(event);
    }
    renderer.flush_block();
    renderer.out
}

impl Renderer<'_> {
    fn style(&self) -> Style {
        self.styles
            .iter()
            .fold(Style::default(), |acc, style| acc.patch(*style))
    }

    fn push_text(&mut self, text: &str, style: Style) {
        if let Some(table) = &mut self.table {
            if let Some(cell) = table.rows.last_mut().and_then(|row| row.last_mut()) {
                cell.push(Span::styled(text.replace('\n', " "), style));
            }
            return;
        }
        for (index, part) in text.split('\n').enumerate() {
            if index > 0 || self.pending.is_empty() {
                self.pending.push(Vec::new());
            }
            if !part.is_empty()
                && let Some(line) = self.pending.last_mut()
            {
                line.push(Span::styled(part.to_owned(), style));
            }
        }
    }

    fn line_break(&mut self) {
        if self.table.is_none() {
            if self.pending.is_empty() {
                self.pending.push(Vec::new());
            }
            self.pending.push(Vec::new());
        }
    }

    fn prefixes(&mut self) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
        let (mut first, mut rest) = (Vec::new(), Vec::new());
        for container in &mut self.containers {
            match container {
                Container::Quote => {
                    first.push(Span::styled("│ ", self.theme.separator));
                    rest.push(Span::styled("│ ", self.theme.separator));
                }
                Container::List(_) => {}
                Container::Item { marker, emitted } => {
                    let pad = " ".repeat(wrap::width(marker));
                    if *emitted {
                        first.push(Span::raw(pad.clone()));
                    } else {
                        first.push(Span::styled(marker.clone(), self.theme.dim));
                        *emitted = true;
                    }
                    rest.push(Span::raw(pad));
                }
            }
        }
        (first, rest)
    }

    fn emit(&mut self, spans: &[Span<'static>]) {
        let (first, rest) = self.prefixes();
        let lines = wrap(spans, self.width, &first, &rest);
        self.out.extend(lines);
    }

    fn blank(&mut self) {
        let (first, _) = self.prefixes();
        let line: Vec<Span<'static>> = first
            .into_iter()
            .map(|span| Span::styled(span.content.trim_end().to_owned(), span.style))
            .filter(|span| !span.content.is_empty())
            .collect();
        self.out.push(Line::from(line));
    }

    fn start_block(&mut self) {
        self.flush_block();
        if self.separate {
            self.blank();
        }
        self.separate = false;
    }

    fn end_block(&mut self) {
        self.flush_block();
        self.separate = true;
    }

    fn flush_block(&mut self) {
        for line in std::mem::take(&mut self.pending) {
            self.emit(&line);
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => {
                if let Some((_, code)) = &mut self.code {
                    code.push_str(&text);
                } else {
                    let style = self.style();
                    self.push_text(&text, style);
                }
            }
            Event::Code(text) => {
                let style = self.style().patch(self.theme.accent);
                self.push_text(&text, style);
            }
            Event::InlineMath(text)
            | Event::DisplayMath(text)
            | Event::Html(text)
            | Event::InlineHtml(text) => {
                let style = self.style();
                self.push_text(text.trim_end_matches('\n'), style);
            }
            Event::SoftBreak | Event::HardBreak => self.line_break(),
            Event::Rule => {
                self.start_block();
                let rule = "─".repeat(self.width.min(80));
                self.emit(&[Span::styled(rule, self.theme.separator)]);
                self.separate = true;
            }
            Event::TaskListMarker(checked) => {
                let style = self.style();
                self.push_text(if checked { "[x] " } else { "[ ] " }, style);
            }
            Event::FootnoteReference(name) => {
                let style = self.style();
                self.push_text(&format!("[^{name}]"), style);
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                // A paragraph is not separated from the list that follows it.
                self.start_block();
            }
            Tag::Heading { level, .. } => {
                self.start_block();
                let mut style = Style::default().add_modifier(Modifier::BOLD);
                if level == HeadingLevel::H1 {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                self.styles.push(style);
                if level >= HeadingLevel::H3 {
                    let prefix = format!("{} ", "#".repeat(level as usize));
                    self.push_text(&prefix, style);
                }
            }
            Tag::BlockQuote(_) => {
                self.start_block();
                self.containers.push(Container::Quote);
                self.styles.push(self.theme.secondary);
            }
            Tag::CodeBlock(kind) => {
                self.start_block();
                let language = match kind {
                    CodeBlockKind::Fenced(language) => language.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((language, String::new()));
            }
            Tag::List(start) => {
                if !self.after_paragraph
                    || !matches!(self.containers.last(), None | Some(Container::Item { .. }))
                {
                    self.start_block();
                } else {
                    self.flush_block();
                    self.separate = false;
                }
                self.containers.push(Container::List(start));
            }
            Tag::Item => {
                self.flush_block();
                let marker = match self.containers.last_mut() {
                    Some(Container::List(Some(next))) => {
                        let marker = format!("{next}. ");
                        *next += 1;
                        marker
                    }
                    _ => "- ".to_owned(),
                };
                self.containers.push(Container::Item {
                    marker,
                    emitted: false,
                });
                self.separate = false;
            }
            Tag::Table(_) => {
                self.start_block();
                self.table = Some(Table::default());
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(Vec::new());
                }
            }
            Tag::Emphasis => self
                .styles
                .push(Style::default().add_modifier(Modifier::ITALIC)),
            Tag::Strong => self
                .styles
                .push(Style::default().add_modifier(Modifier::BOLD)),
            Tag::Strikethrough => self
                .styles
                .push(Style::default().add_modifier(Modifier::CROSSED_OUT)),
            Tag::Link { dest_url, .. } => {
                self.link = Some(dest_url.to_string());
                self.styles.push(self.theme.accent);
            }
            Tag::Image { .. } => self.styles.push(self.theme.dim),
            _ => {}
        }
        self.after_paragraph = false;
    }

    fn end(&mut self, tag: TagEnd) {
        let paragraph = tag == TagEnd::Paragraph;
        match tag {
            TagEnd::Paragraph => self.end_block(),
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.end_block();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_block();
                self.styles.pop();
                self.containers.pop();
                self.separate = true;
            }
            TagEnd::CodeBlock => self.code_block(),
            TagEnd::List(_) => {
                self.flush_block();
                self.containers.pop();
                self.separate = true;
            }
            TagEnd::Item => {
                self.flush_block();
                if let Some(Container::Item { emitted: false, .. }) = self.containers.last() {
                    self.emit(&[]);
                }
                self.containers.pop();
                self.separate = false;
            }
            TagEnd::Table => self.table_block(),
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.header_rows = table.rows.len();
                }
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Image => {
                self.styles.pop();
            }
            TagEnd::Link => {
                self.styles.pop();
                if let Some(url) = self.link.take() {
                    let shown: String = self
                        .pending
                        .last()
                        .map(|line| line.iter().map(|span| span.content.as_ref()).collect())
                        .unwrap_or_default();
                    if !shown.ends_with(&url) && !url.starts_with('#') {
                        let style = self.theme.dim;
                        self.push_text(&format!(" ({url})"), style);
                    }
                }
            }
            _ => {}
        }
        self.after_paragraph = paragraph;
    }

    fn code_block(&mut self) {
        let Some((language, code)) = self.code.take() else {
            return;
        };
        let border = self.theme.dim;
        self.emit(&[Span::styled(format!("```{language}"), border)]);
        let code = code.strip_suffix('\n').unwrap_or(&code);
        if let Some(rows) = self
            .theme
            .syntax
            .as_ref()
            .and_then(|_| crate::syntax::highlight(code, &language, self.theme))
        {
            for row in rows {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(row);
                self.emit(&spans);
            }
        } else {
            for line in code.split('\n') {
                self.emit(&[Span::raw(format!("  {line}"))]);
            }
        }
        self.emit(&[Span::styled("```", border)]);
        self.separate = true;
    }

    fn table_block(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let cell_width =
            |cell: &Vec<Span<'static>>| cell.iter().map(|s| wrap::width(&s.content)).sum::<usize>();
        let mut widths: Vec<usize> = (0..columns)
            .map(|c| {
                table
                    .rows
                    .iter()
                    .filter_map(|row| row.get(c))
                    .map(cell_width)
                    .max()
                    .unwrap_or(0)
                    .max(1)
            })
            .collect();
        // Border overhead: "│ " + (n-1) * " │ " + " │".
        let available = self.width.saturating_sub(3 * columns + 1).max(columns);
        while widths.iter().sum::<usize>() > available {
            let Some(widest) = widths.iter_mut().max() else {
                break;
            };
            if *widest <= 1 {
                break;
            }
            *widest -= 1;
        }
        let border = self.theme.table_border;
        let rule = |left: &str, mid: &str, right: &str| {
            let cells: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            Line::from(Span::styled(
                format!("{left}─{}─{right}", cells.join(&format!("─{mid}─"))),
                border,
            ))
        };
        let mut lines = vec![rule("┌", "┬", "┐")];
        for (index, row) in table.rows.iter().enumerate() {
            let header = index < table.header_rows;
            let wrapped: Vec<Vec<Line<'static>>> = (0..columns)
                .map(|c| {
                    let mut cell = row.get(c).cloned().unwrap_or_default();
                    if header {
                        for span in &mut cell {
                            span.style = span.style.add_modifier(Modifier::BOLD);
                            if self.theme.syntax.is_some()
                                && let Some(rgb) = crate::syntax::scope_foreground(
                                    &["entity.name.type", "support.type"],
                                    self.theme,
                                )
                                && let Some(color) =
                                    crate::palette::best_color(rgb, self.theme.palette.level)
                            {
                                span.style = span.style.fg(color);
                            }
                        }
                    }
                    wrap(&cell, widths[c], &[], &[])
                })
                .collect();
            let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
            for line in 0..height {
                let mut spans = vec![Span::styled("│ ", border)];
                for (c, cell) in wrapped.iter().enumerate() {
                    if c > 0 {
                        spans.push(Span::styled(" │ ", border));
                    }
                    let content = cell.get(line).cloned().unwrap_or_default();
                    let pad = widths[c].saturating_sub(wrap::line_width(&content));
                    spans.extend(content.spans);
                    spans.push(Span::raw(" ".repeat(pad)));
                }
                spans.push(Span::styled(" │", border));
                lines.push(Line::from(spans));
            }
            if header && index + 1 == table.header_rows && index + 1 < table.rows.len() {
                lines.push(rule("├", "┼", "┤"));
            }
        }
        lines.push(rule("└", "┴", "┘"));
        let (first, rest) = self.prefixes();
        for (index, line) in lines.into_iter().enumerate() {
            let mut spans = if index == 0 {
                first.clone()
            } else {
                rest.clone()
            };
            spans.extend(line.spans);
            self.out.push(Line::from(spans));
        }
        self.separate = true;
    }
}
