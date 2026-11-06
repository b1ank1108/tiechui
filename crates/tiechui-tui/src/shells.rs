//! Shell task list and output detail; the session owns the processes and their logs.
use crate::{pager::Pager, theme::Theme, tools::plain, wrap};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui_core::text::{Line, Span};
use serde_json::{Value, json};
use tiechui_protocol::{ShellDetail, ShellMode, ShellState, ShellTask, ShellsView};

pub enum Outcome {
    Open,
    Close,
    Send(Value),
}

pub struct Panel {
    tasks: Vec<ShellTask>,
    selected: usize,
    /// The task whose output is shown, and its latest tail.
    viewing: Option<(String, Option<ShellDetail>)>,
    pager: Pager,
}

/// `1 shell`, or nothing without running background shells.
pub fn indicator(view: &ShellsView) -> Option<String> {
    match view.background() {
        0 => None,
        1 => Some("1 shell".into()),
        count => Some(format!("{count} shells")),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn status(task: &ShellTask) -> String {
    match task.exit_code {
        Some(code) if !task.state.active() => format!("exit {code}"),
        _ => task.state.label().into(),
    }
}

/// Active tasks first, each group in start order.
fn ordered(mut tasks: Vec<ShellTask>) -> Vec<ShellTask> {
    tasks.sort_by_key(|task| !task.state.active());
    tasks
}

impl Panel {
    pub fn new(view: ShellsView) -> Self {
        Self {
            tasks: ordered(view.tasks),
            selected: 0,
            viewing: None,
            pager: Pager::default(),
        }
    }

    pub fn update(&mut self, view: ShellsView) {
        let selected = self.tasks.get(self.selected).map(|task| task.id.clone());
        self.tasks = ordered(view.tasks);
        self.selected = selected
            .and_then(|id| self.tasks.iter().position(|task| task.id == id))
            .unwrap_or(self.selected)
            .min(self.tasks.len().saturating_sub(1));
        if let (Some((id, shown)), Some(detail)) = (&mut self.viewing, view.detail)
            && *id == detail.id
        {
            *shown = Some(detail);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn current(&self) -> Option<&ShellTask> {
        match &self.viewing {
            Some((id, _)) => self.tasks.iter().find(|task| &task.id == id),
            None => self.tasks.get(self.selected),
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Open;
        }
        let current = self
            .current()
            .map(|task| (task.id.clone(), task.mode, task.state));
        match (key.code, current) {
            (KeyCode::Char('x'), Some((id, _, state))) if state.active() => {
                return Outcome::Send(json!({ "stop": id }));
            }
            (
                KeyCode::Char('b'),
                Some((id, ShellMode::Foreground, ShellState::Starting | ShellState::Running)),
            ) => {
                return Outcome::Send(json!({ "background": id }));
            }
            _ => {}
        }
        if self.viewing.is_some() {
            if key.code == KeyCode::Enter || !self.pager.key(key) {
                self.viewing = None;
                return Outcome::Send(json!({ "close": true }));
            }
            return Outcome::Open;
        }
        let count = self.tasks.len().max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Close,
            KeyCode::Up | KeyCode::Char('k') => self.selected = (self.selected + count - 1) % count,
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1) % count,
            KeyCode::Enter => {
                if let Some(task) = self.tasks.get(self.selected) {
                    let id = task.id.clone();
                    self.viewing = Some((id.clone(), None));
                    self.pager = Pager::default();
                    return Outcome::Send(json!({ "detail": id }));
                }
            }
            _ => {}
        }
        Outcome::Open
    }

    pub fn render(&mut self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let now = now();
        let color = |state: ShellState| match state {
            ShellState::Failed | ShellState::TimedOut => theme.error,
            ShellState::Completed => theme.success,
            ShellState::Cancelled => theme.dim,
            _ => theme.secondary,
        };
        let fit = |spans: Vec<Span<'static>>| wrap::fit(spans, width, false);
        let height = rows.saturating_sub(3);
        let mut lines = Vec::new();
        let hints = if let Some((id, detail)) = &self.viewing {
            let task = self.tasks.iter().find(|task| &task.id == id);
            lines.push(fit(vec![
                Span::styled(format!("Shell {id}"), theme.accent),
                Span::styled(
                    task.map_or_else(String::new, |task| {
                        format!(
                            " · {} · {} · {}",
                            task.mode.label(),
                            status(task),
                            task.elapsed(now)
                        )
                    }),
                    task.map_or(theme.dim, |task| color(task.state)),
                ),
            ]));
            let mut body = Vec::new();
            if let Some(task) = task {
                body.extend(wrap::wrap(
                    &[Span::styled(
                        format!("cwd: {}", plain(&task.cwd)),
                        theme.dim,
                    )],
                    width,
                    &[],
                    &[],
                ));
                for line in plain(&task.command).split('\n') {
                    body.extend(wrap::wrap(
                        &[Span::styled(format!("$ {line}"), theme.secondary)],
                        width,
                        &[],
                        &[],
                    ));
                }
                body.push(Line::default());
            }
            match detail {
                Some(detail) if detail.output.is_empty() => {
                    body.push(Line::from(Span::styled("(no output)", theme.dim)));
                }
                Some(detail) => {
                    for line in plain(&detail.output).trim_end().split('\n') {
                        body.extend(wrap::wrap(&[Span::raw(line.to_owned())], width, &[], &[]));
                    }
                    if let Some(path) = &detail.log_path {
                        body.extend(wrap::wrap(
                            &[Span::styled(format!("Log: {}", plain(path)), theme.dim)],
                            width,
                            &[],
                            &[],
                        ));
                    }
                }
                None => body.push(Line::from(Span::styled("Loading…", theme.dim))),
            }
            lines.extend(self.pager.viewport(&body, height));
            match task {
                Some(task) if task.mode == ShellMode::Foreground && task.state.active() => {
                    "b background · x stop · Esc back"
                }
                Some(task) if task.state.active() => "x stop · Esc back",
                _ => "Esc back",
            }
        } else {
            lines.push(fit(vec![Span::styled("Shells", theme.accent)]));
            if self.tasks.is_empty() {
                lines.push(fit(vec![Span::styled(
                    "No shell tasks in this session.",
                    theme.dim,
                )]));
            }
            let start = self.selected.saturating_sub(height.saturating_sub(1));
            for (index, task) in self.tasks.iter().enumerate().skip(start).take(height) {
                let chosen = index == self.selected;
                let command = plain(task.command.lines().next().unwrap_or_default());
                lines.push(fit(vec![
                    Span::styled(
                        format!("{} {:<5}", if chosen { "›" } else { " " }, task.id),
                        if chosen { theme.accent } else { theme.dim },
                    ),
                    Span::styled(format!(" {:<11}", task.mode.label()), theme.dim),
                    Span::styled(format!("{:<10}", status(task)), color(task.state)),
                    Span::styled(format!("{:>8}  ", task.elapsed(now)), theme.dim),
                    Span::raw(command),
                ]));
            }
            "Enter view · b background · x stop · Esc close"
        };
        lines.resize(rows.saturating_sub(1), Line::default());
        lines.push(fit(vec![Span::styled(hints, theme.dim)]));
        lines.truncate(rows);
        lines
    }
}
