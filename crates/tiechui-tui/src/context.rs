//! A frozen context browser on the shared alternate-screen pager.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui_core::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use std::time::{Duration, Instant};
use tiechui_protocol::{ContextItem, ContextSnapshot, ContextView, cache_lines, overview};

use crate::{
    pager::Pager,
    status::{category_style, compact_tokens as n},
    syntax,
    theme::Theme,
    wrap,
};

const TABS: [ContextView; 5] = [
    ContextView::Overview,
    ContextView::System,
    ContextView::Tools,
    ContextView::Injections,
    ContextView::Messages,
];

#[derive(Debug)]
struct Category {
    name: String,
    view: ContextView,
    tokens: u64,
}

#[derive(Debug)]
pub struct ContextPanel {
    snapshot: ContextSnapshot,
    categories: Vec<Category>,
    view: ContextView,
    selected: usize,
    overview_selected: usize,
    category: Option<usize>,
    detail: Option<ContextItem>,
    information: bool,
    zoom: bool,
    can_zoom: bool,
    query: String,
    searching: bool,
    list_offset: usize,
    height: usize,
    pager: Pager,
    detail_cache: Option<(usize, bool, Theme, Vec<Line<'static>>)>,
    targets: Vec<(usize, usize, usize, usize)>,
    last_click: Option<(usize, Instant)>,
    needs_input: bool,
    message_cache: Option<(String, Option<usize>, Vec<usize>, u64)>,
}

impl ContextPanel {
    pub fn text_view(&self) -> bool {
        self.detail.is_some() || self.information
    }

