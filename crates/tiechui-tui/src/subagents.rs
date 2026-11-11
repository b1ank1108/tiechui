//! Read-only child task viewer and the dispatch's transcript cell.
use std::time::Duration;

use crate::{
    activity,
    pager::Pager,
    resume::hint_line,
    status::compact_tokens,
    theme::Theme,
    tools::{Tool, View, plain},
    wrap,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_core::style::{Modifier, Style};
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
fn state(task: &AgentTask, theme: &Theme) -> (&'static str, Style) {
    match task.state {
        AgentState::Waiting => ("○", theme.dim),
        AgentState::Running => ("●", theme.accent),
        AgentState::Completed => ("✓", theme.success),
        AgentState::Failed => ("✗", theme.error),
        AgentState::Cancelled | AgentState::Skipped => ("−", theme.dim),
    }
}

fn usage(task: &AgentTask) -> String {
    let usage = &task.usage;
    let mut parts = vec![
        count(task.turns as usize, "turn"),
        format!(
            "{} in · {} out",
            compact_tokens(usage.input),
            compact_tokens(usage.output)
        ),
    ];
    if usage.cache_read + usage.cache_write > 0 {
        parts.push(format!(
            "{} cached",
            compact_tokens(usage.cache_read + usage.cache_write)
        ));
    }
    if let Some(cost) = usage.cost_nano_usd {
        parts.push(format!("${:.4}", cost as f64 / 1e9));
    }
    let model = plain(&task.model);
    if !model.is_empty() {
        parts.push(model);
    }
    parts.join(" · ")
}

fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

fn task_header_spans(task: &AgentTask, theme: &Theme) -> Vec<Span<'static>> {
    let (icon, color) = state(task, theme);
    let mut spans = vec![
        Span::styled(format!("{icon} "), color),
        Span::styled(
            plain(&task.agent),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    let model = plain(&task.model);
    if !model.is_empty() {
        spans.push(Span::styled(format!(" · {model}"), theme.dim));
    }
    spans.push(Span::styled(format!(" · {:?}", task.state), color));
    spans
}

fn task_row_spans(task: &AgentTask, theme: &Theme) -> Vec<Span<'static>> {
    let (icon, color) = state(task, theme);
    let mut spans = vec![
        Span::styled(format!("{icon} "), color),
        Span::raw(plain(&task.agent)),
    ];
    let model = plain(&task.model);
    if !model.is_empty() {
        spans.push(Span::styled(format!(" · {model}"), theme.dim));
    }
    spans
}

/// A task in full: who and what, the output, then thinking, files and usage.
pub fn task_lines(
    task: &AgentTask,
    width: usize,
    theme: &Theme,
    expanded: bool,
    thinking: bool,
) -> Vec<Line<'static>> {
    let mut lines = wrap::wrap(
        &task_header_spans(task, theme),
        width,
        &[],
        &[Span::raw("  ")],
    );
    let text = |text: &str| {
        if expanded {
            plain(text)
        } else {
            preview(text, width)
        }
    };
    lines.extend(text_lines(&text(&task.task), theme.dim, width));
    if !task.output.is_empty() {
        lines.push(Line::default());
        lines.extend(text_lines(&text(&task.output), Style::default(), width));
    }
    if let Some(error) = &task.diagnostic {
        lines.extend(text_lines(&plain(error), theme.error, width));
    }
    if expanded {
        if thinking && !task.thinking.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "Thinking",
                theme.dim.add_modifier(Modifier::BOLD),
            )));
            lines.extend(text_lines(&plain(&task.thinking), theme.dim, width));
        }
        lines.push(Line::default());
        for (name, path) in [
            ("Full output", &task.output_path),
            ("Full thinking", &task.thinking_path),
        ] {
            if let Some(path) = path {
                lines.extend(text_lines(
                    &format!("{name}: {}", plain(path)),
                    theme.dim,
                    width,
                ));
            }
        }
        lines.extend(text_lines(&usage(task), theme.dim, width));
    }
    lines
}

