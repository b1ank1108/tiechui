//! The resume picker on the alternate screen: search, This folder/All and Updated/Created,
//! one row per session over the selected one's recent conversation, and the read-only full
//! conversation. Its reads run in the host loop and are tagged with the picker's generation,
//! so a closed or replaced picker never sees late results.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::text::{Line, Span};
use tokio_util::sync::CancellationToken;
use unicode_segmentation::UnicodeSegmentation;

use crate::prompt::Input;
use crate::sheet;
use crate::status::compact_path;
use crate::theme::Theme;
use crate::tools::plain;
use crate::{markdown, wrap};

/// Conversation rows under the list, and the rows its details add.
const TAIL: usize = 8;
const DETAILS: usize = 5;

pub enum Outcome {
    Open,
    Pick(PathBuf),
    Cancel,
    Quit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    /// Sessions in the working directory, or in every directory.
    List {
        all: bool,
    },
    Preview(PathBuf),
}

/// Reads for one picker, cancelled when it closes.
pub struct Reads {
    pub generation: u64,
    pub cancel: CancellationToken,
    pub reads: Vec<Read>,
}

/// Storage-independent session information supplied by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub path: PathBuf,
    pub id: String,
    pub cwd: PathBuf,
    pub created_at: u64,
    pub modified: u64,
    pub first_message: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartKind {
    User,
    Assistant,
    Tool,
    Notice,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub kind: PartKind,
    pub text: String,
}

pub enum Loaded {
    List {
        all: bool,
        sessions: Result<Vec<Summary>, String>,
    },
    Preview {
        path: PathBuf,
        parts: Result<Vec<Part>, String>,
    },
}

enum Preview {
    Loading,
    Ready(Vec<Part>),
    Failed(String),
}

/// The full conversation, shown once its read is ready.
struct Full {
    path: PathBuf,
    title: String,
    id: String,
    ready: bool,
}

pub struct Resume {
    pub generation: u64,
    cancel: CancellationToken,
    /// Opened at startup: leaving starts a new session.
    fresh: bool,
    cwd: PathBuf,
    home: Option<PathBuf>,
    reference: u64,
    query: Input,
    all: bool,
    created: bool,
    sort_focused: bool,
    details: bool,
    lists: [Option<Vec<Summary>>; 2],
    loading: [bool; 2],
    previews: HashMap<PathBuf, Preview>,
    /// Indices into the current list, in display order.
    filtered: Vec<usize>,
    selected: usize,
    /// Whether the user moved the selection, which then survives sessions arriving.
    touched: bool,
    top: usize,
    height: usize,
    full: Option<Full>,
    scroll: usize,
    lines: usize,
    error: Option<String>,
    reads: Vec<Read>,
}

impl Drop for Resume {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Resume {
    /// `reference` is the current time in Unix milliseconds, for relative dates.
    pub fn new(
        generation: u64,
        fresh: bool,
        cwd: PathBuf,
        home: Option<PathBuf>,
        reference: u64,
    ) -> Self {
        Self {
            generation,
            cancel: CancellationToken::new(),
            fresh,
            cwd,
            home,
            reference,
            query: Input::default(),
            all: false,
            created: false,
            sort_focused: false,
            details: false,
            lists: [None, None],
            loading: [true, false],
            previews: HashMap::new(),
            filtered: Vec::new(),
            selected: 0,
            touched: false,
            top: 0,
            height: 10,
            full: None,
            scroll: 0,
            lines: 0,
            error: None,
            reads: vec![Read::List { all: false }],
        }
    }

