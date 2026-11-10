//! `/model`, `/thinking` and `/permissions`: pick one entry from a list. Models with
//! several thinking levels take one inline with ←→.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span};
use tiechui_protocol::{ModelInfo, Op, ThinkingLevel};

use crate::sheet::{self, List, Row};
use crate::status::{PermissionMode, compact_tokens, thinking_name};
use crate::theme::Theme;
use crate::wrap;

const ROWS: usize = 10;

/// The levels a model offers, the one it would start at, and the one chosen.
pub struct Thinking {
    levels: Vec<ThinkingLevel>,
    start: usize,
    index: usize,
}

pub struct Choice {
    /// What an exact query picks, and what search matches with the rest.
    pub name: String,
    label: String,
    /// Dim aligned columns: the first goes on narrow terminals, the last aligns right.
    columns: Vec<String>,
    detail: String,
    current: bool,
    /// Shown in the warning tone.
    caution: bool,
    /// A blank row before it.
    gap: bool,
    pub op: Op,
    pub thinking: Option<Thinking>,
}

impl Choice {
    fn new(label: impl Into<String>, detail: impl Into<String>, op: Op) -> Self {
        let label = label.into();
        Self {
            name: label.clone(),
            label,
            columns: Vec::new(),
            detail: detail.into(),
            current: false,
            caution: false,
            gap: false,
            op,
            thinking: None,
        }
    }

    fn ops(&self) -> Vec<Op> {
        let mut ops = vec![self.op.clone()];
        if let Some(thinking) = self.thinking.as_ref().filter(|t| t.index != t.start) {
            ops.push(Op::SetThinking {
                level: thinking.levels[thinking.index],
            });
        }
        ops
    }
}

/// The nearest supported level at or above `level`, else the nearest below, as the core does.
fn clamp(level: ThinkingLevel, levels: &[ThinkingLevel]) -> usize {
    let above = (0..levels.len())
        .filter(|i| levels[*i] >= level)
        .min_by_key(|i| levels[*i]);
    let below = (0..levels.len())
        .filter(|i| levels[*i] < level)
        .max_by_key(|i| levels[*i]);
    above.or(below).unwrap_or(0)
}

pub fn models(
    models: &[ModelInfo],
    current: Option<&ModelInfo>,
    thinking: Option<ThinkingLevel>,
) -> Vec<Choice> {
    let thinking = thinking.unwrap_or(ThinkingLevel::Off);
    models
        .iter()
        .map(|model| {
            let name = format!("{}/{}", model.provider, model.id);
            let mut choice = Choice::new(
                model.id.clone(),
                "",
                Op::SetModel {
                    provider: model.provider.clone(),
                    model: name.clone(),
                },
            );
            choice.name = name;
            choice.columns = vec![
                model.provider.clone(),
                if model.context_window > 0 {
                    compact_tokens(model.context_window)
                } else {
                    String::new()
                },
            ];
            choice.current =
                current.is_some_and(|c| c.provider == model.provider && c.id == model.id);
            if model.thinking_levels.len() > 1 {
                let start = clamp(thinking, &model.thinking_levels);
                choice.thinking = Some(Thinking {
                    levels: model.thinking_levels.clone(),
                    start,
                    index: start,
                });
            }
            choice
        })
        .collect()
}

pub fn thinking(levels: &[ThinkingLevel], current: Option<ThinkingLevel>) -> Vec<Choice> {
    levels
        .iter()
        .map(|level| {
            let detail = match level {
                ThinkingLevel::Off => "No reasoning",
                ThinkingLevel::Minimal => "~1k tokens",
                ThinkingLevel::Low => "~2k tokens",
                ThinkingLevel::Medium => "~8k tokens",
                ThinkingLevel::High => "~16k tokens",
                ThinkingLevel::Xhigh => "~32k tokens",
                ThinkingLevel::Max => "Maximum reasoning",
            };
            let mut choice = Choice::new(
                thinking_name(*level),
                detail,
                Op::SetThinking { level: *level },
            );
            choice.current = current == Some(*level);
            choice
        })
        .collect()
}

