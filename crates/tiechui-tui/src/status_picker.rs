//! `/statusline`: choose, order and style the status line items with a live preview.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};

use crate::status::{self, Item, Snapshot, StatusLineConfig};
use crate::theme::Theme;
use crate::wrap;

const PAGE: usize = 8;

pub enum Outcome {
    Open,
    Cancel,
    Save(StatusLineConfig),
}

/// The first row is the theme-colors switch; item rows follow, selected ones first.
pub struct StatusPicker {
    rows: Vec<(Option<Item>, bool)>,
    icons: HashSet<Item>,
    selected: usize,
    offset: usize,
    search: String,
}

impl StatusPicker {
    pub fn new(config: &StatusLineConfig) -> Self {
        let mut rows = vec![(None, config.use_theme_colors)];
        rows.extend(config.items.iter().map(|item| (Some(*item), true)));
        rows.extend(
            Item::ALL
                .into_iter()
                .filter(|item| !config.items.contains(item))
                .map(|item| (Some(item), false)),
        );
        Self {
            rows,
            icons: config.icons.iter().copied().collect(),
            selected: 0,
            offset: 0,
            search: String::new(),
        }
    }

    /// Rows matching the search as a subsequence, by index into `rows`.
    fn filtered(&self) -> Vec<usize> {
        let query = self.search.to_lowercase();
        (0..self.rows.len())
            .filter(|index| {
                let name = self.rows[*index].0.map_or("use theme colors", Item::id);
                let mut rest = name.chars();
                query.chars().all(|c| rest.any(|n| n == c))
            })
            .collect()
    }