    /// Cancelled when the picker closes.
    pub fn cancel(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub fn take_reads(&mut self) -> Vec<Read> {
        std::mem::take(&mut self.reads)
    }

    pub fn loaded(&mut self, loaded: Loaded) {
        match loaded {
            Loaded::List { all, sessions } => {
                self.loading[usize::from(all)] = false;
                match sessions {
                    Ok(sessions) => {
                        self.lists[usize::from(all)] = Some(sessions);
                        if all == self.all {
                            self.reconcile(true);
                            self.follow();
                        }
                    }
                    Err(error) if all == self.all => {
                        self.error =
                            Some(format!("Failed to load sessions: {}", single_line(&error)));
                    }
                    Err(_) => {}
                }
            }
            Loaded::Preview { path, parts } => {
                // A preview dropped since it was asked for is not wanted any more.
                let Some(preview) = self.previews.get_mut(&path) else {
                    return;
                };
                let waiting = self
                    .full
                    .as_mut()
                    .filter(|full| full.path == path && !full.ready);
                match parts {
                    Ok(parts) => {
                        *preview = Preview::Ready(parts);
                        if let Some(full) = waiting {
                            full.ready = true;
                            self.scroll = 0;
                        }
                    }
                    Err(error) => {
                        let error = single_line(&error);
                        if waiting.is_some() {
                            self.full = None;
                            self.error = Some(format!("Failed to read conversation: {error}"));
                        }
                        *preview = Preview::Failed(error);
                    }
                }
            }
        }
    }

    fn list(&self) -> &[Summary] {
        self.lists[usize::from(self.all)]
            .as_deref()
            .unwrap_or_default()
    }

    fn current(&self) -> Option<&Summary> {
        self.filtered
            .get(self.selected)
            .and_then(|index| self.list().get(*index))
    }

    fn reconcile(&mut self, preserve: bool) {
        let path = self.current().map(|s| s.path.clone());
        self.refilter(preserve, path);
    }

    /// Refilters, staying on `path` when the user moved the selection.
    fn refilter(&mut self, preserve: bool, path: Option<PathBuf>) {
        let keep = preserve && self.touched;
        let path = path.filter(|_| keep);
        self.filtered = matches(self.list(), self.query.text(), self.created);
        let list = self.list();
        let found = path.and_then(|path| self.filtered.iter().position(|i| list[*i].path == path));
        let wanted = if keep { self.selected } else { 0 };
        self.selected = found.unwrap_or(wanted.min(self.filtered.len().saturating_sub(1)));
        if !preserve {
            self.top = 0;
            self.touched = false;
        }
    }

    fn request(&mut self, path: &Path) {
        if matches!(self.previews.get(path), None | Some(Preview::Failed(_))) {
            self.previews.insert(path.to_owned(), Preview::Loading);
            self.reads.push(Read::Preview(path.to_owned()));
        }
    }

    /// Asks once for the selected session's conversation, shown under the list.
    fn follow(&mut self) {
        if let Some(path) = self.current().map(|s| s.path.clone())
            && !self.previews.contains_key(&path)
        {
            self.request(&path);
        }
    }

    fn toggle_option(&mut self) {
        self.touched = true;
        let path = self.current().map(|s| s.path.clone());
        if self.sort_focused {
            self.created = !self.created;
        } else {
            self.all = !self.all;
            let scope = usize::from(self.all);
            if self.lists[scope].is_none() && !self.loading[scope] {
                self.loading[scope] = true;
                self.reads.push(Read::List { all: self.all });
            }
        }
        self.error = None;
        self.refilter(true, path);
        self.top = 0;
    }

    pub fn paste(&mut self, text: &str) {
        if self.full.is_none() {
            self.query.paste(text);
            self.reconcile(false);
            self.follow();
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('c') {
            return Outcome::Quit;
        }
        self.error = None;
        if key.code == KeyCode::Esc {
            if self.full.take().is_none() {
                if self.query.text().is_empty() {
                    return Outcome::Cancel;
                }
                self.touched = true;
                self.query.clear();
                self.reconcile(true);
                self.follow();
            }
            return Outcome::Open;
        }
        if let Some(full) = &self.full {
            if full.ready {
                let page = self.height.max(1);
                match key.code {
                    KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
                    KeyCode::Down => self.scroll += 1,
                    KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(page),
                    KeyCode::Char('u') if control => self.scroll = self.scroll.saturating_sub(page),
                    KeyCode::PageDown => self.scroll += page,
                    KeyCode::Char('d') if control => self.scroll += page,
                    KeyCode::Home => self.scroll = 0,
                    KeyCode::End => self.scroll = self.lines,
                    _ => {}
                }
            }
            return Outcome::Open;
        }
        let step = self.height.max(1);
        let last = self.filtered.len().saturating_sub(1);
        match key.code {
            KeyCode::Enter => {
                if let Some(session) = self.current() {
                    return Outcome::Pick(session.path.clone());
                }
            }
            KeyCode::Tab | KeyCode::BackTab => self.sort_focused = !self.sort_focused,
            KeyCode::Left | KeyCode::Right => self.toggle_option(),
            KeyCode::Char('t') if control => {
                self.touched = true;
                if let Some(session) = self.current() {
                    let full = Full {
                        path: session.path.clone(),
                        title: title(session),
                        id: session.id.clone(),
                        ready: matches!(self.previews.get(&session.path), Some(Preview::Ready(_))),
                    };
                    self.request(&full.path.clone());
                    self.full = Some(full);
                    self.scroll = 0;
                }
            }
            KeyCode::Char('e') if control => self.details = !self.details,
            KeyCode::Up => self.select(self.selected.saturating_sub(1)),
            KeyCode::Down => self.select(self.selected + 1),
            KeyCode::PageUp => self.select(self.selected.saturating_sub(step)),
            KeyCode::Char('u') if control => self.select(self.selected.saturating_sub(step)),
            KeyCode::PageDown => self.select(self.selected + step),
            KeyCode::Char('d') if control => self.select(self.selected + step),
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(last),
            _ => {
                if self.query.key(&key) {
                    self.reconcile(false);
                }
            }
        }
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.follow();
        Outcome::Open
    }

    fn select(&mut self, index: usize) {
        self.touched = true;
        self.selected = index;
    }

    /// Exactly `rows` lines, or fewer on the smallest terminals.
    pub fn render(&mut self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let width = width.max(1);
        let title = self.rule(width, theme);
        if rows < 4 {
            return [title, self.hints(width, theme)][2usize.saturating_sub(rows)..].to_vec();
        }
        let room = rows - 3;
        let mut body = Vec::new();
        if let Some(error) = &self.error
            && room >= 4
        {
            body.push(wrap::fit(
                vec![Span::raw("  "), Span::styled(error.clone(), theme.error)],
                width,
                true,
            ));
        }
        if self.full.is_none() {
            let toolbar = self.toolbar(theme);
            let toolbar_width: usize = toolbar.iter().map(|span| wrap::width(&span.content)).sum();
            if width >= toolbar_width + 24 {
                let search = self.search(width - toolbar_width - 2, theme);
                body.push(sheet::split(search, toolbar, width));
            } else {
                body.push(Line::from(self.search(width, theme)));
                if room >= 8 {
                    let mut line = vec![Span::raw("  ")];
                    line.extend(toolbar);
                    body.push(wrap::fit(line, width, true));
                }
            }
            if room >= 6 {
                body.push(Line::default());
            }
        }
        let height = room.saturating_sub(body.len()).max(1);
        self.height = height;
        let rest = match &self.full {
            Some(full) if !full.ready => vec![Line::from(vec![
                Span::raw("  "),
                Span::styled("Loading conversation…", theme.dim),
            ])],
            Some(full) => {
                let lines = self.conversation(&full.path.clone(), width, theme);
                self.lines = lines.len();
                self.scroll = self.scroll.min(lines.len().saturating_sub(height));
                lines.into_iter().skip(self.scroll).take(height).collect()
            }
            None if self.filtered.is_empty() => {
                let text = if self.loading[usize::from(self.all)] {
                    "Loading sessions…"
                } else if !self.query.text().is_empty() {
                    "No matching sessions"
                } else {
                    "No sessions yet"
                };
                vec![Line::from(vec![
                    Span::raw("  "),
                    Span::styled(text, theme.dim),
                ])]
            }
            None => self.sessions(height, width, theme),
        };
        body.extend(rest);
        body.resize(room, Line::default());
        let hints = self.hints(width, theme);
        sheet::frame(theme.accent, title, body, hints, width)
    }

    fn rule(&self, width: usize, theme: &Theme) -> Line<'static> {
        if let Some(full) = &self.full {
            let mut title = sheet::title("Transcript");
            title.extend([
                Span::styled(" · ", theme.dim),
                Span::raw(full.title.clone()),
            ]);
            let meta = vec![Span::styled(full.id.clone(), theme.dim)];
            return sheet::rule(theme.accent, &[title], &[meta], width);
        }
        let folder = if self.all {
            "all folders".to_owned()
        } else {
            compact_path(&self.cwd, self.home.as_deref())
        };
        let count = self.lists[usize::from(self.all)]
            .is_some()
            .then(|| self.filtered.len().to_string());
        let meta = [Some(folder).filter(|f| !f.is_empty()), count]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        sheet::rule(
            theme.accent,
            &[sheet::title("Resume")],
            &[vec![Span::styled(meta, theme.dim)]],
            width,
        )
    }

    fn hints(&self, width: usize, theme: &Theme) -> Line<'static> {
        if self.full.is_some() {
            let position = format!("{} / {}", self.scroll + 1, self.lines.max(1));
            return sheet::hints(
                theme,
                &[
                    ("↑↓", "scroll"),
                    ("pgup/pgdn", "page"),
                    ("home/end", "jump"),
                    ("esc", "back"),
                ],
                Some(&position),
                width,
            );
        }
        let back = if self.fresh { "start new" } else { "back" };
        let position = (!self.filtered.is_empty())
            .then(|| format!("{} / {}", self.selected + 1, self.filtered.len()));
        sheet::hints(
            theme,
            &[
                ("enter", "resume"),
                ("tab", "filter/sort"),
                ("←→", "change"),
                ("ctrl+t", "transcript"),
                ("ctrl+e", "details"),
                ("esc", back),
            ],
            position.as_deref(),
            width,
        )
    }

