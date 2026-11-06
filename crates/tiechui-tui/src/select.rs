//! `/model` and `/thinking`: pick one entry from a searchable list, selector rows on
//! the input surface.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};
use tiechui_protocol::{ModelInfo, Op, ThinkingLevel};

use crate::status::{compact_tokens, thinking_name};
use crate::status_picker::surface;
use crate::theme::Theme;
use crate::wrap;

const ROWS: usize = 10;

pub struct Choice {
    pub label: String,
    pub detail: String,
    pub current: bool,
    pub op: Op,
}

pub fn models(models: &[ModelInfo], current: Option<&ModelInfo>) -> Vec<Choice> {
    models
        .iter()
        .map(|model| {
            let name = format!("{}/{}", model.provider, model.id);
            Choice {
                label: name.clone(),
                detail: if model.context_window > 0 {
                    format!("{} window", compact_tokens(model.context_window))
                } else {
                    String::new()
                },
                current: current.is_some_and(|c| c.provider == model.provider && c.id == model.id),
                op: Op::SetModel {
                    provider: model.provider.clone(),
                    model: name,
                },
            }
        })
        .collect()
}

pub fn thinking(levels: &[ThinkingLevel], current: Option<ThinkingLevel>) -> Vec<Choice> {
    levels
        .iter()
        .map(|level| Choice {
            label: thinking_name(*level).into(),
            detail: match level {
                ThinkingLevel::Off => "No reasoning",
                ThinkingLevel::Minimal => "Very brief reasoning (~1k tokens)",
                ThinkingLevel::Low => "Light reasoning (~2k tokens)",
                ThinkingLevel::Medium => "Moderate reasoning (~8k tokens)",
                ThinkingLevel::High => "Deep reasoning (~16k tokens)",
                ThinkingLevel::Xhigh => "Extra-high reasoning (~32k tokens)",
                ThinkingLevel::Max => "Maximum reasoning",
            }
            .into(),
            current: current == Some(*level),
            op: Op::SetThinking { level: *level },
        })
        .collect()
}

pub enum Outcome {
    Open,
    Cancel,
    Pick(Op),
}

pub struct Select {
    title: &'static str,
    choices: Vec<Choice>,
    search: String,
    selected: usize,
}

impl Select {
    /// An exact label in `query` picks at once; anything else opens the list searching it.
    pub fn open(title: &'static str, choices: Vec<Choice>, query: &str) -> Result<Op, Self> {
        if let Some(choice) = choices.iter().find(|choice| choice.label == query) {
            return Ok(choice.op.clone());
        }
        let mut select = Self {
            title,
            choices,
            search: query.to_owned(),
            selected: 0,
        };
        if query.is_empty() {
            select.selected = select.choices.iter().position(|c| c.current).unwrap_or(0);
        }
        Err(select)
    }

    fn filtered(&self) -> Vec<usize> {
        let query = self.search.to_lowercase();
        (0..self.choices.len())
            .filter(|index| {
                let choice = &self.choices[*index];
                let text = format!("{} {}", choice.label, choice.detail).to_lowercase();
                let mut rest = text.chars();
                query.chars().all(|c| rest.any(|n| n == c))
            })
            .collect()
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let filtered = self.filtered();
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Char('c') if control => return Outcome::Cancel,
            KeyCode::Enter => {
                return filtered.get(self.selected).map_or(Outcome::Open, |index| {
                    Outcome::Pick(self.choices[*index].op.clone())
                });
            }
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('p') if control => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected += 1,
            KeyCode::Char('n') if control => self.selected += 1,
            KeyCode::Backspace => {
                self.search.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.search.push(c);
                self.selected = 0;
            }
            _ => {}
        }
        self.selected = self.selected.min(self.filtered().len().saturating_sub(1));
        Outcome::Open
    }

    /// At most `height` rows; the cursor sits in the search field.
    pub fn render(
        &self,
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, (usize, usize)) {
        let filtered = self.filtered();
        let indent = || Span::raw("  ");
        let mut lines = vec![
            Line::from(vec![
                indent(),
                Span::styled(self.title, Style::default().add_modifier(Modifier::BOLD)),
            ]),
            Line::from(vec![
                indent(),
                Span::styled("Search ", theme.dim),
                Span::raw(format!("> {}", self.search)),
            ]),
            Line::default(),
        ];
        let cursor = (
            1,
            (2 + wrap::width("Search > ") + wrap::width(&self.search)).min(width.saturating_sub(1)),
        );
        let count = height.saturating_sub(lines.len() + 3).clamp(1, ROWS);
        let first = self
            .selected
            .saturating_sub(count / 2)
            .min(filtered.len().saturating_sub(count));
        for (position, index) in filtered.iter().enumerate().skip(first).take(count) {
            let choice = &self.choices[*index];
            let chosen = position == self.selected;
            let accent = theme.accent.add_modifier(Modifier::BOLD);
            lines.push(Line::from(vec![
                Span::styled(if chosen { "› " } else { "  " }, accent),
                Span::styled(if choice.current { "✓ " } else { "  " }, theme.accent),
                Span::styled(
                    choice.label.clone(),
                    if chosen { accent } else { Style::default() },
                ),
                Span::styled(format!("  {}", choice.detail), theme.dim),
            ]));
        }
        if filtered.is_empty() {
            lines.push(Line::from(vec![
                indent(),
                Span::styled("No matches", theme.dim),
            ]));
        } else if filtered.len() > count {
            lines.push(Line::from(Span::styled(
                format!("  ({}/{})", self.selected + 1, filtered.len()),
                theme.dim,
            )));
        }
        let mut screen: Vec<Line<'static>> = lines
            .into_iter()
            .map(|line| surface(line, width, theme))
            .collect();
        screen.push(Line::default());
        screen.push(wrap::fit(
            vec![Span::styled("  enter select · esc cancel", theme.dim)],
            width,
            true,
        ));
        screen.truncate(height.max(1));
        (screen, cursor)
    }
}
