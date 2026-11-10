//! Shell task list over the selected task's latest output, and that output in full; the
//! session owns the processes and their logs, and sends the output of the task it watches.
use crate::{pager::Pager, sheet, theme::Theme, tools::plain, wrap};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui_core::style::{Modifier, Style};
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
    /// The task whose output the session sends, and its latest tail.
    watched: Option<String>,
    detail: Option<ShellDetail>,
    /// The selected task's output fills the panel.
    viewing: bool,
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

/// The state's mark, its color and the status label's color.
fn look(state: ShellState, theme: &Theme) -> (&'static str, Style, Style) {
    match state {
        ShellState::Completed => ("✓", theme.success, theme.success),
        ShellState::Failed | ShellState::TimedOut => ("✗", theme.error, theme.error),
        ShellState::Cancelled => ("✗", theme.dim, theme.dim),
        _ => ("●", theme.accent, theme.secondary),
    }
}

/// Holding up the turn, so it can go to the background.
fn blocking(task: &ShellTask) -> bool {
    task.mode == ShellMode::Foreground
        && matches!(task.state, ShellState::Starting | ShellState::Running)
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
            watched: None,
            detail: None,
            viewing: false,
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
        if let Some(detail) = view.detail
            && self.watched.as_ref() == Some(&detail.id)
        {
            self.detail = Some(detail);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    /// Asks for the selected task's output unless it is the one already sent.
    pub fn follow(&mut self) -> Option<Value> {
        let id = self.tasks.get(self.selected)?.id.clone();
        if self.watched.as_ref() == Some(&id) {
            return None;
        }
        self.watched = Some(id.clone());
        self.detail = None;
        Some(json!({ "detail": id }))
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Open;
        }
        if let Some(task) = self.tasks.get(self.selected) {
            match key.code {
                KeyCode::Char('x') if task.state.active() => {
                    return Outcome::Send(json!({ "stop": task.id }));
                }
                KeyCode::Char('b') if blocking(task) => {
                    return Outcome::Send(json!({ "background": task.id }));
                }
                _ => {}
            }
        }
        if self.viewing {
            if key.code == KeyCode::Enter || !self.pager.key(key) {
                self.viewing = false;
            }
            return Outcome::Open;
        }
        let count = self.tasks.len().max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.watched = None;
                self.detail = None;
                return Outcome::Close;
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = (self.selected + count - 1) % count,
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1) % count,
            KeyCode::Enter if !self.tasks.is_empty() => {
                self.viewing = true;
                self.pager = Pager::default();
            }
            _ => {}
        }
        self.follow().map_or(Outcome::Open, Outcome::Send)
    }

    /// The watched task's latest output as plain lines, or why there is none.
    fn output(&self, id: &str) -> Result<Vec<String>, &'static str> {
        match &self.detail {
            Some(detail) if detail.id == id && detail.output.is_empty() => Err("(no output)"),
            Some(detail) if detail.id == id => Ok(plain(&detail.output)
                .trim_end()
                .split('\n')
                .map(str::to_owned)
                .collect()),
            _ => Err("Loading…"),
        }
    }

    /// Exactly `rows` lines, or fewer on the smallest terminals.
    pub fn render(&mut self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let width = width.max(1);
        let room = rows.saturating_sub(3).max(1);
        self.viewing &= !self.tasks.is_empty();
        let (title, mut body, hints) = if self.viewing {
            self.page(room, width, theme)
        } else {
            self.list(room, width, theme)
        };
        if rows < 4 {
            return [title, hints][2usize.saturating_sub(rows)..].to_vec();
        }
        body.resize(room, Line::default());
        sheet::frame(theme.accent, title, body, hints, width)
    }

    fn list(
        &self,
        room: usize,
        width: usize,
        theme: &Theme,
    ) -> (Line<'static>, Vec<Line<'static>>, Line<'static>) {
        let active = self.tasks.iter().filter(|task| task.state.active()).count();
        let mut meta = format!("{} total", self.tasks.len());
        if active > 0 {
            meta = format!("{active} running · {meta}");
        }
        let title = sheet::rule(
            theme.accent,
            &[sheet::title("Shells")],
            &[vec![Span::styled(meta, theme.dim)]],
            width,
        );
        let mut pairs = vec![("enter", "open")];
        if let Some(task) = self.tasks.get(self.selected) {
            if blocking(task) {
                pairs.push(("b", "background"));
            }
            if task.state.active() {
                pairs.push(("x", "stop"));
            }
        }
        pairs.push(("esc", "close"));
        let position = (!self.tasks.is_empty())
            .then(|| format!("{} / {}", self.selected + 1, self.tasks.len()));
        let hints = sheet::hints(theme, &pairs, position.as_deref(), width);
        if self.tasks.is_empty() {
            let empty = Line::from(vec![
                Span::raw("  "),
                Span::styled("No shell tasks in this session.", theme.dim),
            ]);
            return (title, vec![empty], hints);
        }
        let now = now();
        let pane = room >= 8;
        let shown = self
            .tasks
            .len()
            .min(if pane { room / 2 } else { room })
            .max(1);
        let start = self
            .selected
            .saturating_sub(shown - 1)
            .min(self.tasks.len() - shown);
        let widest = |text: &dyn Fn(&ShellTask) -> String| {
            self.tasks
                .iter()
                .map(|task| wrap::width(&text(task)))
                .max()
                .unwrap_or(0)
        };
        let id_width = widest(&|task| task.id.clone());
        let status_width = widest(&status);
        let elapsed_width = widest(&|task| task.elapsed(now));
        let accent = theme.accent.add_modifier(Modifier::BOLD);
        let mut body = Vec::new();
        for (index, task) in self.tasks.iter().enumerate().skip(start).take(shown) {
            let selected = index == self.selected;
            let (mark, mark_style, status_style) = look(task.state, theme);
            let left = vec![
                Span::raw("  "),
                if selected {
                    Span::styled("› ", accent)
                } else {
                    Span::raw("  ")
                },
                Span::styled(
                    format!("{:<id_width$}  ", task.id),
                    if selected { theme.accent } else { theme.dim },
                ),
                Span::styled(format!("{mark} "), mark_style),
                Span::styled(format!("{:<status_width$}", status(task)), status_style),
                Span::styled(
                    format!("  {:>elapsed_width$}  ", task.elapsed(now)),
                    theme.dim,
                ),
                Span::styled(
                    plain(task.command.lines().next().unwrap_or_default()),
                    if selected { accent } else { Style::default() },
                ),
            ];
            let right = if blocking(task) {
                vec![Span::styled("foreground", theme.warning)]
            } else {
                Vec::new()
            };
            body.push(sheet::split(left, right, width));
        }
        if pane {
            let task = &self.tasks[self.selected];
            body.push(Line::default());
            body.push(wrap::fit(
                vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{} · {} · {}", task.id, task.mode.label(), plain(&task.cwd)),
                        theme.dim,
                    ),
                ],
                width,
                true,
            ));
            let rows = room.saturating_sub(body.len());
            let quote = |spans: Vec<Span<'static>>| {
                let mut line = vec![Span::raw("  "), Span::styled("│ ", theme.separator)];
                line.extend(spans);
                wrap::fit(line, width, true)
            };
            match self.output(&task.id) {
                Ok(lines) => body.extend(
                    lines[lines.len().saturating_sub(rows)..]
                        .iter()
                        .map(|line| quote(vec![Span::styled(line.clone(), theme.secondary)])),
                ),
                Err(note) => body.push(quote(vec![Span::styled(note, theme.dim)])),
            }
        }
        (title, body, hints)
    }

    fn page(
        &mut self,
        room: usize,
        width: usize,
        theme: &Theme,
    ) -> (Line<'static>, Vec<Line<'static>>, Line<'static>) {
        let task = &self.tasks[self.selected];
        let (mark, mark_style, _) = look(task.state, theme);
        let meta = vec![
            Span::styled(format!("{mark} "), mark_style),
            Span::styled(
                format!("{} · {}", status(task), task.elapsed(now())),
                theme.dim,
            ),
        ];
        let title = sheet::rule(
            theme.accent,
            &[sheet::title(format!("Shell {}", task.id))],
            &[meta],
            width,
        );
        let indent = [Span::raw("  ")];
        let mut body = sheet::command(&plain(&task.command), width, theme);
        body.extend(wrap::wrap(
            &[Span::styled(
                format!("{} · {}", plain(&task.cwd), task.mode.label()),
                theme.dim,
            )],
            width,
            &indent,
            &indent,
        ));
        body.push(Line::default());
        match self.output(&task.id) {
            Ok(lines) => {
                for line in lines {
                    body.extend(wrap::wrap(&[Span::raw(line)], width, &indent, &indent));
                }
            }
            Err(note) => body.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(note, theme.dim),
            ])),
        }
        if let Some(path) = self
            .detail
            .as_ref()
            .filter(|detail| detail.id == task.id)
            .and_then(|detail| detail.log_path.as_ref())
        {
            body.push(Line::default());
            body.extend(wrap::wrap(
                &[Span::styled(format!("Log {}", plain(path)), theme.dim)],
                width,
                &indent,
                &indent,
            ));
        }
        let mut pairs = Vec::new();
        if blocking(task) {
            pairs.push(("b", "background"));
        }
        if task.state.active() {
            pairs.push(("x", "stop"));
        }
        pairs.extend([("pgup/pgdn", "scroll"), ("esc", "back")]);
        let shown = self.pager.viewport(&body, room);
        let (_, last, count) = self.pager.position();
        let position = (count > room).then(|| format!("{last} / {count}"));
        let hints = sheet::hints(theme, &pairs, position.as_deref(), width);
        (title, shown, hints)
    }
}