    /// `Search` and the query with a block cursor, which stays visible.
    fn search(&self, width: usize, theme: &Theme) -> Vec<Span<'static>> {
        let room = width.saturating_sub(10);
        let (before, after) = self.query.text().split_at(self.query.cursor());
        let mut after = after.graphemes(true);
        let under = after.next().unwrap_or(" ").to_owned();
        let mut before = before.to_owned();
        while wrap::width(&before) + wrap::width(&under) > room {
            before.remove(0);
        }
        let mut after = after.as_str().to_owned();
        while wrap::width(&before) + wrap::width(&under) + wrap::width(&after) > room {
            after.pop();
        }
        vec![
            Span::raw("  "),
            Span::styled("Search  ", theme.dim),
            Span::raw(before),
            Span::styled(under, Style::default().add_modifier(Modifier::REVERSED)),
            Span::raw(after),
        ]
    }

    /// The current value bold, in the accent where focused; the others dim.
    fn toolbar(&self, theme: &Theme) -> Vec<Span<'static>> {
        let value = |name: &'static str, active: bool, focused: bool| match (active, focused) {
            (true, true) => Span::styled(name, theme.accent.add_modifier(Modifier::BOLD)),
            (true, false) => Span::styled(name, Style::default().add_modifier(Modifier::BOLD)),
            (false, _) => Span::styled(name, theme.dim),
        };
        vec![
            value("This folder", !self.all, !self.sort_focused),
            Span::raw("  "),
            value("All", self.all, !self.sort_focused),
            Span::raw("    "),
            value("Updated", !self.created, self.sort_focused),
            Span::raw("  "),
            value("Created", self.created, self.sort_focused),
        ]
    }

    /// One row per session around the selection, then the selected one's conversation when
    /// there is room or its details are asked for.
    fn sessions(&mut self, height: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let cap = TAIL + if self.details { DETAILS } else { 0 };
        let pane = height >= 9 || self.details && height >= 5;
        let reserve = if pane {
            (height / 3).clamp(3, cap) + 1
        } else {
            0
        };
        let shown = self
            .filtered
            .len()
            .min(height.saturating_sub(reserve))
            .max(1);
        self.height = shown;
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + shown {
            self.top = self.selected + 1 - shown;
        }
        self.top = self.top.min(self.filtered.len().saturating_sub(shown));
        let list = self.list();
        let visible: Vec<&Summary> = self.filtered[self.top..self.top + shown]
            .iter()
            .map(|index| &list[*index])
            .collect();
        let home = self.home.as_deref();
        let folders: Vec<String> = visible
            .iter()
            .filter(|_| self.all)
            .map(|session| match session.cwd.file_name() {
                Some(name) if width < 80 => name.to_string_lossy().into_owned(),
                _ => compact_path(&session.cwd, home),
            })
            .collect();
        let folder_width = folders
            .iter()
            .map(|folder| wrap::width(folder))
            .max()
            .unwrap_or(0)
            .min(28);
        let dates: Vec<String> = visible
            .iter()
            .map(|session| {
                let time = if self.created {
                    session.created_at
                } else {
                    session.modified
                };
                relative(time, self.reference)
            })
            .collect();
        let date_width = dates.iter().map(|date| date.len()).max().unwrap_or(0);
        let accent = theme.accent.add_modifier(Modifier::BOLD);
        let mut lines = Vec::new();
        for (offset, session) in visible.iter().enumerate() {
            let selected = self.top + offset == self.selected;
            let left = vec![
                Span::raw("  "),
                if selected {
                    Span::styled("› ", accent)
                } else {
                    Span::raw("  ")
                },
                Span::styled(
                    title(session),
                    if selected { accent } else { Style::default() },
                ),
            ];
            let mut right = Vec::new();
            if let Some(folder) = folders.get(offset) {
                let cell = wrap::fit(
                    vec![Span::styled(folder.clone(), theme.dim)],
                    folder_width,
                    true,
                );
                let used = wrap::line_width(&cell);
                right.extend(cell.spans);
                right.push(Span::raw(" ".repeat(folder_width - used + 2)));
            }
            right.push(Span::styled(
                format!("{:>date_width$}", dates[offset]),
                theme.dim,
            ));
            lines.push(sheet::split(left, right, width));
        }
        if pane {
            lines.push(Line::default());
            let rows = height.saturating_sub(shown + 1).min(cap);
            lines.extend(self.preview(rows, width, theme));
        }
        lines
    }

    /// The selected session's details when asked for, then its conversation's last rows.
    fn preview(&self, rows: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(session) = self.current() else {
            return Vec::new();
        };
        let quote = |spans: Vec<Span<'static>>| {
            let mut line = vec![Span::raw("  "), Span::styled("│ ", theme.separator)];
            line.extend(spans);
            wrap::fit(line, width, true)
        };
        let mut lines = Vec::new();
        if self.details {
            let metadata = [
                ("Session", session.id.clone()),
                ("Created", date_time(session.created_at, self.reference)),
                ("Updated", date_time(session.modified, self.reference)),
                (
                    "Directory",
                    compact_path(&session.cwd, self.home.as_deref()),
                ),
            ];
            for (key, value) in metadata {
                lines.push(quote(vec![
                    Span::styled(format!("{:<12}", format!("{key}:")), theme.dim),
                    Span::raw(single_line(&value)),
                ]));
            }
            lines.push(quote(Vec::new()));
        }
        let room = rows.saturating_sub(lines.len());
        let tail = self.tail(&session.path, room, width.saturating_sub(4).max(1), theme);
        lines.extend(tail.into_iter().map(|line| quote(line.spans)));
        lines.truncate(rows);
        lines
    }

    /// The conversation's last `rows` rows: the user's words in the secondary tone, the rest
    /// dim.
    fn tail(&self, path: &Path, rows: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let notice = |text: String, style: Style| vec![Line::from(Span::styled(text, style))];
        let parts = match self.previews.get(path) {
            None | Some(Preview::Loading) => {
                return notice("Loading conversation…".into(), theme.dim);
            }
            Some(Preview::Failed(error)) => {
                return notice(format!("Unable to read conversation: {error}"), theme.error);
            }
            Some(Preview::Ready(parts)) => parts,
        };
        let inner = width.saturating_sub(2).max(1);
        let mut lines = Vec::new();
        for part in parts.iter().rev() {
            if lines.len() >= rows {
                break;
            }
            let (marker, style) = if part.kind == PartKind::User {
                (
                    Span::styled("› ", theme.secondary),
                    theme.secondary.add_modifier(Modifier::ITALIC),
                )
            } else {
                (Span::styled("• ", theme.dim), theme.dim)
            };
            let rows: Vec<Line<'static>> = layout(part, inner, theme)
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    let mut spans = vec![if index == 0 {
                        marker.clone()
                    } else {
                        Span::raw("  ")
                    }];
                    spans.extend(
                        line.spans
                            .into_iter()
                            .map(|span| Span::styled(span.content, span.style.patch(style))),
                    );
                    Line::from(spans)
                })
                .collect();
            lines.splice(0..0, rows);
        }
        if lines.is_empty() {
            return notice("No conversation messages yet.".into(), theme.dim);
        }
        lines.split_off(lines.len().saturating_sub(rows))
    }

    fn conversation(&self, path: &Path, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(Preview::Ready(parts)) = self.previews.get(path) else {
            return Vec::new();
        };
        let inner = width.saturating_sub(2).max(1);
        let mut lines = Vec::new();
        for part in parts {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let marker = match part.kind {
                PartKind::User => Span::styled(
                    "› ",
                    Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
                ),
                PartKind::Assistant => Span::styled("• ", theme.dim),
                PartKind::Tool | PartKind::Notice => Span::raw("  "),
            };
            let quiet = matches!(part.kind, PartKind::Tool | PartKind::Notice);
            for (index, line) in layout(part, inner, theme).into_iter().enumerate() {
                let mut spans = vec![if index == 0 {
                    marker.clone()
                } else {
                    Span::raw("  ")
                }];
                spans.extend(line.spans.into_iter().map(|span| {
                    if quiet {
                        Span::styled(span.content, theme.dim.patch(span.style))
                    } else {
                        span
                    }
                }));
                let line = wrap::fit(spans, width, true);
                lines.push(if part.kind == PartKind::User {
                    pad(line, width, theme.user())
                } else {
                    line
                });
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "No conversation messages yet.",
                theme.dim,
            )));
        }
        lines
    }
}