    pub fn needs_input(&mut self, pending: bool) {
        self.needs_input = pending;
    }
    pub fn mouse(&mut self, event: MouseEvent) {
        match event.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let code = if event.kind == MouseEventKind::ScrollUp {
                    KeyCode::Up
                } else {
                    KeyCode::Down
                };
                for _ in 0..if self.detail.is_some() || self.information {
                    3
                } else {
                    1
                } {
                    self.key(KeyEvent::new(code, KeyModifiers::NONE));
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, _, _, index)) = self
                    .targets
                    .iter()
                    .find(|(x, y, width, _)| {
                        usize::from(event.row) == *y
                            && (*x..x + width).contains(&usize::from(event.column))
                    })
                    .copied()
                {
                    if self.dashboard() {
                        self.overview_selected = index;
                    } else {
                        self.selected = index;
                    }
                    let now = Instant::now();
                    let double = self.last_click.is_some_and(|(last, at)| {
                        last == index && now.duration_since(at) < Duration::from_millis(400)
                    });
                    self.last_click = Some((index, now));
                    if double {
                        self.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                        self.last_click = None;
                    }
                }
            }
            _ => {}
        }
    }

    pub fn paste(&mut self, text: &str) {
        if self.searching {
            self.query
                .extend(text.chars().filter(|ch| !ch.is_control()));
            self.selected = 0;
            self.list_offset = 0;
        }
    }

    pub fn new(snapshot: ContextSnapshot, view: ContextView) -> Self {
        let mut categories: Vec<Category> = Vec::new();
        for (view, items) in [
            (ContextView::System, &snapshot.system),
            (ContextView::Tools, &snapshot.tools),
            (ContextView::Messages, &snapshot.messages),
        ] {
            for item in items {
                if let Some(group) = categories
                    .iter_mut()
                    .find(|group| group.view == view && group.name == item.category)
                {
                    group.tokens += item.tokens;
                } else {
                    categories.push(Category {
                        name: item.category.clone(),
                        view,
                        tokens: item.tokens,
                    });
                }
            }
        }
        Self {
            snapshot,
            categories,
            view,
            selected: 0,
            overview_selected: 0,
            category: None,
            detail: None,
            information: view == ContextView::Cache,
            zoom: false,
            can_zoom: false,
            query: String::new(),
            searching: false,
            list_offset: 0,
            height: 1,
            pager: Pager::from_top(),
            detail_cache: None,
            targets: vec![],
            last_click: None,
            needs_input: false,
            message_cache: None,
        }
    }

    fn dashboard(&self) -> bool {
        self.view == ContextView::Overview && !self.information
    }
    fn entries(&self) -> Vec<&ContextItem> {
        let items = match self.view {
            ContextView::System => &self.snapshot.system,
            ContextView::Tools => &self.snapshot.tools,
            ContextView::Injections => &self.snapshot.injections,
            ContextView::Messages => &self.snapshot.messages,
            _ => return vec![],
        };
        let query = self.query.to_lowercase();
        items
            .iter()
            .filter(|item| {
                self.category
                    .is_none_or(|index| item.category == self.categories[index].name)
                    && format!("{} {}", item.label, item.source)
                        .to_lowercase()
                        .contains(&query)
            })
            .collect()
    }

    fn message_indices(&mut self) {
        if self
            .message_cache
            .as_ref()
            .is_some_and(|(query, category, _, _)| {
                *query == self.query && *category == self.category
            })
        {
            return;
        }
        let query = self.query.to_lowercase();
        let indices: Vec<usize> = self
            .snapshot
            .messages
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                self.category
                    .is_none_or(|index| item.category == self.categories[index].name)
                    && (query.is_empty()
                        || format!("{} {}", item.label, item.source)
                            .to_lowercase()
                            .contains(&query))
            })
            .map(|(index, _)| index)
            .collect();
        let total = indices
            .iter()
            .map(|index| self.snapshot.messages[*index].tokens)
            .sum();
        self.message_cache = Some((self.query.clone(), self.category, indices, total));
    }

    fn entry_count(&mut self) -> usize {
        if self.view == ContextView::Messages {
            self.message_indices();
            self.message_cache
                .as_ref()
                .map_or(0, |(_, _, indices, _)| indices.len())
        } else {
            self.entries().len()
        }
    }
    fn reset_detail(&mut self) {
        self.pager = Pager::from_top();
        self.detail_cache = None;
    }
    fn reset_list(&mut self) {
        self.selected = 0;
        self.list_offset = 0;
        self.query.clear();
        self.searching = false;
        self.reset_detail();
    }
    fn back(&mut self) {
        self.view = ContextView::Overview;
        self.information = false;
        self.category = None;
        self.detail = None;
        self.reset_list();
    }

    /// False closes the overlay; no operation is sent to the actor.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return true;
        }
        self.targets.clear();
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('c') {
            return false;
        }
        if self.searching {
            match key.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.query.clear();
                }
                KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(ch) if !control => self.query.push(ch),
                _ => {}
            }
            self.selected = 0;
            self.list_offset = 0;
            return true;
        }
        if key.code == KeyCode::Esc {
            if self.detail.take().is_some() {
                if self.category.is_some() && self.entries().len() == 1 {
                    self.back();
                }
                self.reset_detail();
            } else if self.category.is_some() || self.information {
                self.back();
            } else {
                return false;
            }
            return true;
        }
        if matches!(
            key.code,
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab
        ) {
            let current = TABS.iter().position(|view| *view == self.view).unwrap_or(0);
            let delta = if matches!(key.code, KeyCode::Left | KeyCode::BackTab) {
                TABS.len() - 1
            } else {
                1
            };
            self.view = TABS[(current + delta) % TABS.len()];
            self.detail = None;
            self.category = None;
            self.information = false;
            self.reset_list();
            return true;
        }
        if self.dashboard() {
            match key.code {
                KeyCode::Char('z' | 'Z') if self.can_zoom => self.zoom = !self.zoom,
                KeyCode::Char('i' | 'I' | 'c' | 'C') => {
                    self.information = true;
                    if matches!(key.code, KeyCode::Char('c' | 'C')) {
                        self.view = ContextView::Cache;
                    }
                    self.reset_detail();
                }
                KeyCode::Enter => {
                    if let Some(category) = self.categories.get(self.overview_selected) {
                        self.view = category.view;
                        self.category = Some(self.overview_selected);
                        self.reset_list();
                        if self.entries().len() == 1 {
                            self.detail = self.entries().first().cloned().cloned();
                        }
                    }
                }
                _ => {
                    self.overview_selected = movement(
                        key,
                        self.overview_selected,
                        self.categories.len(),
                        self.height,
                    )
                }
            }
        } else if self.detail.is_some() || self.information {
            // Context uses layered Escape and Ctrl+C; the shared pager owns scrolling.
            if !matches!(key.code, KeyCode::Char('q')) {
                let code = match key.code {
                    KeyCode::Char('u') if control => KeyCode::PageUp,
                    KeyCode::Char('d') if control => KeyCode::PageDown,
                    code => code,
                };
                self.pager.key(KeyEvent { code, ..key });
            }
        } else {
            match key.code {
                KeyCode::Char('/') => self.searching = true,
                KeyCode::Enter => {
                    self.detail = self.entries().get(self.selected).cloned().cloned();
                    self.reset_detail();
                }
                _ => self.selected = movement(key, self.selected, self.entry_count(), self.height),
            }
        }
        true
    }

    fn hint(&self, width: usize) -> String {
        if self.searching {
            return format!("Search › {}", self.query);
        }
        let choices: &[&str] = if self.dashboard() {
            if self.can_zoom {
                &[
                    "←/→ tabs · ↑↓ select · Enter open · z zoom · c cache · i details · Esc close",
                    "←→ tabs · ↑↓ · Enter · z · c · i · Esc",
                    "↑↓ · Enter · i · Esc",
                    "Esc",
                    "⎋",
                ]
            } else {
                &[
                    "←/→ tabs · ↑↓ select · Enter open · c cache · i details · Esc close",
                    "←→ tabs · ↑↓ · Enter · c · i · Esc",
                    "↑↓ · Enter · i · Esc",
                    "Esc",
                    "⎋",
                ]
            }
        } else if self.detail.is_some() || self.information {
            &[
                "↑↓ / PgUp PgDn scroll · ←/→ tabs · Esc back",
                "↑↓ scroll · Esc back",
                "Esc back",
                "Esc",
                "⎋",
            ]
        } else {
            &[
                "←/→ tabs · ↑↓ select · Enter preview · / search · Esc close",
                "←→ tabs · ↑↓ · Enter · / · Esc",
                "↑↓ · Enter · / · Esc",
                "Esc",
                "⎋",
            ]
        };
        choices
            .iter()
            .find(|text| wrap::width(text) <= width)
            .unwrap_or(&"")
            .to_string()
    }

    pub fn render(&mut self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        self.targets.clear();
        if rows == 0 || width == 0 {
            return vec![];
        }
        let title = format!(
            "Context · {:?}{}",
            self.view,
            if self.information && self.view != ContextView::Cache {
                " · Details"
            } else {
                ""
            }
        );
        if rows < 3 {
            let mut screen = if rows == 2 {
                vec![Line::styled(title, theme.accent)]
            } else {
                vec![]
            };
            screen.push(Line::styled(self.hint(width), theme.dim));
            return fit(screen, width);
        }
        let dashboard = self.dashboard();
        let framed = !dashboard && rows >= 12 && width >= 16;
        let mut header = if framed {
            vec![
                Line::styled("─".repeat(width), theme.separator),
                Line::default(),
            ]
        } else {
            vec![]
        };
        header.push(Line::styled(
            title,
            theme.accent.add_modifier(Modifier::BOLD),
        ));
        if self.needs_input && rows >= 4 {
            header.push(Line::styled(
                "Main needs input · close context to respond",
                theme.warning,
            ));
        }
        if width >= 65 && rows >= if dashboard { 12 } else { 16 } {
            header.push(Line::from(
                TABS.iter()
                    .map(|tab| {
                        Span::styled(
                            format!("{tab:?}  "),
                            if *tab == self.view {
                                theme.accent
                            } else {
                                theme.dim
                            },
                        )
                    })
                    .collect::<Vec<_>>(),
            ));
        }
        if let Some(detail) = &self.detail {
            if rows >= 9 {
                header.push(Line::styled(
                    format!("{} · ~{} tokens", detail.label, n(detail.tokens)),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
            }
        } else if let Some(index) = self.category {
            header.push(Line::styled(self.categories[index].name.clone(), theme.dim));
        }
        if !self.query.is_empty() && !self.searching && rows >= 9 {
            header.push(Line::styled(format!("Search: {}", self.query), theme.dim));
        }
        let pinned = self
            .detail
            .as_ref()
            .filter(|_| rows >= 18)
            .map(|item| wrapped(&item.source, width, theme.dim))
            .filter(|lines| lines.len() <= 2);
        if let Some(lines) = &pinned {
            header.extend(lines.clone());
        }
        if rows >= 8 {
            header.push(Line::default());
        }
        let description = if !dashboard && self.detail.is_none() && !self.information && rows >= 18
        {
            let text = match self.view {
                ContextView::System => {
                    "Effective system prompt and sections in the current branch."
                }
                ContextView::Tools => {
                    "Tool definitions declared in this snapshot, grouped by known source."
                }
                ContextView::Injections => {
                    "References already counted in System / Messages, plus changes from the last observed request. Not added to the total."
                }
                ContextView::Messages => "Current branch messages in chronological order.",
                _ => "",
            };
            let lines = wrapped(text, width, theme.dim);
            if lines.len() <= 3 && rows.saturating_sub(header.len() + lines.len()) >= 12 {
                lines
            } else {
                vec![]
            }
        } else {
            vec![]
        };
        let footer_size =
            if framed { 3 } else { 1 } + description.len() + usize::from(!description.is_empty());
        self.height = rows.saturating_sub(header.len() + footer_size).max(1);
        let body = if dashboard {
            self.dashboard_lines(width, self.height, theme)
        } else if self.detail.is_some() || self.information {
            let stale = self.detail_cache.as_ref().is_none_or(|(w, source, t, _)| {
                *w != width || *source != pinned.is_none() || t != theme
            });
            if stale {
                let lines = if let Some(item) = &self.detail {
                    detail_lines(item, width, theme, pinned.is_none())
                } else {
                    let mut report = if self.view == ContextView::Cache {
                        cache_lines(&self.snapshot.cache)
                    } else {
                        overview(&self.snapshot)
                    };
                    if self.view == ContextView::Cache {
                        let start = self.snapshot.cache.responses.len().saturating_sub(12);
                        for (index, response) in
                            self.snapshot.cache.responses.iter().enumerate().skip(start)
                        {
                            report.push(format!(
                                "#{}: {} input · {} cached · ~{} fresh{} · {} unattributed",
                                index + 1,
                                response.input,
                                response.cached,
                                response.fresh,
                                if self.snapshot.cache.folds > 0 {
                                    format!(" · ~{} compression", response.compression)
                                } else {
                                    String::new()
                                },
                                response.unattributed
                            ));
                        }
                    }
                    report
                        .iter()
                        .flat_map(|line| wrapped(line, width, Style::default()))
                        .collect()
                };
                self.detail_cache = Some((width, pinned.is_none(), theme.clone(), lines));
            }
            let lines = &self
                .detail_cache
                .as_ref()
                .map(|(_, _, _, lines)| lines.as_slice())
                .unwrap_or_default();
            let overflow = lines.len() > self.height && self.height > 1;
            let mut body = self
                .pager
                .viewport(lines, self.height - usize::from(overflow));
            if overflow {
                body.resize(self.height - 1, Line::default());
                let (start, end, count) = self.pager.position();
                body.push(Line::styled(
                    format!("  Lines {start}–{end} of {count}"),
                    theme.dim,
                ));
            }
            body
        } else {
            self.list_lines(width, self.height, theme)
        };
        for (_, y, _, _) in &mut self.targets {
            *y += header.len();
        }
        header.extend(body);
        header.resize(rows.saturating_sub(footer_size), Line::default());
        if !description.is_empty() {
            header.push(Line::default());
            header.extend(description);
        }
        if framed {
            header.push(Line::default());
            header.push(Line::styled("─".repeat(width), theme.separator));
        }
        header.push(Line::styled(self.hint(width), theme.dim));
        fit(header, width)
    }

    fn list_lines(&mut self, width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.view == ContextView::Messages {
            return self.message_lines(width, height, theme);
        }
        let entries = self.entries();
        if entries.is_empty() {
            return vec![Line::styled(
                if self.query.is_empty() {
                    "  No items in this snapshot."
                } else {
                    "  No matching items. Change / search."
                },
                theme.dim,
            )];
        }
        let mut groups: Vec<(String, Vec<(usize, &ContextItem)>)> = vec![];
        for (index, item) in entries.iter().enumerate() {
            let label = match self.view {
                ContextView::Tools => item.group.clone(),
                ContextView::Messages => "Current branch · chronological order".into(),
                ContextView::Injections => format!(
                    "{} · {}",
                    item.category,
                    if matches!(
                        item.category.as_str(),
                        "Request-only" | "Removed / replaced"
                    ) {
                        "Last observed request"
                    } else {
                        "System reference"
                    }
                ),
                _ => item.category.clone(),
            };
            if let Some((_, group)) = groups.iter_mut().find(|(name, _)| name == &label) {
                group.push((index, item));
            } else {
                groups.push((label, vec![(index, item)]));
            }
        }
        let total: u64 = entries.iter().map(|item| item.tokens).sum();
        let value_width = format!("~{}", n(total)).len();
        let mut lines = vec![];
        let mut selected_line = 0;
        let mut entry_rows = vec![];
        for (label, group) in groups {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let amount = if self.view == ContextView::Injections {
                String::new()
            } else {
                format!("~{}", n(group.iter().map(|(_, item)| item.tokens).sum()))
            };
            lines.push(value_row(
                "  ",
                &format!("{label} ({})", group.len()),
                &amount,
                value_width,
                width.min(76),
                theme.dim,
            ));
            for (position, (index, item)) in group.iter().enumerate() {
                entry_rows.push((lines.len(), *index));
                if *index == self.selected {
                    selected_line = lines.len();
                }
                let prefix = format!(
                    "{}{}",
                    if *index == self.selected {
                        "→ "
                    } else {
                        "  "
                    },
                    if width < 16 {
                        ""
                    } else if position + 1 == group.len() {
                        "└─ "
                    } else {
                        "├─ "
                    }
                );
                lines.push(value_row(
                    &prefix,
                    &item.label,
                    &format!("~{}", n(item.tokens)),
                    value_width,
                    width.min(76),
                    if *index == self.selected {
                        theme.accent
                    } else {
                        Style::default()
                    },
                ));
            }
        }
        if self.view != ContextView::Injections {
            lines.push(Line::default());
            lines.push(value_row(
                "  ",
                "TOTAL",
                &format!("~{}", n(total)),
                value_width,
                width.min(76),
                theme.dim,
            ));
        }
        let count = entries.len();
        let overflow = lines.len() > height && height > 1;
        let available = height - usize::from(overflow);
        if selected_line < self.list_offset {
            self.list_offset = selected_line.saturating_sub(1);
        } else if selected_line >= self.list_offset + available {
            self.list_offset = selected_line + 1 - available;
        }
        if self.selected + 1 == count && lines.len() - selected_line <= available {
            self.list_offset = lines.len().saturating_sub(available);
        }
        self.list_offset = self.list_offset.min(lines.len().saturating_sub(available));
        self.targets.extend(
            entry_rows
                .into_iter()
                .filter(|(row, _)| *row >= self.list_offset && *row < self.list_offset + available)
                .map(|(row, index)| (0, row - self.list_offset, width, index)),
        );
        let mut result: Vec<_> = lines
            .into_iter()
            .skip(self.list_offset)
            .take(available)
            .collect();
        if overflow {
            result.push(Line::styled(
                format!("  ({}/{count})", self.selected + 1),
                theme.dim,
            ));
        }
        result
    }

    fn message_lines(&mut self, width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
        self.message_indices();
        let Some((_, _, indices, total)) = &self.message_cache else {
            return vec![];
        };
        let count = indices.len();
        if count == 0 {
            return vec![Line::styled(
                if self.query.is_empty() {
                    "  No items in this snapshot."
                } else {
                    "  No matching items. Change / search."
                },
                theme.dim,
            )];
        }
        let rows = count + 3;
        let overflow = rows > height && height > 1;
        let available = height - usize::from(overflow);
        let selected_row = self.selected + 1;
        if selected_row < self.list_offset {
            self.list_offset = selected_row.saturating_sub(1);
        } else if selected_row >= self.list_offset + available {
            self.list_offset = selected_row + 1 - available;
        }
        if self.selected + 1 == count && rows - selected_row <= available {
            self.list_offset = rows.saturating_sub(available);
        }
        self.list_offset = self.list_offset.min(rows.saturating_sub(available));
        let value_width = format!("~{}", n(*total)).len();
        let mut lines = Vec::new();
        for row in self.list_offset..(self.list_offset + available).min(rows) {
            let line = if row == 0 {
                value_row(
                    "  ",
                    &format!("Current branch · chronological order ({count})"),
                    &format!("~{}", n(*total)),
                    value_width,
                    width.min(76),
                    theme.dim,
                )
            } else if row <= count {
                let index = row - 1;
                let item = &self.snapshot.messages[indices[index]];
                self.targets.push((0, row - self.list_offset, width, index));
                let prefix = format!(
                    "{}{}",
                    if index == self.selected { "→ " } else { "  " },
                    if width < 16 {
                        ""
                    } else if index + 1 == count {
                        "└─ "
                    } else {
                        "├─ "
                    }
                );
                value_row(
                    &prefix,
                    &item.label,
                    &format!("~{}", n(item.tokens)),
                    value_width,
                    width.min(76),
                    if index == self.selected {
                        theme.accent
                    } else {
                        Style::default()
                    },
                )
            } else if row == count + 1 {
                Line::default()
            } else {
                value_row(
                    "  ",
                    "TOTAL",
                    &format!("~{}", n(*total)),
                    value_width,
                    width.min(76),
                    theme.dim,
                )
            };
            lines.push(line);
        }
        if overflow {
            lines.push(Line::styled(
                format!("  ({}/{count})", self.selected + 1),
                theme.dim,
            ));
        }
        lines
    }

    fn dashboard_lines(
        &mut self,
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let window = self.snapshot.window;
        let fitted = window
            .min(10_000.max((self.snapshot.total as f64 * 1.15 / 1000.0).ceil() as u64 * 1000));
        let mut lines = vec![];
        if height >= 2 {
            lines.push(Line::from(format!(
                "Category estimate: ~{} / {}",
                n(self.snapshot.total),
                if window > 0 {
                    n(window)
                } else {
                    "unknown".into()
                }
            )));
        }
        if height >= 5 {
            lines.push(Line::styled(
                format!(
                    "Session context estimate: {} · i details",
                    n(self.snapshot.input_tokens)
                ),
                theme.dim,
            ));
        }
        if height >= 12 {
            lines.push(Line::styled(
                cache_lines(&self.snapshot.cache)[0].replace("Provider cache hit rate", "Cache")
                    + " · c diagnostics",
                theme.dim,
            ));
        }
        if window > 0 && self.snapshot.total > window && height >= 4 {
            lines.push(Line::styled(
                format!(
                    "Category estimate exceeds window by {}",
                    n(self.snapshot.total - window)
                ),
                theme.warning,
            ));
        }
        if height >= 8 {
            lines.push(Line::default());
        }
        let remaining = height.saturating_sub(lines.len());
        let map = width >= 52 && remaining >= 8 && window > 0;
        self.can_zoom = map && fitted < window;
        let scale = if self.zoom { fitted } else { window };
        let spacing = usize::from(width >= 72);
        let map_width = if map { 2 + 16 + 15 * spacing + 3 } else { 0 };
        let legend_width = width.saturating_sub(map_width);
        let available = remaining.saturating_sub(if map { 3 } else { 1 }).max(1);
        let count = available.min(self.categories.len());
        let start = self
            .overview_selected
            .saturating_add(1)
            .saturating_sub(count)
            .min(self.categories.len().saturating_sub(count));
        let map_rows = if map { available.clamp(4, 16) } else { 0 };
        let cells = 16 * map_rows;
        if remaining >= 3 {
            lines.push(Line::styled(
                format!("{}Categories", " ".repeat(map_width)),
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        for row in 0..map_rows.max(count + usize::from(window > 0 && available > count)) {
            if lines.len() >= height.saturating_sub(if map { 2 } else { 0 }) {
                break;
            }
            let mut spans = vec![];
            if map && row < map_rows {
                spans.push(Span::raw("  "));
                for column in 0..16 {
                    let cell = row * 16 + column;
                    let (group, mixed) = map_cell(&self.categories, scale, cells, cell);
                    if let Some(index) = group {
                        self.targets
                            .push((2 + column * (1 + spacing), lines.len(), 1, index));
                    }
                    spans.push(if let Some(index) = group {
                        let mut style = category_style(&self.categories[index].name, theme);
                        if index == self.overview_selected {
                            style = style.add_modifier(Modifier::BOLD);
                        }
                        Span::styled(if mixed { "◧" } else { "■" }, style)
                    } else {
                        Span::styled("□", theme.dim)
                    });
                    if column < 15 && spacing > 0 {
                        spans.push(Span::raw(" "));
                    }
                }
                spans.push(Span::raw("   "));
            } else if map {
                spans.push(Span::raw(" ".repeat(map_width)));
            }
            if row < count {
                let index = start + row;
                let group = &self.categories[index];
                self.targets
                    .push((map_width, lines.len(), legend_width, index));
                let label = match group.name.as_str() {
                    "Appended prompt" => "Prompt additions",
                    "User / injected messages" => "User / injected",
                    "Assistant" => "Assistant messages",
                    "Tool results" => "Tool output",
                    name => name,
                };
                let value = if window > 0 && legend_width >= 30 {
                    format!(
                        "~{}  {:>5.1}%",
                        n(group.tokens),
                        group.tokens as f64 * 100.0 / window as f64
                    )
                } else {
                    format!("~{}", n(group.tokens))
                };
                let style = if index == self.overview_selected {
                    theme.accent
                } else {
                    theme.dim
                };
                let prefix = if index == self.overview_selected {
                    "› "
                } else {
                    "  "
                };
                spans.push(Span::styled(prefix, style));
                spans.push(Span::styled("■ ", category_style(&group.name, theme)));
                spans.extend(
                    value_row(
                        "",
                        label,
                        &value,
                        value.len(),
                        legend_width.saturating_sub(4),
                        style,
                    )
                    .spans,
                );
            } else if row == count && window > 0 {
                spans.push(Span::styled(
                    format!(
                        "  □ Free (estimate) ~{}",
                        n(window.saturating_sub(self.snapshot.total))
                    ),
                    theme.dim,
                ));
            }
            lines.push(Line::from(spans));
            if !map && row >= count {
                break;
            }
        }
        if map {
            let label = if self.zoom && self.can_zoom {
                format!("Fit {} → {}", n(window), n(scale))
            } else {
                "Full window".into()
            };
            lines.push(Line::styled(
                format!(
                    "  {label} · ~{} tokens/cell",
                    n(scale / cells.max(1) as u64)
                ),
                theme.dim,
            ));
            lines.push(Line::styled(
                "  ■ category · ◧ mixed / partial · □ free (estimate)",
                theme.dim,
            ));
        }
        lines.truncate(height);
        lines
    }
}

fn movement(key: KeyEvent, selected: usize, count: usize, height: usize) -> usize {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let next = match key.code {
        KeyCode::Up | KeyCode::Char('k') => selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => selected.saturating_add(1),
        KeyCode::Home => 0,
        KeyCode::End => count,
        KeyCode::PageUp => selected.saturating_sub(height),
        KeyCode::PageDown => selected.saturating_add(height),
        KeyCode::Char('u') if control => selected.saturating_sub(height),
        KeyCode::Char('d') if control => selected.saturating_add(height),
        _ => selected,
    };
    next.min(count.saturating_sub(1))
}

fn fit(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .map(|line| {
            wrap::fit(
                line.spans
                    .into_iter()
                    .map(|span| Span::styled(span.content, line.style.patch(span.style)))
                    .collect(),
                width,
                false,
            )
        })
        .collect()
}
fn wrapped(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let indent = if width > 4 {
        vec![Span::raw("  ")]
    } else {
        vec![]
    };
    text.split('\n')
        .flat_map(|line| {
            wrap::wrap(
                &[Span::styled(line.to_owned(), style)],
                width,
                &indent,
                &indent,
            )
        })
        .collect()
}
fn value_row(
    prefix: &str,
    label: &str,
    value: &str,
    value_width: usize,
    width: usize,
    style: Style,
) -> Line<'static> {
    if value.is_empty() {
        return Line::styled(format!("{prefix}{label}"), style);
    }
    let room = width.saturating_sub(wrap::width(prefix) + value_width + 1);
    let name = wrap::fit(vec![Span::raw(label.to_owned())], room, true);
    let gap = width.saturating_sub(wrap::width(prefix) + wrap::line_width(&name) + value_width);
    let dots = if gap >= 4 {
        format!(" {} ", "·".repeat(gap - 2))
    } else {
        " ".repeat(gap)
    };
    let mut spans = vec![Span::styled(prefix.to_owned(), style)];
    spans.extend(
        name.spans
            .into_iter()
            .map(|span| Span::styled(span.content, style)),
    );
    spans.push(Span::styled(format!("{dots}{value:>value_width$}"), style));
    Line::from(spans)
}
fn detail_lines(
    item: &ContextItem,
    width: usize,
    theme: &Theme,
    include_source: bool,
) -> Vec<Line<'static>> {
    let label = if item.json { "Definition" } else { "Content" };
    let mut lines = vec![
        Line::styled(
            format!("{label} · ~{} tokens (estimate)", n(item.tokens)),
            theme.accent,
        ),
        Line::default(),
    ];
    let indent = if width > 4 {
        vec![Span::raw("  ")]
    } else {
        vec![]
    };
    let text = if item.json {
        serde_json::from_str::<serde_json::Value>(&item.text)
            .ok()
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .unwrap_or_else(|| item.text.clone())
    } else {
        item.text.clone()
    };
    if let Some(rows) = item
        .json
        .then(|| syntax::highlight(&text, "json", theme))
        .flatten()
    {
        for row in rows {
            lines.extend(wrap::wrap(&row, width, &indent, &indent));
        }
    } else {
        lines.extend(wrapped(&text, width, Style::default()));
    }
    if include_source {
        lines.extend([
            Line::default(),
            Line::styled("Source", theme.accent),
            Line::default(),
        ]);
        lines.extend(wrapped(&item.source, width, theme.dim));
    }
    lines
}
fn map_cell(groups: &[Category], scale: u64, count: usize, cell: usize) -> (Option<usize>, bool) {
    if scale == 0 || count == 0 {
        return (None, false);
    }
    let start = cell as f64 * scale as f64 / count as f64;
    let end = (cell + 1) as f64 * scale as f64 / count as f64;
    let mut used = 0.0;
    let mut best = (None, 0.0);
    let mut hits = 0;
    for (index, group) in groups.iter().enumerate() {
        let next = used + group.tokens as f64;
        let overlap = (end.min(next) - start.max(used)).max(0.0);
        if overlap > 0.0 {
            hits += 1;
            if overlap > best.1 {
                best = (Some(index), overlap);
            }
        }
        used = next;
    }
    (best.0, hits > 1 || best.1 < (end - start) * (1.0 - 1e-9))
}
