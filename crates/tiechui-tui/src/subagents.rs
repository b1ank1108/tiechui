//! Child task viewer with background and stop actions, and the dispatch's transcript cell.
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
use serde_json::Value;
use tiechui_protocol::{AgentDispatch, AgentState, AgentTask, SubagentsView};

pub enum Outcome {
    Open,
    Close,
    Send(Value),
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

pub fn indicator(view: &SubagentsView) -> Option<String> {
    match view.background() {
        0 => None,
        1 => Some("1 agent".into()),
        count => Some(format!("{count} agents")),
    }
}

pub(crate) fn dispatch(tool: &Tool) -> Option<AgentDispatch> {
    let output = tool.output.as_ref().or(tool.partial.as_ref())?;
    serde_json::from_value::<AgentDispatch>(output["details"].clone()).ok()
}

pub(crate) fn has_dispatch(tool: &Tool) -> bool {
    dispatch(tool).is_some()
}

fn preview(text: &str, width: usize) -> String {
    let text = plain(text).split_whitespace().collect::<Vec<_>>().join(" ");
    wrap::fit(vec![Span::raw(text)], width, true).to_string()
}

fn text_lines(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
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

fn task_label(task: &AgentTask) -> String {
    let agent = plain(&task.agent);
    let id = plain(&task.id);
    if id.is_empty() {
        return agent;
    }
    match task
        .name
        .as_deref()
        .map(plain)
        .filter(|name| !name.is_empty())
    {
        Some(name) => format!("{id} ({name}) · {agent}"),
        None => format!("{id} · {agent}"),
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
            task_label(task),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    let model = plain(&task.model);
    if !model.is_empty() {
        spans.push(Span::styled(format!(" · {model}"), theme.dim));
    }
    if task.background {
        spans.push(Span::styled(" · background", theme.dim));
    } else if !task.id.is_empty() {
        spans.push(Span::styled(" · foreground", theme.dim));
    }
    spans.push(Span::styled(format!(" · {:?}", task.state), color));
    spans
}

fn task_row_spans(task: &AgentTask, theme: &Theme) -> Vec<Span<'static>> {
    let (icon, color) = state(task, theme);
    let mut spans = vec![
        Span::styled(format!("{icon} "), color),
        Span::raw(task_label(task)),
    ];
    let model = plain(&task.model);
    if !model.is_empty() {
        spans.push(Span::styled(format!(" · {model}"), theme.dim));
    }
    spans
}

fn expanded_meta_lines(
    task: &AgentTask,
    width: usize,
    theme: &Theme,
    thinking: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
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
    lines
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
    let format_body = |text: &str| {
        if expanded {
            plain(text)
        } else {
            preview(text, width)
        }
    };
    lines.extend(text_lines(&format_body(&task.task), theme.dim, width));
    if !task.output.is_empty() {
        lines.push(Line::default());
        lines.extend(text_lines(
            &format_body(&task.output),
            Style::default(),
            width,
        ));
    }
    if let Some(error) = &task.diagnostic {
        lines.extend(text_lines(&plain(error), theme.error, width));
    }
    if expanded {
        lines.extend(expanded_meta_lines(task, width, theme, thinking));
    }
    lines
}

fn header_bullet(
    tool: &Tool,
    background: bool,
    tasks: &[AgentTask],
    theme: &Theme,
    view: View,
) -> Span<'static> {
    if !tool.done() {
        if let View::Live { phase, now } = view {
            let elapsed = tool.started.map_or(Duration::ZERO, |started| {
                now.saturating_duration_since(started)
            });
            return activity::bullet(theme, phase, elapsed);
        }
        return Span::styled("•", theme.dim);
    }
    let tone = if tool.error {
        theme.error
    } else if background && tasks.iter().any(|task| task.state.active()) {
        theme.accent
    } else {
        theme.success
    };
    Span::styled("•", tone.add_modifier(Modifier::BOLD))
}

fn header_suffix(tool: &Tool, tasks: &[AgentTask], view: View) -> String {
    let mut suffix = Vec::new();
    if !tasks.is_empty() {
        suffix.push(count(tasks.len(), "task"));
    }
    let running = tasks.iter().filter(|task| task.state.active()).count();
    if running > 0 && !tool.done() {
        suffix.push(format!("{running} running"));
    }
    if let View::Live { now, .. } = view
        && !tool.done()
        && tool
            .started
            .is_some_and(|started| now.saturating_duration_since(started) >= Duration::from_secs(2))
    {
        suffix.push("ctrl+b background".to_owned());
    }
    if let Some(duration) = tool
        .duration
        .filter(|d| *d >= Duration::from_secs(1) && tool.done())
    {
        suffix.push(activity::elapsed(duration));
    }
    suffix.iter().map(|part| format!(" · {part}")).collect()
}

fn pad_label(label: &str, target: usize) -> String {
    let fitted = wrap::fit(vec![Span::raw(label.to_owned())], target, true).to_string();
    let pad = target.saturating_sub(wrap::width(&fitted));
    format!("{fitted}{}  ", " ".repeat(pad))
}

fn cell_body(text: &str, style: Style, width: usize, expanded: bool) -> Vec<Line<'static>> {
    let indent = [Span::raw("      ")];
    if !expanded {
        return vec![wrap::fit(
            vec![
                indent[0].clone(),
                Span::styled(preview(text, width.saturating_sub(6)), style),
            ],
            width,
            true,
        )];
    }
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
}

