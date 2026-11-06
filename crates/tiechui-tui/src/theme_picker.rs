//! Syntax themes shared by every renderer, with a reversible picker preview.

use std::path::Path;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::text::{Line, Span};
use syntect::highlighting::{Theme as SyntaxTheme, ThemeSet};

use crate::{diff, syntax, theme::Theme, wrap};

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub custom: bool,
    pub theme: Option<Arc<SyntaxTheme>>,
}

pub fn catalog(directory: Option<&Path>) -> Vec<Entry> {
    static BUILTINS: std::sync::OnceLock<Vec<Entry>> = std::sync::OnceLock::new();
    let mut entries = BUILTINS
        .get_or_init(|| {
            let mut entries = vec![Entry {
                name: "automatic".into(),
                custom: false,
                theme: None,
            }];
            for (name, light) in [("Catppuccin Latte", true), ("Catppuccin Mocha", false)] {
                if let Some(theme) = syntax::theme(light) {
                    entries.push(Entry {
                        name: name.into(),
                        custom: false,
                        theme: Some(Arc::new(theme.clone())),
                    });
                }
            }
            for (name, theme) in ThemeSet::from(two_face::theme::extra()).themes {
                if !entries.iter().any(|entry| entry.name == name) {
                    entries.push(Entry {
                        name,
                        custom: false,
                        theme: Some(Arc::new(theme)),
                    });
                }
            }
            entries
        })
        .clone();
    if let Some(directory) = directory
        && let Ok(files) = std::fs::read_dir(directory)
    {
        let mut paths: Vec<_> = files
            .flatten()
            .map(|file| file.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "tmTheme"))
            .collect();
        paths.sort();
        for path in paths {
            let Some(name) = path.file_stem().and_then(|name| name.to_str()) else {
                continue;
            };
            if entries.iter().any(|entry| entry.name == name) {
                continue;
            }
            if let Ok(theme) = ThemeSet::get_theme(&path) {
                entries.push(Entry {
                    name: name.into(),
                    custom: true,
                    theme: Some(Arc::new(theme)),
                });
            }
        }
    }
    entries
}

pub enum Outcome {
    Preview(Option<Arc<SyntaxTheme>>),
    Save(String),
    Cancel(Option<Arc<SyntaxTheme>>),
}

pub struct ThemePicker {
    entries: Vec<Entry>,
    matches: Vec<usize>,
    selected: usize,
    query: String,
    original: Option<Arc<SyntaxTheme>>,
}

impl ThemePicker {
    pub fn new(entries: Vec<Entry>, name: &str, theme: &Theme) -> Self {
        let selected = entries
            .iter()
            .position(|entry| entry.name == name)
            .unwrap_or(0);
        Self {
            matches: (0..entries.len()).collect(),
            entries,
            selected,
            query: String::new(),
            original: theme.syntax.clone(),
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Outcome::Cancel(self.original.clone()),
            KeyCode::Char('c') if control => return Outcome::Cancel(self.original.clone()),
            KeyCode::Enter => {
                if let Some(index) = self.matches.get(self.selected) {
                    return Outcome::Save(self.entries[*index].name.clone());
                }
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.matches.len().saturating_sub(1))
            }
            KeyCode::Char(ch) if !control => {
                self.query.push(ch);
                self.filter();
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.filter();
            }
            _ => {}
        }
        Outcome::Preview(self.matches.get(self.selected).map_or_else(
            || self.original.clone(),
            |index| self.entries[*index].theme.clone(),
        ))
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        self.matches = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.name.to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();
        self.selected = 0;
    }

    pub fn render(&self, width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::styled("Select Syntax Theme", theme.accent),
            Line::styled("Move up/down to live preview themes", theme.dim),
            Line::from(format!("Filter: {}", self.query)),
        ];
        let available = height.saturating_sub(11).clamp(1, 8);
        let start = self.selected.saturating_sub(available - 1);
        for (position, index) in self.matches.iter().enumerate().skip(start).take(available) {
            let entry = &self.entries[*index];
            lines.push(Line::styled(
                format!(
                    "{}{}{}",
                    if position == self.selected {
                        "› "
                    } else {
                        "  "
                    },
                    entry.name,
                    if entry.custom { " (custom)" } else { "" }
                ),
                if position == self.selected {
                    theme.accent
                } else {
                    theme.dim
                },
            ));
        }
        if self.matches.is_empty() {
            lines.push(Line::styled("  No matching themes", theme.dim));
        }
        let preview = diff::lines(
            " 12 fn greet(name: &str) -> String {\n-13     format!(\"Hello, {}!\", name)\n+13     format!(\"Hello, {name}!\")\n 14 }",
            Some("example.rs"),
            if width >= 88 { width / 2 - 2 } else { width },
            theme,
        );
        if width >= 88 {
            let column = width / 2;
            let top = 3 + available.saturating_sub(preview.len()) / 2;
            for (index, row) in preview.into_iter().enumerate() {
                let y = top + index;
                lines.resize_with(lines.len().max(y + 1), Line::default);
                let left = &mut lines[y];
                *left = wrap::fit(
                    left.spans
                        .iter()
                        .map(|span| {
                            Span::styled(span.content.clone(), left.style.patch(span.style))
                        })
                        .collect(),
                    column,
                    true,
                );
                left.spans
                    .push(Span::raw(" ".repeat(column + 2 - wrap::line_width(left))));
                left.spans.extend(row.spans);
            }
        } else {
            lines.push(Line::default());
            lines.extend(preview);
        }
        lines.truncate(height.saturating_sub(1));
        lines.push(Line::styled(
            "↑/↓ preview · enter save · esc cancel",
            theme.dim,
        ));
        lines
            .into_iter()
            .take(height.max(1))
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
}
