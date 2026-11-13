//! The resume picker on the alternate screen: search, This folder/All and Updated/Created,
//! the sessions beside the selected one's turns (above them on narrow terminals), and the
//! read-only full conversation. Its reads run in the host loop and are tagged with the
//! picker's generation, so a closed or replaced picker never sees late results.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::text::{Line, Span};
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use unicode_segmentation::UnicodeSegmentation;

use crate::prompt::Input;
use crate::sheet;
use crate::status::compact_path;
use crate::theme::Theme;
use crate::tools::{self, Tool, plain};
use crate::{markdown, wrap};

/// The narrowest terminal that shows the sessions beside the conversation.
const SPLIT: usize = 100;
/// Rows of a user message and of a reply in the preview.
const ASKED: usize = 3;
const REPLY: usize = 8;

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

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    User(String),
    Assistant(String),
    /// A tool the assistant called.
    Call {
        name: String,
        arguments: Value,
    },
    /// What a tool returned.
    Output(String),
    /// A compaction's summary of the messages it replaced.
    Compaction(String),
    Notice(String),
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
        let count = self.lists[usize::from(self.all)].is_some().then(|| {
            let count = self.filtered.len();
            format!("{count} {}", plural(count, "session"))
        });
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
                ("tab", "scope/sort"),
                ("←→", "toggle"),
                ("ctrl+t", "transcript"),
                ("esc", back),
            ],
            position.as_deref(),
            width,
        )
    }

    /// `Search` and the query with a block cursor, which stays visible; a hint while empty.
    fn search(&self, width: usize, theme: &Theme) -> Vec<Span<'static>> {
        if self.query.text().is_empty() {
            return vec![
                Span::raw("  "),
                Span::styled("Search  ", theme.dim),
                Span::styled("t", Style::default().add_modifier(Modifier::REVERSED)),
                Span::styled("ype to filter…", theme.dim.add_modifier(Modifier::ITALIC)),
            ];
        }
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

    /// `Scope ● This folder ○ All    Sort ● Updated ○ Created`, the focused group's label in
    /// the accent.
    fn toolbar(&self, theme: &Theme) -> Vec<Span<'static>> {
        let group = |label: &str, options: [&'static str; 2], active: bool, focused: bool| {
            let mut spans = vec![Span::styled(
                format!("{label} "),
                if focused {
                    theme.accent.add_modifier(Modifier::BOLD)
                } else {
                    theme.dim
                },
            )];
            for (index, option) in options.into_iter().enumerate() {
                if index > 0 {
                    spans.push(Span::raw("  "));
                }
                if (index == 1) == active {
                    let dot = if focused {
                        theme.accent
                    } else {
                        Style::default()
                    };
                    spans.push(Span::styled("● ", dot));
                    spans.push(Span::styled(
                        option,
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                } else {
                    spans.push(Span::styled("○ ", theme.dim));
                    spans.push(Span::styled(option, theme.dim));
                }
            }
            spans
        };
        let mut spans = group(
            "Scope",
            ["This folder", "All"],
            self.all,
            !self.sort_focused,
        );
        spans.push(Span::raw("    "));
        spans.extend(group(
            "Sort",
            ["Updated", "Created"],
            self.created,
            self.sort_focused,
        ));
        spans
    }

    /// The list beside the selected session on wide terminals, above its turns otherwise.
    fn sessions(&mut self, height: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        if width >= SPLIT && height >= 6 {
            return self.columns(height, width, theme);
        }
        let pane = height >= 9;
        let reserve = if pane { height / 2 } else { 0 };
        let shown = self.scroll_to(height.saturating_sub(reserve), 1);
        let visible = self.visible(shown);
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
        let dates: Vec<String> = visible.iter().map(|s| self.when(s)).collect();
        let date_width = dates
            .iter()
            .map(|date| wrap::width(date))
            .max()
            .unwrap_or(0);
        let mut lines = Vec::new();
        for (offset, session) in visible.iter().enumerate() {
            let selected = self.top + offset == self.selected;
            let tone = if selected { theme.secondary } else { theme.dim };
            let mut right = Vec::new();
            if let Some(folder) = folders.get(offset) {
                let cell = wrap::fit(vec![Span::styled(folder.clone(), tone)], folder_width, true);
                let used = wrap::line_width(&cell);
                right.extend(cell.spans);
                right.push(Span::raw(" ".repeat(folder_width - used + 2)));
            }
            right.push(Span::styled(
                format!("{:>date_width$}", dates[offset]),
                tone,
            ));
            right.push(Span::raw("  "));
            let line = sheet::split(heading(session, selected, theme), right, width);
            lines.push(if selected {
                pad(line, width, theme.selection())
            } else {
                line
            });
        }
        if pane {
            lines.push(Line::default());
            if let Some(session) = self.current() {
                let mut meta = self.turn_count(&session.path);
                meta.push(format!(
                    "created {}",
                    relative(session.created_at, self.reference)
                ));
                let mut line = vec![Span::raw("  ")];
                line.extend(
                    sheet::rule(
                        theme.separator,
                        &[vec![Span::styled(
                            "Preview",
                            theme.secondary.add_modifier(Modifier::BOLD),
                        )]],
                        &[vec![Span::styled(meta.join(" · "), theme.dim)]],
                        width.saturating_sub(2),
                    )
                    .spans,
                );
                lines.push(Line::from(line));
                let path = session.path.clone();
                let rows = height.saturating_sub(shown + 2);
                let inner = width.saturating_sub(2).max(1);
                lines.extend(
                    self.digest(&path, inner, rows, theme)
                        .into_iter()
                        .map(|line| {
                            let mut spans = vec![Span::raw("  ")];
                            spans.extend(line.spans);
                            Line::from(spans)
                        }),
                );
            }
        }
        lines
    }

    /// Two rows per session on the left; the selected one's title, details and turns on the
    /// right.
    fn columns(&mut self, height: usize, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let left = width * 12 / 25;
        let right = width - left - 3;
        let shown = self.scroll_to(height, 2);
        let mut rows = Vec::new();
        for (offset, session) in self.visible(shown).into_iter().enumerate() {
            let selected = self.top + offset == self.selected;
            let when = self.when(session);
            let meta = match session.cwd.file_name().filter(|_| self.all) {
                Some(folder) => format!("{} · {when}", folder.to_string_lossy()),
                None => when,
            };
            let base = if selected {
                theme.selection()
            } else {
                Style::default()
            };
            let tone = if selected { theme.secondary } else { theme.dim };
            rows.push(pad(
                wrap::fit(heading(session, selected, theme), left - 1, true),
                left,
                base,
            ));
            rows.push(pad(
                wrap::fit(
                    vec![Span::raw("    "), Span::styled(meta, tone)],
                    left - 1,
                    true,
                ),
                left,
                base,
            ));
        }
        let detail = self.detail(right, height, theme);
        (0..height)
            .map(|index| {
                let mut line = rows
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| Line::from(" ".repeat(left)));
                line.spans.push(Span::styled(" │ ", theme.separator));
                if let Some(row) = detail.get(index) {
                    line.spans
                        .extend(wrap::fit(row.spans.clone(), right, true).spans);
                }
                line
            })
            .collect()
    }

    /// Keeps the selection among `rows / per` visible sessions; returns how many there are.
    fn scroll_to(&mut self, rows: usize, per: usize) -> usize {
        let shown = self.filtered.len().min(rows / per).max(1);
        self.height = shown;
        if self.selected < self.top {
            self.top = self.selected;
        } else if self.selected >= self.top + shown {
            self.top = self.selected + 1 - shown;
        }
        self.top = self.top.min(self.filtered.len().saturating_sub(shown));
        shown
    }

    fn visible(&self, shown: usize) -> Vec<&Summary> {
        let list = self.list();
        let end = (self.top + shown).min(self.filtered.len());
        self.filtered[self.top..end]
            .iter()
            .map(|index| &list[*index])
            .collect()
    }

    fn when(&self, session: &Summary) -> String {
        let time = if self.created {
            session.created_at
        } else {
            session.modified
        };
        relative(time, self.reference)
    }

    /// `N turns`, once the conversation is read.
    fn turn_count(&self, path: &Path) -> Vec<String> {
        match self.previews.get(path) {
            Some(Preview::Ready(parts)) => {
                let count = turns(parts).len();
                vec![format!("{count} {}", plural(count, "turn"))]
            }
            _ => Vec::new(),
        }
    }

    /// The selected session's title, where and when, then its turns.
    fn detail(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(session) = self.current() else {
            return Vec::new();
        };
        let title = wrap::wrap(
            &[Span::styled(
                title(session),
                Style::default().add_modifier(Modifier::BOLD),
            )],
            width,
            &[],
            &[],
        );
        let mut lines = clip(title, 2, width);
        let mut meta = vec![compact_path(&session.cwd, self.home.as_deref())];
        meta.extend(self.turn_count(&session.path));
        meta.push(format!(
            "updated {}",
            relative(session.modified, self.reference)
        ));
        meta.push(format!(
            "created {}",
            relative(session.created_at, self.reference)
        ));
        lines.push(wrap::fit(
            vec![Span::styled(meta.join(" · "), theme.dim)],
            width,
            true,
        ));
        lines.push(Line::from(Span::styled("─".repeat(width), theme.separator)));
        let room = rows.saturating_sub(lines.len());
        lines.extend(self.digest(&session.path, width, room, theme));
        lines
    }

    /// The newest turns that fit in `rows`, oldest first, under a count of the earlier ones.
    fn digest(&self, path: &Path, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
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
        let mut blocks: Vec<Vec<Line<'static>>> = turns(parts)
            .iter()
            .map(|turn| turn.render(width, theme))
            .collect();
        let Some(newest) = blocks.last() else {
            return notice("No conversation messages yet.".into(), theme.dim);
        };
        let mut start = blocks.len();
        let mut used = 0;
        while start > 0 {
            let need = blocks[start - 1].len() + usize::from(start < blocks.len());
            let header = if start > 1 { 2 } else { 0 };
            if used + need + header > rows {
                break;
            }
            used += need;
            start -= 1;
        }
        if start == blocks.len() {
            return newest.iter().take(rows).cloned().collect();
        }
        let mut lines = Vec::new();
        if start > 0 {
            lines.push(Line::from(Span::styled(
                format!("↑ {start} earlier {}", plural(start, "turn")),
                theme.dim,
            )));
            lines.push(Line::default());
        }
        for (index, block) in blocks.drain(start..).enumerate() {
            if index > 0 {
                lines.push(Line::default());
            }
            lines.extend(block);
        }
        lines
    }

    /// The whole conversation; tool calls as one row each.
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
            let (marker, rows, user, quiet) = match part {
                Part::User(text) => (
                    Span::styled(
                        "› ",
                        Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
                    ),
                    plain_rows(text, inner),
                    true,
                    false,
                ),
                Part::Assistant(text) => (
                    Span::styled("• ", theme.dim),
                    markdown_rows(text, inner, theme),
                    false,
                    false,
                ),
                Part::Call { name, arguments } => (
                    Span::styled("◦ ", theme.dim),
                    vec![Line::from(tools::summary(&call(name, arguments)))],
                    false,
                    true,
                ),
                Part::Output(text) | Part::Compaction(text) | Part::Notice(text) => {
                    (Span::raw("  "), plain_rows(text, inner), false, true)
                }
            };
            for (index, line) in rows.into_iter().enumerate() {
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
                lines.push(if user {
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

/// What the user asked, the tools that ran for it and the last reply.
#[derive(Default)]
struct Turn<'a> {
    user: Option<&'a str>,
    calls: Vec<Tool>,
    compacted: bool,
    notices: Vec<&'a str>,
    reply: Option<&'a str>,
}

impl Turn<'_> {
    fn is_empty(&self) -> bool {
        self.user.is_none()
            && self.calls.is_empty()
            && !self.compacted
            && self.notices.is_empty()
            && self.reply.is_none()
    }

    /// `Explored 3 files · Ran 1 command · Context compacted`, in the order they first ran.
    fn activity(&self) -> String {
        let mut counts: Vec<((&str, Option<&str>), usize)> = Vec::new();
        for call in &self.calls {
            let group = tools::tally(call);
            match counts.iter_mut().find(|(seen, _)| *seen == group) {
                Some((_, count)) => *count += 1,
                None => counts.push((group, 1)),
            }
        }
        let mut items: Vec<String> = counts
            .into_iter()
            .map(|((verb, noun), count)| match noun {
                Some(noun) => format!("{verb} {count} {}", plural(count, noun)),
                None => verb.to_owned(),
            })
            .collect();
        if self.compacted {
            items.push("Context compacted".into());
        }
        items.join(" · ")
    }

    fn render(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if let Some(user) = self.user {
            let rows = wrap::wrap(
                &[Span::raw(single_line(user))],
                width,
                &[Span::styled(
                    "› ",
                    theme.accent.add_modifier(Modifier::BOLD),
                )],
                &[Span::raw("  ")],
            );
            lines.extend(
                clip(rows, ASKED, width)
                    .into_iter()
                    .map(|line| pad(line, width, theme.user())),
            );
        }
        let quiet = |text: String| {
            wrap::fit(
                vec![
                    Span::raw("  "),
                    Span::styled("◦ ", theme.dim),
                    Span::styled(text, theme.dim),
                ],
                width,
                true,
            )
        };
        let activity = self.activity();
        if !activity.is_empty() {
            lines.push(quiet(activity));
        }
        lines.extend(self.notices.iter().map(|notice| quiet(single_line(notice))));
        if let Some(reply) = self.reply {
            let rows = markdown_rows(reply, width.saturating_sub(2).max(1), theme);
            let more = rows.len().saturating_sub(REPLY);
            for (index, row) in rows.into_iter().take(REPLY).enumerate() {
                let marker = if index == 0 {
                    Span::styled("• ", theme.dim)
                } else {
                    Span::raw("  ")
                };
                let mut spans = vec![marker];
                spans.extend(row.spans);
                lines.push(Line::from(spans));
            }
            if more > 0 {
                lines.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("… {more} more {}", plural(more, "line")),
                        theme.dim.add_modifier(Modifier::ITALIC),
                    ),
                ]));
            }
        }
        lines
    }
}