/// The dispatch's cell: a header like other tools, then a row per task with its result.
pub fn render(tool: &Tool, width: usize, theme: &Theme, view: View) -> Vec<Line<'static>> {
    let record = tool
        .output
        .as_ref()
        .or(tool.partial.as_ref())
        .and_then(|o| serde_json::from_value::<AgentDispatch>(o["details"].clone()).ok());
    let tasks = record.map(|record| record.tasks).unwrap_or_default();
    let bullet = match view {
        View::Live { phase, now } if !tool.done() => {
            let elapsed = tool.started.map_or(Duration::ZERO, |started| now - started);
            activity::bullet(theme, phase, elapsed)
        }
        _ if !tool.done() => Span::styled("•", theme.dim),
        _ if tool.error => Span::styled("•", theme.error.add_modifier(Modifier::BOLD)),
        _ => Span::styled("•", theme.success.add_modifier(Modifier::BOLD)),
    };
    let mut suffix = Vec::new();
    if !tasks.is_empty() {
        suffix.push(count(tasks.len(), "task"));
    }
    let running = tasks.iter().filter(|task| task.state.active()).count();
    if running > 0 && !tool.done() {
        suffix.push(format!("{running} running"));
    }
    if let Some(duration) = tool
        .duration
        .filter(|d| *d >= Duration::from_secs(1) && tool.done())
    {
        suffix.push(activity::elapsed(duration));
    }
    let mut lines = vec![wrap::fit(
        vec![
            bullet,
            Span::raw(" "),
            Span::styled(
                if tool.done() {
                    "Delegated"
                } else {
                    "Delegating"
                },
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                suffix
                    .iter()
                    .map(|part| format!(" · {part}"))
                    .collect::<String>(),
                theme.dim,
            ),
        ],
        width,
        true,
    )];
    let expanded = matches!(view, View::Transcript);
    let name = tasks
        .iter()
        .map(|task| wrap::width(&plain(&task.agent)))
        .max()
        .unwrap_or(0)
        .min(16);
    for (index, task) in tasks.iter().enumerate() {
        let (icon, color) = state(task, theme);
        let agent = plain(&task.agent);
        let status = match task.state {
            AgentState::Completed => String::new(),
            state => format!(" · {}", format!("{state:?}").to_lowercase()),
        };
        let room = width.saturating_sub(name + 8 + wrap::width(&status));
        let head = vec![
            Span::styled(if index == 0 { "  └ " } else { "    " }, theme.dim),
            Span::styled(format!("{icon} "), color),
            Span::raw(format!("{agent:<name$}  ")),
            Span::styled(preview(&task.task, room), theme.dim),
            Span::styled(status, color),
        ];
        lines.push(wrap::fit(head, width, true));
        let indent = [Span::raw("      ")];
        let body = |text: &str, style: Style| -> Vec<Line<'static>> {
            if expanded {
                plain(text)
                    .split('\n')
                    .flat_map(|line| {
                        wrap::wrap(
                            &[Span::styled(line.to_owned(), style)],
                            width,
                            &indent,
                            &indent,
                        )
                    })
                    .collect()
            } else {
                vec![wrap::fit(
                    vec![
                        indent[0].clone(),
                        Span::styled(preview(text, width.saturating_sub(6)), style),
                    ],
                    width,
                    true,
                )]
            }
        };
        if !task.output.is_empty() {
            lines.extend(body(&task.output, Style::default()));
        }
        if let Some(error) = &task.diagnostic {
            lines.extend(body(error, theme.error));
        }
        if expanded {
            lines.extend(body(&usage(task), theme.dim));
        }
    }
    if tasks.is_empty()
        && let Some(output) = &tool.output
    {
        lines.extend(wrap::wrap(
            &[Span::styled(crate::tools::output_text(output), theme.error)],
            width,
            &[Span::raw("  └ ")],
            &[Span::raw("    ")],
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
                format!("{} {} {} {}", t.agent, t.model, t.task, t.output)
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
        let mut title = vec![Span::styled(
            "Subagents",
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if needs_input {
            title.push(Span::styled(" · Main needs input", theme.warning));
        }
        let mut lines = vec![wrap::fit(title, width, true)];
        if self.searching || !self.search.is_empty() {
            lines.push(wrap::fit(
                vec![
                    Span::styled("Search  ", theme.dim),
                    Span::raw(self.search.clone()),
                    Span::styled(if self.searching { "▏" } else { "" }, theme.accent),
                ],
                width,
                true,
            ));
        }
        lines.push(Line::default());
        let footer = self.footer(width, theme);
        let height = rows.saturating_sub(lines.len() + footer.len());
        let split = !self.expanded && width >= 70;
        const LIST: usize = 28;
        let body_width = if split { width - LIST - 3 } else { width };
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
            let row = |index: usize, spans: Vec<Span<'static>>| {
                let chosen = index == selected;
                let mut line = vec![Span::styled(
                    if chosen { "› " } else { "  " },
                    theme.accent.add_modifier(Modifier::BOLD),
                )];
                line.extend(spans.into_iter().map(|span| {
                    if chosen && span.style == Style::default() {
                        Span::styled(span.content, theme.accent.add_modifier(Modifier::BOLD))
                    } else {
                        span
                    }
                }));
                wrap::fit(line, if split { LIST } else { width }, true)
            };
            let mut list = std::iter::once(vec![Span::raw("Main session")])
                .chain(tasks.iter().map(|(_, _, task)| task_row_spans(task, theme)))
                .enumerate()
                .skip(start)
                .take(height)
                .map(|(index, spans)| row(index, spans))
                .collect::<Vec<_>>();
            if split {
                list.resize(height, Line::default());
                for (index, mut line) in list.into_iter().enumerate() {
                    line.spans
                        .push(Span::raw(" ".repeat(LIST.saturating_sub(line.width()))));
                    line.spans.push(Span::styled(" │ ", theme.dim));
                    if let Some(right) = preview.get(index) {
                        line.spans.extend(right.spans.clone());
                    }
                    lines.push(line);
                }
            } else {
                lines.extend(list);
            }
        }
        lines.resize(rows.saturating_sub(footer.len()), Line::default());
        lines.extend(footer);
        lines.truncate(rows);
        lines
    }

    fn footer(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let thinking = if self.thinking {
            "hide thinking"
        } else {
            "thinking"
        };
        let group = if self.grouped { "ungroup" } else { "group" };
        let hints = if self.searching {
            vec!["enter done · esc done".to_owned()]
        } else if self.expanded {
            vec![
                format!("↑/↓ scroll · esc back · ctrl+x stop · ctrl+t {thinking}"),
                "↑/↓ scroll · esc back".into(),
            ]
        } else {
            vec![
                format!(
                    "↑/↓ select · enter open · ctrl+x stop · ctrl+f search · ctrl+s {group} · ctrl+t {thinking} · esc back"
                ),
                format!(
                    "↑/↓ select · enter open · ctrl+x stop · ctrl+f search · ctrl+t {thinking}"
                ),
                "↑/↓ select · enter open · ctrl+x stop · esc back".into(),
                "enter open · esc back".into(),
            ]
        };
        let hint = hints
            .iter()
            .find(|hint| wrap::width(hint) <= width)
            .or(hints.last())
            .map_or("", String::as_str);
        vec![Line::default(), hint_line(hint, width, theme)]
    }
}
