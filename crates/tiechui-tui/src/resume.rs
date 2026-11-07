//! The resume picker on the alternate screen: search, Cwd/All and Updated/Created,
//! comfortable or dense rows, details with the conversation's tail, and the read-only full
//! conversation. Its reads run in the host loop and are tagged with the picker's generation,
//! so a closed or replaced picker never sees late results.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::text::{Line, Span};
use tokio_util::sync::CancellationToken;

use crate::prompt::Input;
use crate::status::compact_path;
use crate::theme::Theme;
use crate::tools::plain;
use crate::{markdown, wrap};

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
    home: Option<PathBuf>,
    reference: u64,
    query: Input,
    all: bool,
    created: bool,
    sort_focused: bool,
    dense: bool,
    lists: [Option<Vec<Summary>>; 2],
    loading: [bool; 2],
    previews: HashMap<PathBuf, Preview>,
    expanded: HashSet<PathBuf>,
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
    pub fn new(generation: u64, fresh: bool, home: Option<PathBuf>, reference: u64) -> Self {
        Self {
            generation,
            cancel: CancellationToken::new(),
            fresh,
            home,
            reference,
            query: Input::default(),
            all: false,
            created: false,
            sort_focused: false,
            dense: false,
            lists: [None, None],
            loading: [true, false],
            previews: HashMap::new(),
            expanded: HashSet::new(),
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
        self.filtered = matches(self.list(), &self.query.0, self.created);
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
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('c') {
            return Outcome::Quit;
        }
        self.error = None;
        if key.code == KeyCode::Esc {
            if let Some(full) = self.full.take() {
                if !full.ready && !self.expanded.contains(&full.path) {
                    self.previews.remove(&full.path);
                }
            } else if !self.query.0.is_empty() {
                self.touched = true;
                self.query.0.clear();
                self.reconcile(true);
            } else {
                return Outcome::Cancel;
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
        let step = (self.height / if self.dense { 1 } else { 3 }).max(1);
        let last = self.filtered.len().saturating_sub(1);
        match key.code {
            KeyCode::Enter => {
                if let Some(session) = self.current() {
                    return Outcome::Pick(session.path.clone());
                }
            }
            KeyCode::Tab | KeyCode::BackTab => self.sort_focused = !self.sort_focused,
            KeyCode::Left | KeyCode::Right => self.toggle_option(),
            KeyCode::Char('o') if control => {
                self.dense = !self.dense;
                self.top = 0;
            }
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
            KeyCode::Char('e') if control => {
                self.touched = true;
                if let Some(path) = self.current().map(|s| s.path.clone()) {
                    if self.expanded.remove(&path) {
                        if matches!(self.previews.get(&path), Some(Preview::Loading)) {
                            self.previews.remove(&path);
                        }
                    } else {
                        self.request(&path);
                        self.expanded.insert(path);
                    }
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
        self.selected = self.selected.min(last);
        Outcome::Open
    }

    fn select(&mut self, index: usize) {
        self.touched = true;
        self.selected = index;
    }

    /// Exactly `rows` lines, or fewer on the smallest terminals.
    pub fn render(&mut self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let width = width.max(1);
        let rows = rows.max(1);
        let styles = Styles::new(theme);
        let esc = Line::from(Span::styled(
            if width >= 3 { "esc" } else { "⎋" },
            theme.dim,
        ));
        let heading = |text: &str| {
            wrap::fit(
                vec![Span::styled(text.to_owned(), styles.title)],
                width,
                true,
            )
        };
        if rows <= 2 {
            let mut lines = Vec::new();
            if rows == 2 {
                lines.push(heading("Resume a previous session"));
            }
            lines.push(esc);
            return lines;
        }
        if rows == 3 {
            return vec![
                heading("Resume a previous session"),
                self.search(width, theme),
                esc,
            ];
        }
        let mut header = vec![match &self.full {
            Some(full) => heading(&format!("Conversation · {}", full.title)),
            None => heading("Resume a previous session"),
        }];
        if let Some(error) = &self.error
            && rows >= 7
        {
            header.push(wrap::fit(
                vec![Span::styled(error.clone(), theme.error)],
                width,
                true,
            ));
        }
        if let Some(full) = &self.full {
            if rows >= 8 {
                header.push(wrap::fit(
                    vec![Span::styled(full.id.clone(), theme.dim)],
                    width,
                    true,
                ));
                header.push(Line::default());
            }
        } else {
            let toolbar = self.toolbar(theme);
            let toolbar_width = wrap::line_width(&toolbar);
            if width >= toolbar_width + 18 && rows >= 7 {
                let mut line = self.search(width - toolbar_width - 3, theme);
                let gap = (width - toolbar_width)
                    .saturating_sub(wrap::line_width(&line))
                    .max(3);
                line.spans.push(Span::raw(" ".repeat(gap)));
                line.spans.extend(toolbar.spans);
                header.push(line);
            } else {
                header.push(self.search(width, theme));
                if rows >= 10 {
                    header.push(wrap::fit(toolbar.spans, width, true));
                }
            }
            if rows >= 8 {
                header.push(Line::default());
            }
        }
        let footer_rows = self.footer(width, rows, theme).len();
        self.height = rows.saturating_sub(header.len() + footer_rows).max(1);
        let body = match &self.full {
            Some(full) if !full.ready => loading(width, self.height, theme),
            Some(full) => {
                let lines = self.conversation(&full.path.clone(), width, theme);
                self.lines = lines.len();
                self.scroll = self.scroll.min(lines.len().saturating_sub(self.height));
                lines
                    .into_iter()
                    .skip(self.scroll)
                    .take(self.height)
                    .collect()
            }
            None if self.filtered.is_empty() => {
                let text = if self.loading[usize::from(self.all)] {
                    "Loading sessions…"
                } else if !self.query.0.is_empty() {
                    "No matching sessions"
                } else {
                    "No sessions yet"
                };
                vec![Line::from(Span::styled(text, theme.dim))]
            }
            None => self.rows(width, theme, &styles),
        };
        let footer = self.footer(width, rows, theme);
        let blank = self.height.saturating_sub(body.len());
        header
            .into_iter()
            .chain(body)
            .chain(std::iter::repeat_n(Line::default(), blank))
            .chain(footer)
            .map(|line| wrap::fit(line.spans, width, false))
            .take(rows)
            .collect()
    }

    /// The query with a block cursor, or the placeholder; the tail stays visible.
    fn search(&self, width: usize, theme: &Theme) -> Line<'static> {
        let cursor = Style::default().add_modifier(Modifier::REVERSED);
        if self.query.0.is_empty() {
            return wrap::fit(
                vec![
                    Span::styled("T", cursor.patch(theme.dim)),
                    Span::styled("ype to search", theme.dim),
                ],
                width,
                true,
            );
        }
        let room = width.saturating_sub(1);
        let mut text = self.query.0.clone();
        while wrap::width(&text) > room {
            text.remove(0);
        }
        Line::from(vec![Span::raw(text), Span::styled(" ", cursor)])
    }

    fn toolbar(&self, theme: &Theme) -> Line<'static> {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let value = |name: &'static str, active: bool, focused: bool| match (active, focused) {
            (true, true) => Span::styled(name, theme.accent.add_modifier(Modifier::BOLD)),
            (true, false) => Span::styled(name, bold),
            (false, _) => Span::styled(name, theme.dim),
        };
        Line::from(vec![
            Span::styled("Filter: ", theme.dim),
            value("Cwd", !self.all, !self.sort_focused),
            Span::raw("  "),
            value("All", self.all, !self.sort_focused),
            Span::raw("    "),
            Span::styled("Sort: ", theme.dim),
            value("Updated", !self.created, self.sort_focused),
            Span::raw("  "),
            value("Created", self.created, self.sort_focused),
        ])
    }

    /// Key hints, the widest that fit, with the position on the right; the list's view keys
    /// get a row of their own when they do not fit beside the rest.
    fn footer(&self, width: usize, rows: usize, theme: &Theme) -> Vec<Line<'static>> {
        let back = if self.fresh { "start new" } else { "back" };
        let view = if self.dense { "comfortable" } else { "dense" };
        let views = format!("ctrl+e details · ctrl+o {view} · ctrl+t transcript");
        let hints = if self.full.is_some() {
            vec![
                "↑/↓ scroll · PgUp/PgDn · Home/End · esc back · ctrl+c quit".to_owned(),
                "↑↓ scroll · esc back".into(),
                "esc".into(),
                "⎋".into(),
            ]
        } else {
            vec![
                format!("enter resume · esc {back} · tab filter/sort · ←/→ change · {views}"),
                format!("enter resume · esc {back} · tab filter/sort · ←/→ change"),
                format!("enter resume · esc {back}"),
                "esc".into(),
                "⎋".into(),
            ]
        };
        let count = if self.full.is_some() {
            format!("{} / {}", self.scroll + 1, self.lines.max(1))
        } else {
            let position = if self.filtered.is_empty() {
                0
            } else {
                self.selected + 1
            };
            format!("{position} / {}", self.filtered.len())
        };
        let fits = |hint: &String| wrap::width(hint) + wrap::width(&count) + 3 <= width;
        let (hint, count) = match hints.iter().position(fits) {
            Some(index) if rows >= 8 => (index, Some(count)),
            _ => (
                hints
                    .iter()
                    .position(|hint| wrap::width(hint) <= width)
                    .unwrap_or(hints.len() - 1),
                None,
            ),
        };
        let mut line = hint_line(&hints[hint], width, theme);
        if let Some(count) = count {
            let gap = width.saturating_sub(wrap::line_width(&line) + wrap::width(&count));
            line.spans.push(Span::raw(" ".repeat(gap)));
            line.spans.push(Span::styled(count, theme.dim));
        }
        let mut lines = vec![line];
        if self.full.is_none() && hint == 1 && rows >= 12 {
            lines.push(hint_line(&views, width, theme));
        }
        if rows >= 8 {
            lines.insert(0, Line::default());
        }
        lines
    }

    /// The visible rows. Heights are known without laying out collapsed rows, so only the
    /// expanded ones are rendered to measure.
    fn rows(&mut self, width: usize, theme: &Theme, styles: &Styles) -> Vec<Line<'static>> {
        let comfortable = !self.dense && width >= 24;
        let gap = usize::from(!self.dense);
        let count = self.filtered.len();
        let mut rendered: HashMap<usize, Vec<Line<'static>>> = HashMap::new();
        let mut positions = Vec::with_capacity(count);
        let mut heights = Vec::with_capacity(count);
        let mut total = 0;
        for (index, item) in self.filtered.iter().enumerate() {
            positions.push(total);
            let height = if self.expanded.contains(&self.list()[*item].path) {
                let row = self.row(index, width, theme, styles);
                let height = row.len();
                rendered.insert(index, row);
                height
            } else if comfortable {
                2
            } else {
                1
            };
            heights.push(height);
            total += height + if index + 1 < count { gap } else { 0 };
        }
        let position = positions.get(self.selected).copied().unwrap_or(0);
        if position < self.top || position >= self.top + self.height {
            self.top = position;
        }
        self.top = self.top.min(total.saturating_sub(self.height));
        let mut body = Vec::new();
        for (index, (start, height)) in positions.into_iter().zip(heights).enumerate() {
            if start > self.top + self.height {
                break;
            }
            if start + height < self.top {
                continue;
            }
            let mut row = rendered
                .remove(&index)
                .unwrap_or_else(|| self.row(index, width, theme, styles));
            if gap == 1 && index + 1 < count {
                row.push(Line::default());
            }
            for (offset, line) in row.into_iter().enumerate() {
                if start + offset >= self.top && body.len() < self.height {
                    body.push(line);
                }
            }
        }
        body
    }

    fn row(
        &self,
        index: usize,
        width: usize,
        theme: &Theme,
        styles: &Styles,
    ) -> Vec<Line<'static>> {
        let session = &self.list()[self.filtered[index]];
        let active = index == self.selected;
        let expanded = self.expanded.contains(&session.path);
        let cursor = if active {
            Span::styled(
                if expanded { "⌄ " } else { "› " },
                styles.selected.add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("  ")
        };
        let name = title(session);
        let name_style = if active {
            styles.selected
        } else {
            Style::default()
        };
        let time = if self.created {
            session.created_at
        } else {
            session.modified
        };
        let date = relative(time, self.reference);
        let mut spans = vec![cursor];
        if self.dense && width >= 24 {
            let date_style = if active { styles.selected } else { theme.dim };
            spans.push(Span::styled(format!("{date:<12}"), date_style));
            spans.extend(
                wrap::fit(
                    vec![Span::styled(name, name_style)],
                    width.saturating_sub(14).max(1),
                    true,
                )
                .spans,
            );
        } else {
            spans.extend(
                wrap::fit(
                    vec![Span::styled(name, name_style)],
                    width.saturating_sub(2).max(1),
                    true,
                )
                .spans,
            );
        }
        let mut lines = vec![wrap::fit(spans, width, true)];
        let directory = compact_path(&session.cwd, self.home.as_deref());
        if expanded {
            let metadata = [
                ("Session", session.id.clone()),
                ("Created", date_time(session.created_at, self.reference)),
                ("Updated", date_time(session.modified, self.reference)),
                ("Directory", directory),
            ];
            for (key, value) in metadata {
                lines.extend(wrap::wrap(
                    &[
                        Span::styled(format!("  │ {:<12}", format!("{key}:")), theme.dim),
                        Span::raw(single_line(&value)),
                    ],
                    width,
                    &[],
                    &[],
                ));
            }
            lines.push(Line::from(Span::styled("  │", theme.dim)));
            lines.push(Line::from(Span::styled("  │ Conversation:", theme.dim)));
            let tail = self.tail(&session.path, width.saturating_sub(4).max(1), theme, styles);
            let recent = &tail[tail.len().saturating_sub(6)..];
            for (position, (line, color)) in recent.iter().enumerate() {
                let prefix = if position + 1 == recent.len() {
                    "  └ "
                } else {
                    "  │ "
                };
                let mut spans = vec![Span::styled(prefix, Style::default().fg(*color))];
                spans.extend(line.spans.clone());
                lines.push(Line::from(spans));
            }
            if tail.len() > recent.len() {
                lines.push(Line::from(Span::styled(
                    "    ctrl+t to view the full conversation",
                    theme.dim,
                )));
            }
        } else if !self.dense && width >= 24 {
            let mut text = format!("  {date}");
            if self.all {
                text.push_str(&format!(" · {}", single_line(&directory)));
            }
            lines.push(wrap::fit(vec![Span::styled(text, theme.dim)], width, true));
        }
        lines
    }

    /// The conversation's last seven rows, each with the color its prefix takes.
    fn tail(
        &self,
        path: &Path,
        width: usize,
        theme: &Theme,
        styles: &Styles,
    ) -> Vec<(Line<'static>, Color)> {
        let quiet = styles.quiet;
        let parts = match self.previews.get(path) {
            None | Some(Preview::Loading) => {
                return vec![(
                    Line::from(Span::styled("Loading conversation…", theme.dim)),
                    quiet,
                )];
            }
            Some(Preview::Failed(error)) => {
                let text = format!("Unable to read conversation: {error}");
                return vec![(Line::from(Span::styled(text, theme.error)), quiet)];
            }
            Some(Preview::Ready(parts)) => parts,
        };
        let mut lines: Vec<(Line<'static>, Color)> = Vec::new();
        for part in parts.iter().rev() {
            let user = part.kind == PartKind::User;
            let color = if user { styles.user } else { quiet };
            let mut style = Style::default().fg(color);
            if user {
                style = style.add_modifier(Modifier::ITALIC);
            }
            let rows: Vec<_> = layout(part, width, theme)
                .into_iter()
                .map(|line| {
                    let mut line = wrap::fit(line.spans, width, true);
                    for span in &mut line.spans {
                        span.style = span.style.patch(style);
                    }
                    (line, color)
                })
                .collect();
            let keep = rows.len().saturating_sub(7);
            lines.splice(0..0, rows.into_iter().skip(keep));
            if lines.len() >= 7 {
                break;
            }
        }
        if lines.is_empty() {
            let text = "No conversation messages yet.";
            lines.push((Line::from(Span::styled(text, theme.dim)), quiet));
        }
        let skip = lines.len().saturating_sub(7);
        lines.split_off(skip)
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

struct Styles {
    title: Style,
    selected: Style,
    /// The tail's color for everything but the user's words.
    quiet: Color,
    user: Color,
}

impl Styles {
    fn new(theme: &Theme) -> Self {
        let light = theme.palette.light();
        Self {
            title: Style::default().add_modifier(Modifier::BOLD),
            selected: theme.accent,
            quiet: Color::Indexed(if light { 7 } else { 8 }),
            user: Color::Indexed(if light { 8 } else { 7 }),
        }
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

/// The centered "Loading conversation…" card.
fn loading(width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
    let bg = theme.palette.surface(0.14, 0.08).or_else(|| {
        theme
            .palette
            .background
            .is_none()
            .then_some(Color::Indexed(8))
    });
    let frame = width.min(32);
    let left = " ".repeat((width - frame) / 2);
    let message = "Loading conversation…";
    let padding = frame.saturating_sub(wrap::width(message)) / 2;
    let surface = bg.map_or_else(Style::default, |bg| Style::default().bg(bg));
    let card = |spans: Vec<Span<'static>>| {
        let mut line = pad(wrap::fit(spans, frame, true), frame, surface);
        line.spans.insert(0, Span::raw(left.clone()));
        line
    };
    let mut lines = vec![Line::default(); height.saturating_sub(3) / 2];
    lines.push(card(vec![]));
    lines.push(card(vec![
        Span::raw(" ".repeat(padding)),
        Span::styled(message, Style::default().add_modifier(Modifier::BOLD)),
    ]));
    lines.push(card(vec![]));
    lines
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
