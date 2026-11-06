//! Read-only child task viewer and the dispatch's transcript cell.
use crate::{
    pager::Pager,
    theme::Theme,
    tools::{Tool, View, plain},
    wrap,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_core::text::{Line, Span};
use tiechui_protocol::{AgentDispatch, AgentState, AgentTask};

pub enum Outcome {
    Open,
    Close,
    Cancel(String),
}
pub struct Panel {
    pub records: Vec<AgentDispatch>,
    selected: usize,
    search: String,
    searching: bool,
    grouped: bool,
    thinking: bool,
    expanded: bool,
    pager: Pager,
}
fn preview(text: &str, width: usize) -> String {
    let text = plain(text).split_whitespace().collect::<Vec<_>>().join(" ");
    wrap::fit(vec![Span::raw(text)], width, true).to_string()
}
fn text_lines(text: &str, style: ratatui_core::style::Style, width: usize) -> Vec<Line<'static>> {
    text.split('\n')
        .flat_map(|line| wrap::wrap(&[Span::styled(line.to_owned(), style)], width, &[], &[]))
        .collect()
}
pub fn task_lines(
    task: &AgentTask,
    width: usize,
    theme: &Theme,
    expanded: bool,
    thinking: bool,
) -> Vec<Line<'static>> {
    let color = match task.state {
        AgentState::Failed => theme.error,
        AgentState::Completed => theme.success,
        _ => theme.dim,
    };
    let mut lines = wrap::wrap(
        &[
            Span::styled("• ", theme.dim),
            Span::styled(plain(&task.agent), theme.secondary),
            Span::styled(format!("  {:?}", task.state), color),
        ],
        width,
        &[],
        &[],
    );
    lines.extend(text_lines(
        &if expanded {
            plain(&task.task)
        } else {
            preview(&task.task, width)
        },
        theme.dim,
        width,
    ));
    lines.extend(text_lines(
        &if expanded {
            plain(&task.output)
        } else {
            preview(&task.output, width)
        },
        Default::default(),
        width,
    ));
    if let Some(error) = &task.diagnostic {
        lines.extend(text_lines(&plain(error), theme.error, width));
    }
    if expanded {
        if thinking && !task.thinking.is_empty() {
            lines.push(Line::from(Span::styled("Thinking", theme.dim)));
            lines.extend(text_lines(&plain(&task.thinking), theme.dim, width));
        }
        for (name, path) in [
            ("Full output", &task.output_path),
            ("Full thinking", &task.thinking_path),
        ] {
            if let Some(path) = path {
                lines.extend(wrap::wrap(
                    &[Span::styled(
                        format!("{name}: {} (read)", plain(path)),
                        theme.dim,
                    )],
                    width,
                    &[],
                    &[],
                ));
            }
        }
        lines.extend(wrap::wrap(
            &[Span::styled(
                format!(
                    "{} turns · ↑{} ↓{} R{} W{} · {}",
                    task.turns,
                    task.usage.input,
                    task.usage.output,
                    task.usage.cache_read,
                    task.usage.cache_write,
                    plain(&task.model)
                ),
                theme.dim,
            )],
            width,
            &[],
            &[],
        ));
        if let Some(cost) = task.usage.cost_nano_usd {
            lines.push(Line::from(Span::styled(
                format!("${:.6}", cost as f64 / 1e9),
                theme.dim,
            )));
        }
    }
    lines
}
pub fn render(tool: &Tool, width: usize, theme: &Theme, view: View) -> Vec<Line<'static>> {
    let record = tool
        .output
        .as_ref()
        .or(tool.partial.as_ref())
        .and_then(|o| serde_json::from_value::<AgentDispatch>(o["details"].clone()).ok());
    let mut lines = vec![Line::from(Span::styled(
        if tool.done() {
            "Finished waiting"
        } else {
            "Waiting"
        },
        theme.dim,
    ))];
    if let Some(record) = record {
        for task in record.tasks {
            lines.extend(task_lines(
                &task,
                width,
                theme,
                matches!(view, View::Transcript),
                true,
            ));
        }
    } else if let Some(output) = &tool.output {
        lines.extend(wrap::wrap(
            &[Span::styled(crate::tools::output_text(output), theme.error)],
            width,
            &[],
            &[],
        ));
    }
    lines
}
impl Panel {
    pub fn new(records: Vec<AgentDispatch>) -> Self {
        Self {
            records,
            selected: 0,
            search: String::new(),
            searching: false,
            grouped: false,
            thinking: false,
            expanded: false,
            pager: Pager::from_top(),
        }
    }
    fn tasks(&self) -> Vec<(String, usize, &AgentTask)> {
        let mut tasks = self
            .records
            .iter()
            .flat_map(|r| {
                r.tasks
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (r.id.clone(), i, t))
            })
            .filter(|(_, _, t)| {
                format!("{} {} {}", t.agent, t.task, t.output)
                    .to_lowercase()
                    .contains(&self.search.to_lowercase())
            })
            .collect::<Vec<_>>();
        if self.grouped {
            tasks.sort_by_key(|(_, _, t)| t.state);
        }
        tasks
    }
    pub fn update(&mut self, records: Vec<AgentDispatch>) {
        let selected = self
            .selected
            .checked_sub(1)
            .and_then(|i| self.tasks().get(i).map(|(id, i, _)| (id.clone(), *i)));
        self.records = records;
        if let Some(selected) = selected {
            self.selected = self
                .tasks()
                .iter()
                .position(|(id, i, _)| (id.clone(), *i) == selected)
                .map_or(0, |i| i + 1);
        }
    }
    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Open;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control {
            match key.code {
                KeyCode::Char('f') => {
                    self.searching = !self.searching;
                    return Outcome::Open;
                }
                KeyCode::Char('s') => {
                    self.grouped = !self.grouped;
                    self.selected = 0;
                    return Outcome::Open;
                }
                KeyCode::Char('t') => {
                    self.thinking = !self.thinking;
                    return Outcome::Open;
                }
                KeyCode::Char('x') => {
                    if let Some((id, _, _)) = self
                        .selected
                        .checked_sub(1)
                        .and_then(|i| self.tasks().get(i).cloned())
                    {
                        return Outcome::Cancel(id);
                    }
                }
                _ => {}
            }
        }
        if self.searching {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.search.pop();
                    self.selected = 0;
                }
                KeyCode::Char(c) if !control => {
                    self.search.push(c);
                    self.selected = 0;
                }
                _ => {}
            }
            return Outcome::Open;
        }
        if self.expanded {
            if !self.pager.key(key) {
                self.expanded = false;
            }
            return Outcome::Open;
        }
        let count = self.tasks().len() + 1;
        match key.code {
            KeyCode::Esc => return Outcome::Close,
            KeyCode::Left | KeyCode::Up => self.selected = (self.selected + count - 1) % count,
            KeyCode::Right | KeyCode::Down => self.selected = (self.selected + 1) % count,
            KeyCode::Enter if self.selected == 0 => return Outcome::Close,
            KeyCode::Enter => {
                self.expanded = true;
                self.pager = Pager::from_top();
            }
            _ => {}
        }
        Outcome::Open
    }
    pub fn render(
        &mut self,
        width: usize,
        rows: usize,
        theme: &Theme,
        main: &[Line<'static>],
        needs_input: bool,
    ) -> Vec<Line<'static>> {
        let tasks = self.tasks();
        let selected = self.selected.min(tasks.len());
        let title = if needs_input {
            "Subagents · Main needs input"
        } else {
            "Subagents"
        };
        let mut lines = vec![wrap::fit(
            vec![Span::styled(title, theme.accent)],
            width,
            false,
        )];
        let height = rows.saturating_sub(3);
        let body_width = if !self.expanded && width >= 70 {
            width.saturating_sub(29)
        } else {
            width
        };
        let preview = if selected == 0 {
            std::borrow::Cow::Borrowed(main)
        } else {
            std::borrow::Cow::Owned(task_lines(
                tasks[selected - 1].2,
                body_width,
                theme,
                true,
                self.thinking,
            ))
        };
        if self.expanded {
            lines.extend(self.pager.viewport(&preview, height));
        } else {
            let start = selected.saturating_sub(height.saturating_sub(1));
            let mut list = std::iter::once("Main session".into())
                .chain(
                    tasks
                        .iter()
                        .map(|(_, _, t)| format!("{}  {:?}", plain(&t.agent), t.state)),
                )
                .enumerate()
                .skip(start)
                .take(height)
                .map(|(i, t)| {
                    wrap::fit(
                        vec![Span::styled(
                            format!("{} {t}", if i == selected { "›" } else { " " }),
                            if i == selected {
                                theme.accent
                            } else {
                                theme.dim
                            },
                        )],
                        if width >= 70 { 27 } else { width },
                        false,
                    )
                })
                .collect::<Vec<_>>();
            if width >= 70 {
                list.resize(height, Line::default());
                for (i, mut line) in list.into_iter().enumerate() {
                    let padding = 29usize.saturating_sub(line.width());
                    line.spans.push(Span::raw(" ".repeat(padding)));
                    if let Some(right) = preview.get(i) {
                        line.spans.extend(right.spans.clone());
                    }
                    lines.push(line);
                }
            } else {
                lines.extend(list);
            }
        }
        lines.resize(rows.saturating_sub(2), Line::default());
        lines.push(wrap::fit(
            vec![Span::styled(
                format!(
                    "^F search: {}{} · ^S group · ^T think",
                    self.search,
                    if self.searching { "_" } else { "" }
                ),
                theme.dim,
            )],
            width,
            false,
        ));
        lines.push(wrap::fit(
            vec![Span::styled(
                "Enter view · Ctrl+X stop · Esc back",
                theme.dim,
            )],
            width,
            false,
        ));
        lines.truncate(rows);
        lines
    }
}