/// `/permissions` without arguments: the modes, then the full report.
pub fn permissions(current: PermissionMode) -> Vec<Choice> {
    let command = |arguments: &str| Op::RunCommand {
        name: "permissions".into(),
        arguments: arguments.into(),
    };
    let mut choices: Vec<Choice> = [
        (
            PermissionMode::Rules,
            "rules",
            "Ask about anything the rules do not allow",
        ),
        (
            PermissionMode::Auto,
            "auto",
            "The model reviews what the rules would ask about",
        ),
        (
            PermissionMode::Yolo,
            "yolo",
            "Approve everything except explicit denies",
        ),
    ]
    .into_iter()
    .map(|(mode, label, detail)| {
        let mut choice = Choice::new(label, detail, command(label));
        choice.current = mode == current;
        choice.caution = mode == PermissionMode::Yolo;
        choice
    })
    .collect();
    let mut show = Choice::new(
        "show",
        "Effective rules, session grants and recent decisions",
        command("show"),
    );
    show.gap = true;
    choices.push(show);
    choices
}

pub enum Outcome {
    Open,
    Cancel,
    Pick(Vec<Op>),
}

pub struct Select {
    title: &'static str,
    meta: String,
    choices: Vec<Choice>,
    search: String,
    selected: usize,
}