fn render_task_cell(
    index: usize,
    task: &AgentTask,
    name_width: usize,
    width: usize,
    theme: &Theme,
    expanded: bool,
) -> Vec<Line<'static>> {
    let (icon, color) = state(task, theme);
    let status = match task.state {
        AgentState::Completed => String::new(),
        state => format!(" · {}", format!("{state:?}").to_lowercase()),
    };
    let room = width.saturating_sub(name_width + 8 + wrap::width(&status));
    let head = vec![
        Span::styled(if index == 0 { "  └ " } else { "    " }, theme.dim),
        Span::styled(format!("{icon} "), color),
        Span::raw(pad_label(&task_label(task), name_width)),
        Span::styled(preview(&task.task, room), theme.dim),
        Span::styled(status, color),
    ];
    let mut lines = vec![wrap::fit(head, width, true)];
    if !task.output.is_empty() {
        lines.extend(cell_body(&task.output, Style::default(), width, expanded));
    }
    if let Some(error) = &task.diagnostic {
        lines.extend(cell_body(error, theme.error, width, expanded));
    }
    if expanded {
        lines.extend(cell_body(&usage(task), theme.dim, width, true));
    }
    lines
}

/// The dispatch's cell: a header like other tools, then a row per task with its result.
pub fn render(tool: &Tool, width: usize, theme: &Theme, view: View) -> Vec<Line<'static>> {
    let record = dispatch(tool);
    let background = record.as_ref().is_some_and(|r| r.background);
    let tasks = record.map(|r| r.tasks).unwrap_or_default();
    let title = if !tool.done() {
        "Delegating"
    } else if background {
        "Delegated in background"
    } else {
        "Delegated"
    };
    let mut lines = vec![wrap::fit(
        vec![
            header_bullet(tool, background, &tasks, theme, view),
            Span::raw(" "),
            Span::styled(title, Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(header_suffix(tool, &tasks, view), theme.dim),
        ],
        width,
        true,
    )];
    let expanded = matches!(view, View::Transcript);
    let name_width = tasks
        .iter()
        .map(|task| wrap::width(&task_label(task)))
        .max()
        .unwrap_or(0)
        .min(16.max(width / 3));
    for (index, task) in tasks.iter().enumerate() {
        lines.extend(render_task_cell(
            index, task, name_width, width, theme, expanded,
        ));
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
        let needle = self.search.to_lowercase();
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
                format!(
                    "{} {} {} {} {} {}",
                    t.id,
                    t.name.as_deref().unwrap_or_default(),
                    t.agent,
                    t.model,
                    t.task,
                    t.output
                )
                .to_lowercase()
                .contains(&needle)
            })
            .collect::<Vec<_>>();
        if self.grouped {
            tasks.sort_by_key(|(_, _, t)| t.state);
        }
        tasks
    }

    fn selected_task(&self) -> Option<(String, usize, &AgentTask)> {
        let index = self.selected.checked_sub(1)?;
        self.tasks().into_iter().nth(index)
    }

    pub fn update(&mut self, records: Vec<AgentDispatch>) {
        let selected = self.selected_task().map(|(id, index, _)| (id, index));
        self.records = records;
        if let Some(selected) = selected {
            self.selected = self
                .tasks()
                .iter()
                .position(|(id, i, _)| (id.clone(), *i) == selected)
                .map_or(0, |i| i + 1);
        }
    }

    fn active_target(&self) -> Option<(String, bool)> {
        let (dispatch_id, _, task) = self.selected_task()?;
        if !task.state.active() {
            return None;
        }
        let id = if task.id.is_empty() {
            dispatch_id
        } else {
            task.id.clone()
        };
        Some((id, task.background))
    }

    fn task_action(&self, key: KeyEvent) -> Option<Outcome> {
        let (id, background) = self
            .active_target()
            .filter(|_| !key.modifiers.contains(KeyModifiers::ALT))?;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, self.searching, control, background) {
            (KeyCode::Char('b'), false, false, false) => {
                Some(Outcome::Send(serde_json::json!({ "background": id })))
            }
            (KeyCode::Char('x'), false, _, _) | (KeyCode::Char('x'), true, true, _) => {
                Some(Outcome::Send(serde_json::json!({ "stop": id })))
            }
            _ => None,
        }
    }

    fn control_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char('f') => self.searching = !self.searching,
            KeyCode::Char('s') => {
                self.grouped = !self.grouped;
                self.selected = 0;
            }
            KeyCode::Char('t') => self.thinking = !self.thinking,
            _ => return false,
        }
        true
    }

    fn search_key(&mut self, key: KeyEvent, control: bool) {
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
    }

    fn list_key(&mut self, code: KeyCode) -> Outcome {
        let count = self.tasks().len() + 1;
        match code {
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

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Open;
        }
        if let Some(outcome) = self.task_action(key) {
            return outcome;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && self.control_key(key.code) {
            return Outcome::Open;
        }
        if self.searching {
            self.search_key(key, control);
            return Outcome::Open;
        }
        if self.expanded {
            if !self.pager.key(key) {
                self.expanded = false;
            }
            return Outcome::Open;
        }
        self.list_key(key.code)
    }

    fn list_row(
        index: usize,
        selected: usize,
        spans: Vec<Span<'static>>,
        row_width: usize,
        theme: &Theme,
    ) -> Line<'static> {
        let chosen = index == selected;
        let accent = theme.accent.add_modifier(Modifier::BOLD);
        let mut line = vec![Span::styled(if chosen { "› " } else { "  " }, accent)];
        line.extend(spans.into_iter().map(|span| {
            if chosen && span.style == Style::default() {
                Span::styled(span.content, accent)
            } else {
                span
            }
        }));
        wrap::fit(line, row_width, true)
    }

    fn split_columns(
        mut list: Vec<Line<'static>>,
        preview: &[Line<'static>],
        height: usize,
        list_width: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        list.resize(height, Line::default());
        list.into_iter()
            .enumerate()
            .map(|(index, mut line)| {
                line.spans.push(Span::raw(
                    " ".repeat(list_width.saturating_sub(line.width())),
                ));
                line.spans.push(Span::styled(" │ ", theme.dim));
                if let Some(right) = preview.get(index) {
                    line.spans.extend(right.spans.clone());
                }
                line
            })
            .collect()
    }

    fn header_lines(&self, width: usize, theme: &Theme, needs_input: bool) -> Vec<Line<'static>> {
        let mut title = vec![Span::styled(
            "Subagents",
            Style::default().add_modifier(Modifier::BOLD),
        )];
        if needs_input {
            title.push(Span::styled(" · Main needs input", theme.warning));
        }
        let mut lines = vec![wrap::fit(title, width, true)];
        if self.searching || !self.search.is_empty() {
            let cursor = if self.searching { "▏" } else { "" };
            lines.push(wrap::fit(
                vec![
                    Span::styled("Search  ", theme.dim),
                    Span::raw(self.search.clone()),
                    Span::styled(cursor, theme.accent),
                ],
                width,
                true,
            ));
        }
        lines.push(Line::default());
        lines
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
        let mut lines = self.header_lines(width, theme, needs_input);
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
            let row_width = if split { LIST } else { width };
            let list = std::iter::once(vec![Span::raw("Main session")])
                .chain(tasks.iter().map(|(_, _, task)| task_row_spans(task, theme)))
                .enumerate()
                .skip(start)
                .take(height)
                .map(|(index, spans)| Self::list_row(index, selected, spans, row_width, theme))
                .collect::<Vec<_>>();
            if split {
                lines.extend(Self::split_columns(list, &preview, height, LIST, theme));
            } else {
                lines.extend(list);
            }
        }
        lines.resize(rows.saturating_sub(footer.len()), Line::default());
        lines.extend(footer);
        lines.truncate(rows);
        lines
    }

    fn action_hints(&self) -> Vec<String> {
        let Some((_, _, task)) = self.selected_task() else {
            return Vec::new();
        };
        let mut hints = Vec::new();
        if !task.background && task.state.active() {
            hints.push("b background".into());
        }
        if task.state.active() {
            hints.push("x/ctrl+x stop".into());
        }
        hints
    }

    fn footer_segments(&self) -> Vec<String> {
        if self.searching {
            return vec!["enter done".into(), "esc done".into()];
        }
        let thinking = if self.thinking {
            "hide thinking"
        } else {
            "thinking"
        };
        let actions = self.action_hints();
        if self.expanded {
            let mut parts = vec!["↑/↓ scroll".to_owned()];
            parts.extend(actions);
            parts.push(format!("ctrl+t {thinking}"));
            parts.push("esc back".into());
            return parts;
        }
        let group = if self.grouped { "ungroup" } else { "group" };
        let mut parts = vec!["↑/↓ select".into(), "enter open".into()];
        parts.extend(actions);
        parts.extend([
            "ctrl+f search".into(),
            format!("ctrl+s {group}"),
            format!("ctrl+t {thinking}"),
            "esc back".into(),
        ]);
        parts
    }

    fn footer(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut parts = self.footer_segments();
        while parts.len() > 2 && wrap::width(&parts.join(" · ")) > width {
            parts.remove(parts.len() - 2);
        }
        if parts.len() > 1 && wrap::width(&parts.join(" · ")) > width {
            parts.remove(0);
        }
        let hint = parts.join(" · ");
        vec![Line::default(), hint_line(&hint, width, theme)]
    }
}