/// The conversation as turns, each opened by the user's message.
fn turns(parts: &[Part]) -> Vec<Turn<'_>> {
    let mut turns = Vec::new();
    let mut turn = Turn::default();
    for part in parts {
        match part {
            Part::User(text) => turns.push(std::mem::replace(
                &mut turn,
                Turn {
                    user: Some(text),
                    ..Turn::default()
                },
            )),
            Part::Assistant(text) => turn.reply = Some(text),
            Part::Call { name, arguments } => turn.calls.push(call(name, arguments)),
            Part::Compaction(_) => turn.compacted = true,
            Part::Notice(text) => turn.notices.push(text),
            Part::Output(_) => {}
        }
    }
    turns.push(turn);
    turns.retain(|turn| !turn.is_empty());
    turns
}

/// A finished call, as the tool cells describe one.
fn call(name: &str, arguments: &Value) -> Tool {
    let mut tool = Tool::new(String::new(), name.to_owned(), arguments.clone());
    tool.output = Some(Value::Null);
    tool
}

/// `›` and the title, bold when selected.
fn heading(session: &Summary, selected: bool, theme: &Theme) -> Vec<Span<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    vec![
        Span::raw("  "),
        if selected {
            Span::styled("› ", theme.accent.add_modifier(Modifier::BOLD))
        } else {
            Span::raw("  ")
        },
        Span::styled(
            title(session),
            if selected { bold } else { Style::default() },
        ),
    ]
}

/// At most `rows` lines, the last one ending in `…` when more were cut.
fn clip(mut lines: Vec<Line<'static>>, rows: usize, width: usize) -> Vec<Line<'static>> {
    if lines.len() > rows {
        lines.truncate(rows);
        if let Some(last) = lines.pop() {
            let mut spans = last.spans;
            spans.push(Span::raw("…"));
            lines.push(wrap::fit(spans, width, true));
        }
    }
    lines
}

fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        noun.to_owned()
    } else {
        format!("{noun}s")
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

fn markdown_rows(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = markdown::render(&plain(text), width, theme);
    while lines.last().is_some_and(|line| wrap::line_width(line) == 0) {
        lines.pop();
    }
    lines
}

fn plain_rows(text: &str, width: usize) -> Vec<Line<'static>> {
    plain(text)
        .split('\n')
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

/// `now`, minutes, hours or days ago, then `Oct 09` after 30 days, with the year when it
/// differs.
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
            if year == civil(reference).0 {
                const MONTHS: [&str; 12] = [
                    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov",
                    "Dec",
                ];
                format!("{} {day:02}", MONTHS[month as usize - 1])
            } else {
                format!("{year:04}-{month:02}-{day:02}")
            }
        }
    }
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