impl Select {
    /// An exact name in `query` picks at once; anything else opens the list searching it.
    pub fn open(
        title: &'static str,
        meta: String,
        choices: Vec<Choice>,
        query: &str,
    ) -> Result<Vec<Op>, Self> {
        if let Some(choice) = choices.iter().find(|choice| choice.name == query) {
            return Ok(vec![choice.op.clone()]);
        }
        let mut select = Self {
            title,
            meta,
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
                let text = format!(
                    "{} {} {}",
                    choice.name,
                    choice.columns.join(" "),
                    choice.detail
                )
                .to_lowercase();
                let mut rest = text.chars();
                query.chars().all(|c| rest.any(|n| n == c))
            })
            .collect()
    }

    /// Short lists are numbered and pick by digit; longer ones show the search field.
    fn numbered(&self) -> bool {
        self.choices.len() <= 9
    }

    fn searching(&self) -> bool {
        !self.numbered() || !self.search.is_empty()
    }

    fn adjustable(&self) -> Option<usize> {
        self.filtered()
            .get(self.selected)
            .copied()
            .filter(|index| self.choices[*index].thinking.is_some())
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let filtered = self.filtered();
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Char('c') if control => return Outcome::Cancel,
            KeyCode::Enter => {
                return filtered.get(self.selected).map_or(Outcome::Open, |index| {
                    Outcome::Pick(self.choices[*index].ops())
                });
            }
            KeyCode::Char(c @ '1'..='9') if self.numbered() && !control => {
                if let Some(index) = filtered.get(c as usize - '1' as usize) {
                    return Outcome::Pick(self.choices[*index].ops());
                }
            }
            KeyCode::Left | KeyCode::Right => {
                if let Some(index) = self.adjustable()
                    && let Some(thinking) = &mut self.choices[index].thinking
                {
                    thinking.index = if key.code == KeyCode::Left {
                        thinking.index.saturating_sub(1)
                    } else {
                        (thinking.index + 1).min(thinking.levels.len() - 1)
                    };
                }
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

    fn row(&self, choice: &Choice, selected: bool, widths: &[usize], theme: &Theme) -> Row {
        let mut row = Row::new(choice.label.clone(), choice.detail.clone(), theme);
        row.current = choice.current;
        if choice.caution {
            row.label[0].style = theme.warning;
        }
        let last = widths.len().saturating_sub(1);
        let mut cells: Vec<String> = choice
            .columns
            .iter()
            .zip(widths)
            .enumerate()
            .filter(|(_, (_, width))| **width > 0)
            .map(|(index, (text, width))| {
                if index == last {
                    format!("{text:>width$}")
                } else {
                    format!("{text:<width$}")
                }
            })
            .collect();
        if let Some(thinking) = &choice.thinking {
            cells.push(String::new());
            row.detail.push(Span::styled(cells.join("   "), theme.dim));
            if selected {
                row.detail.extend([
                    Span::styled("‹ ", theme.dim),
                    Span::styled(
                        thinking_name(thinking.levels[thinking.index]),
                        theme.accent.add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(" ›", theme.dim),
                ]);
            } else {
                row.detail.push(Span::styled("thinking", theme.dim));
            }
        } else if !cells.is_empty() {
            row.detail.push(Span::styled(cells.join("   "), theme.dim));
        }
        row
    }

    /// At most `height` rows; the cursor sits in the search field while it is shown.
    pub fn render(
        &self,
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let filtered = self.filtered();
        let mut body = Vec::new();
        let mut cursor = None;
        if self.searching() {
            body.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Search  ", theme.dim),
                Span::raw(self.search.clone()),
            ]));
            body.push(Line::default());
            cursor = Some((
                1,
                (2 + wrap::width("Search  ") + wrap::width(&self.search))
                    .min(width.saturating_sub(1)),
            ));
        }
        let gaps = filtered
            .iter()
            .filter(|index| self.choices[**index].gap)
            .count();
        let count = height.saturating_sub(body.len() + 3 + gaps).clamp(1, ROWS);
        let first = self
            .selected
            .saturating_sub(count / 2)
            .min(filtered.len().saturating_sub(count));
        // The provider column goes when narrow unless labels repeat; empty columns take no room.
        let columns = self
            .choices
            .iter()
            .map(|c| c.columns.len())
            .max()
            .unwrap_or(0);
        let labels: std::collections::HashSet<&str> =
            self.choices.iter().map(|c| c.label.as_str()).collect();
        let narrow = width < 64 && labels.len() == self.choices.len();
        let widths: Vec<usize> = (0..columns)
            .map(|column| {
                if column == 0 && columns > 1 && narrow {
                    return 0;
                }
                self.choices
                    .iter()
                    .filter_map(|c| c.columns.get(column))
                    .map(|text| wrap::width(text))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let shown: Vec<(usize, usize)> = filtered
            .iter()
            .copied()
            .enumerate()
            .skip(first)
            .take(count)
            .collect();
        let rows: Vec<Row> = shown
            .iter()
            .map(|(position, index)| {
                self.row(
                    &self.choices[*index],
                    *position == self.selected,
                    &widths,
                    theme,
                )
            })
            .collect();
        let list = List::new(&rows, self.choices.len(), width, false);
        for ((position, index), row) in shown.iter().zip(&rows) {
            if self.choices[*index].gap && self.search.is_empty() {
                body.push(Line::default());
            }
            body.extend(list.row(*position, row, *position == self.selected, width, theme));
        }
        if filtered.is_empty() {
            body.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("No matches", theme.dim),
            ]));
        }
        let range = format!("1-{}", filtered.len().max(1));
        let mut pairs = vec![("enter", "select")];
        if self.adjustable().is_some() {
            pairs.push(("←→", "thinking"));
        }
        if self.numbered() {
            pairs.push((&range, "pick"));
        }
        pairs.extend([("a-z", "search"), ("esc", "cancel")]);
        let position =
            (filtered.len() > count).then(|| format!("{} / {}", self.selected + 1, filtered.len()));
        let meta = if self.meta.is_empty() {
            Vec::new()
        } else {
            vec![vec![Span::styled(self.meta.clone(), theme.dim)]]
        };
        let mut lines = sheet::frame(
            theme.accent,
            sheet::rule(theme.accent, &[sheet::title(self.title)], &meta, width),
            body,
            sheet::hints(theme, &pairs, position.as_deref(), width),
            width,
        );
        lines.truncate(height.max(1));
        (lines, cursor.map(|(row, column)| (row + 1, column)))
    }
}