fn pad(mut line: Line<'static>, width: usize, base: Style) -> Line<'static> {
    let used = wrap::line_width(&line);
    line.spans
        .push(Span::raw(" ".repeat(width.saturating_sub(used))));
    for span in &mut line.spans {
        span.style = base.patch(span.style);
    }
    line
}

/// A part's rows: Markdown for the assistant, plain wrapped text for the rest.
fn layout(part: &Part, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let text = plain(&part.text);
    if part.kind == PartKind::Assistant {
        let mut lines = markdown::render(&text, width, theme);
        while lines.last().is_some_and(|line| wrap::line_width(line) == 0) {
            lines.pop();
        }
        return lines;
    }
    text.split('\n')
        .flat_map(|line| wrap::wrap(&[Span::raw(line.to_owned())], width, &[], &[]))
        .collect()
}

/// Every word of the query in the session's text; most recent first, then by path.
fn matches(sessions: &[Summary], query: &str, created: bool) -> Vec<usize> {
    let words: Vec<String> = single_line(query)
        .to_lowercase()
        .split(' ')
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect();
    let mut found: Vec<usize> = (0..sessions.len())
        .filter(|index| {
            let session = &sessions[*index];
            let text = format!(
                "{} {} {} {}",
                session.first_message,
                session.cwd.display(),
                session.id,
                session.text
            )
            .to_lowercase();
            words.iter().all(|word| text.contains(word.as_str()))
        })
        .collect();
    let time = |session: &Summary| {
        if created {
            session.created_at
        } else {
            session.modified
        }
    };
    found.sort_by(|a, b| {
        let (a, b) = (&sessions[*a], &sessions[*b]);
        time(b).cmp(&time(a)).then_with(|| a.path.cmp(&b.path))
    });
    found
}

fn single_line(text: &str) -> String {
    plain(text).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn title(session: &Summary) -> String {
    let title = single_line(&session.first_message);
    if title.is_empty() {
        single_line(&session.id)
    } else {
        title
    }
}

/// `key label` segments: the key at full strength, its label dim.
pub(crate) fn hint_line(text: &str, width: usize, theme: &Theme) -> Line<'static> {
    let light = theme.palette.light();
    let (key, label) = if light {
        (
            Style::default().fg(Color::Indexed(0)),
            Style::default().fg(Color::Indexed(8)),
        )
    } else {
        (
            Style::default(),
            Style::default().add_modifier(Modifier::DIM),
        )
    };
    let mut spans = Vec::new();
    for (index, segment) in text.split(" · ").enumerate() {
        if index > 0 {
            spans.push(Span::styled(" · ", label));
        }
        let (name, rest) = segment
            .find(' ')
            .map_or((segment, ""), |at| segment.split_at(at));
        spans.push(Span::styled(name.to_owned(), key));
        spans.push(Span::styled(rest.to_owned(), label));
    }
    wrap::fit(spans, width, true)
}

/// `now`, minutes, hours or days ago, then the date after 30 days.
pub fn relative(time: u64, reference: u64) -> String {
    if time == 0 {
        return "unknown".into();
    }
    let minutes = reference.saturating_sub(time) / 60_000;
    match minutes {
        0 => "now".into(),
        1..60 => format!("{minutes}m ago"),
        60..1440 => format!("{}h ago", minutes / 60),
        1440..43200 => format!("{}d ago", minutes / 1440),
        _ => {
            let (year, month, day) = civil(time);
            format!("{year:04}-{month:02}-{day:02}")
        }
    }
}

fn date_time(time: u64, reference: u64) -> String {
    if time == 0 {
        return "unknown".into();
    }
    format!("{} · {}", relative(time, reference), local(time))
}

/// The UTC calendar date of a Unix millisecond time.
fn civil(time: u64) -> (i64, u32, u32) {
    let days = i64::try_from(time / 86_400_000).unwrap_or(0) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted + 2) / 5 + 1) as u32;
    let month = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `YYYY-MM-DD HH:MM:SS` in local time.
fn local(time: u64) -> String {
    {
        let seconds = libc::time_t::try_from(time / 1000).unwrap_or_default();
        // SAFETY: `tm` is plain data that the converter fills; both pointers outlive the call.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        #[cfg(unix)]
        let converted = !unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null();
        #[cfg(windows)]
        let converted = unsafe { libc::localtime_s(&mut tm, &seconds) } == 0;
        if converted {
            return format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                tm.tm_year + 1900,
                tm.tm_mon + 1,
                tm.tm_mday,
                tm.tm_hour,
                tm.tm_min,
                tm.tm_sec
            );
        }
    }
    let (year, month, day) = civil(time);
    let seconds = time / 1000 % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_relative_then_absolute() {
        let reference = 1_800_000_000_000;
        let ago = |minutes: u64| relative(reference - minutes * 60_000, reference);
        assert_eq!(
            [ago(0), ago(5), ago(125), ago(3 * 1440), ago(31 * 1440)],
            ["now", "5m ago", "2h ago", "3d ago", "2026-12-15"]
        );
        assert_eq!(relative(0, reference), "unknown");
        assert_eq!(civil(951_782_400_000), (2000, 2, 29));
    }
}