    /// The configuration as edited; icon choices of hidden items are kept.
    pub fn selection(&self) -> StatusLineConfig {
        let items = || {
            self.rows
                .iter()
                .filter_map(|(item, on)| item.map(|i| (i, *on)))
        };
        StatusLineConfig {
            items: items()
                .filter(|(_, on)| *on)
                .map(|(item, _)| item)
                .collect(),
            use_theme_colors: self.rows[0].1,
            icons: items()
                .map(|(item, _)| item)
                .filter(|item| self.icons.contains(item))
                .collect(),
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let filtered = self.filtered();
        let actual = filtered.get(self.selected).copied();
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Char('c') if control => return Outcome::Cancel,
            KeyCode::Enter => return Outcome::Save(self.selection()),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('p') if control => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected += 1,
            KeyCode::Char('n') if control => self.selected += 1,
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(PAGE),
            KeyCode::PageDown => self.selected += PAGE,
            KeyCode::Tab | KeyCode::BackTab => {
                if let Some((Some(item), _)) = actual.map(|index| self.rows[index])
                    && !self.icons.remove(&item)
                {
                    self.icons.insert(item);
                }
            }
            KeyCode::Char(' ') => {
                if let Some(index) = actual {
                    self.rows[index].1 = !self.rows[index].1;
                }
            }
            KeyCode::Left | KeyCode::Right if self.search.is_empty() => {
                if let Some(index) = actual.filter(|index| *index > 0) {
                    let next = if key.code == KeyCode::Left {
                        index - 1
                    } else {
                        index + 1
                    };
                    if next > 0 && next < self.rows.len() {
                        self.rows.swap(index, next);
                        self.selected = next;
                    }
                }
            }
            KeyCode::Backspace => {
                self.search.pop();
                self.selected = 0;
                self.offset = 0;
            }
            KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.search.push(c);
                self.selected = 0;
                self.offset = 0;
            }
            _ => {}
        }
        self.selected = self.selected.min(self.filtered().len().saturating_sub(1));
        Outcome::Open
    }

    /// At most `height` rows: the menu on the input surface, then the selected item's two
    /// forms, the preview and the key hints. The cursor sits in the search field.
    pub fn render(
        &mut self,
        s: &Snapshot<'_>,
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, (usize, usize)) {
        let height = height.max(1);
        let rich = height >= 20;
        let filtered = self.filtered();
        let config = self.selection();
        let colors = status::colored(&config, theme);
        let dim = |text: &str| Span::styled(text.to_owned(), theme.dim);
        let chosen =
            |text: &str| Span::styled(text.to_owned(), theme.accent.add_modifier(Modifier::BOLD));
        let sample = |item: Item, icon: bool| {
            status::segment(item, s, theme, colors, icon).unwrap_or_else(|| {
                let prefix = if icon {
                    format!("{} ", status::glyph(item))
                } else {
                    String::new()
                };
                vec![dim(&format!("{prefix}unavailable"))]
            })
        };
        let indent = || Span::raw("  ");
        let active = filtered.get(self.selected).map(|index| self.rows[*index]);

        let mut footer: Vec<Line<'static>> = Vec::new();
        if rich && let Some((Some(item), on)) = active {
            let hidden = if on { "" } else { " · hidden" };
            footer.push(Line::from(vec![
                indent(),
                dim(&format!("{}{hidden}", status::description(item))),
            ]));
            let candidates: Vec<Vec<Span<'static>>> = [false, true]
                .into_iter()
                .map(|icon| {
                    let label = if icon { "Icon" } else { "Text" };
                    let badge = if self.icons.contains(&item) == icon {
                        chosen(&format!("[{label}]"))
                    } else {
                        dim(&format!(" {label} "))
                    };
                    let mut spans = vec![badge, Span::raw(" ")];
                    spans.extend(sample(item, icon));
                    spans
                })
                .collect();
            if width >= 90 {
                let half = (width - 6) / 2;
                let left = wrap::fit(candidates[0].clone(), half, true);
                let mut spans = vec![indent()];
                let pad = half.saturating_sub(wrap::line_width(&left)) + 2;
                spans.extend(left.spans);
                spans.push(Span::raw(" ".repeat(pad)));
                spans.extend(wrap::fit(candidates[1].clone(), half, true).spans);
                footer.push(Line::from(spans));
            } else {
                for candidate in candidates {
                    let mut spans = vec![indent()];
                    spans.extend(candidate);
                    footer.push(Line::from(spans));
                }
            }
            if height >= 28 {
                footer.push(Line::default());
            }
        }
        if height >= 14 {
            if rich {
                footer.push(Line::from(vec![indent(), dim("Preview")]));
            }
            for text in ["", "› Message", ""] {
                footer.push(surface(Line::from(text), width, theme));
            }
        }
        footer.push(if status::spans(&config, s, theme).is_empty() {
            Line::from(vec![
                indent(),
                dim(if config.items.is_empty() {
                    "Status line disabled"
                } else {
                    "No selected values are available"
                }),
            ])
        } else {
            status::line(&config, s, width, theme)
        });
        if height >= 18 {
            footer.push(Line::from(vec![
                indent(),
                dim("space show/hide · tab text/icon · ←/→ order"),
            ]));
            footer.push(Line::from(vec![indent(), dim("enter save · esc cancel")]));
        } else {
            footer.push(Line::from(vec![indent(), dim("tab style · enter/esc")]));
        }
        if height >= 28 {
            footer.push(Line::from(vec![
                indent(),
                dim("Icon uses your terminal's Nerd Font."),
            ]));
        }

        let mut lines = vec![
            Line::from(vec![
                indent(),
                Span::styled(
                    "Configure Status Line",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                indent(),
                dim("Search "),
                Span::raw(format!("> {}", self.search)),
            ]),
            Line::default(),
        ];
        let cursor = (
            1,
            (2 + wrap::width("Search > ") + wrap::width(&self.search)).min(width.saturating_sub(1)),
        );
        let name_width = 22
            .min(width.saturating_sub(if width >= 48 { 23 } else { 16 }))
            .max(1);
        if rich {
            let preview = if width >= 72 { "    Preview" } else { "" };
            lines.push(Line::from(dim(&format!(
                "      {:<pad$}Presentation{preview}",
                "Item",
                pad = name_width + 2
            ))));
        }
        let count = height
            .saturating_sub(lines.len() + footer.len() + 1)
            .clamp(1, PAGE);
        self.offset = self.offset.min(self.selected);
        if self.selected >= self.offset + count {
            self.offset = self.selected + 1 - count;
        }
        for (visible, index) in filtered.iter().enumerate().skip(self.offset).take(count) {
            let (item, on) = self.rows[*index];
            let is_active = visible == self.selected;
            let name = item.map_or("Use theme colors", Item::id);
            let label = wrap::fit(vec![Span::raw(name)], name_width, true);
            let label: String = label.spans.iter().map(|s| s.content.as_ref()).collect();
            let head = format!(
                "{}{}{label}{}",
                if is_active { "› " } else { "  " },
                if on { "[x] " } else { "[ ] " },
                " ".repeat(name_width.saturating_sub(wrap::width(&label)) + 2)
            );
            let mut spans = vec![if is_active {
                chosen(&head)
            } else {
                Span::raw(head)
            }];
            match item {
                Some(item) if width >= 22 => {
                    let icon = self.icons.contains(&item);
                    let short = width < 48;
                    let badge = |label: &str, short_label: &str, picked: bool| {
                        let text = match (short, picked) {
                            (true, true) => format!("[{short_label}]"),
                            (true, false) => format!(" {short_label} "),
                            (false, true) => format!("[{label}]"),
                            (false, false) => format!(" {label} "),
                        };
                        if picked { chosen(&text) } else { dim(&text) }
                    };
                    spans.push(badge("Text", "T", !icon));
                    spans.push(Span::raw(" "));
                    spans.push(badge("Icon", "I", icon));
                    if width >= 64 {
                        spans.push(Span::raw("   "));
                        spans.extend(sample(item, icon));
                    }
                }
                None if width >= 56 => {
                    let text = "Apply colors from the active theme";
                    spans.push(if is_active { chosen(text) } else { dim(text) });
                }
                _ => {}
            }
            lines.push(wrap::fit(spans, width, true));
        }
        if filtered.is_empty() {
            lines.push(Line::from(vec![indent(), dim("No matching items")]));
        }
        if height >= 10 && filtered.len() > count {
            lines.push(Line::from(vec![
                indent(),
                dim(&format!(
                    "{}–{} / {} · ↑/↓ select",
                    self.offset + 1,
                    filtered.len().min(self.offset + count),
                    filtered.len()
                )),
            ]));
        }
        let mut screen: Vec<Line<'static>> = lines
            .into_iter()
            .map(|line| surface(line, width, theme))
            .collect();
        screen.extend(
            footer
                .into_iter()
                .map(|line| wrap::fit(line.spans, width, true)),
        );
        screen.truncate(height);
        (screen, cursor)
    }
}

/// A full-width row on the input surface.
pub(crate) fn surface(line: Line<'static>, width: usize, theme: &Theme) -> Line<'static> {
    let mut line = wrap::fit(line.spans, width, true);
    let used = wrap::line_width(&line);
    line.spans
        .push(Span::raw(" ".repeat(width.saturating_sub(used))));
    let base = theme.user();
    for span in &mut line.spans {
        span.style = base.patch(span.style);
    }
    line
}
