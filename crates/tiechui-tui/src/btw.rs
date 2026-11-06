//! The side conversation uses the main transcript cells, composer and status line.

use crate::{
    activity,
    cells::Cell,
    composer::{Composer, Draft},
    pager::Pager,
    paste,
    status::{self, Phase, Snapshot, StatusLineConfig},
    status_picker::{Outcome as PickerOutcome, StatusPicker},
    theme::Theme,
    tools::View,
    wrap,
};
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui_core::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use tiechui_protocol::{BtwAction, BtwView, InputContent};

pub enum Outcome {
    Open,
    Switch,
    Action(BtwAction),
    Save(StatusLineConfig),
}
pub fn switch_key(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('/' | '7' | '_'))
        || key.code == KeyCode::Char('\x1f')
}

pub struct Panel {
    pub view: BtwView,
    pub(crate) composer: Composer,
    scroll: Pager,
    full: Option<Pager>,
    thinking: bool,
    /// Replies whose thinking differs from `thinking`, flipped by their control rows.
    unfolded: std::collections::HashSet<usize>,
    picker: Option<StatusPicker>,
    error: Option<String>,
    stash: Option<Draft>,
    history: Option<(usize, Draft)>,
    help: bool,
    width: usize,
    editor_rows: std::ops::Range<usize>,
    /// Visible control rows: screen row, reply and the row itself.
    controls: Vec<(usize, usize, Line<'static>)>,
    pointer: Option<(usize, usize)>,
    started: std::time::Instant,
    pub cursor: Option<(usize, usize)>,
    /// Whether the terminal has focus, for the composer rules.
    pub focused: bool,
}
impl Panel {
    pub fn new(view: BtwView) -> Self {
        Self {
            view,
            composer: Composer::default(),
            scroll: Pager::default(),
            full: None,
            thinking: false,
            unfolded: Default::default(),
            picker: None,
            error: None,
            stash: None,
            history: None,
            help: false,
            width: 80,
            editor_rows: 0..0,
            controls: Vec::new(),
            pointer: None,
            started: std::time::Instant::now(),
            cursor: None,
            focused: true,
        }
    }
    pub fn update(&mut self, view: BtwView) {
        if view.busy && !self.view.busy {
            self.started = std::time::Instant::now();
        }
        self.view = view;
    }
    pub fn paste_with_path(&mut self, text: &str, cwd: &std::path::Path) {
        if self.full.is_some() || self.picker.is_some() {
            return;
        }
        if !self.composer.text().trim_start().starts_with('!')
            && let Some((mime, data)) = paste::image(text, cwd)
        {
            self.composer.insert_image(mime, data);
        } else {
            self.paste(text);
        }
    }

    pub fn paste(&mut self, text: &str) {
        if self.full.is_some() || self.picker.is_some() {
            return;
        }
        let text = paste::normalize(text);
        if crate::composer::should_collapse(&text) {
            self.composer.insert_paste(&text);
        } else {
            self.composer.insert(&text);
        }
    }
    pub fn mouse(&mut self, event: MouseEvent) {
        if self.picker.is_some() {
            return;
        }
        let row = usize::from(event.row);
        let column = usize::from(event.column);
        self.pointer = Some((row, column));
        if event.kind == MouseEventKind::Down(MouseButton::Left) && self.full.is_none() {
            if self.editor_rows.contains(&row) {
                self.composer.click(
                    self.width.saturating_sub(2),
                    row - self.editor_rows.start,
                    column.saturating_sub(2),
                );
            } else if let Some((_, reply, _)) = self.control(row, column) {
                let reply = *reply;
                if !self.unfolded.remove(&reply) {
                    self.unfolded.insert(reply);
                }
            }
            return;
        }
        let code = match event.kind {
            MouseEventKind::ScrollUp => KeyCode::Up,
            MouseEventKind::ScrollDown => KeyCode::Down,
            _ => return,
        };
        let pager = self.full.as_mut().unwrap_or(&mut self.scroll);
        for _ in 0..3 {
            pager.key(KeyEvent::new(code, KeyModifiers::NONE));
        }
    }
    /// The visible control row under a pointer on its label or padding.
    fn control(&self, row: usize, column: usize) -> Option<&(usize, usize, Line<'static>)> {
        self.controls
            .iter()
            .find(|(at, _, line)| *at == row && crate::cells::on_control(line, column, self.width))
    }

    pub fn key(&mut self, key: KeyEvent, config: &StatusLineConfig) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Open;
        }
        if let Some(picker) = &mut self.picker {
            match picker.key(key) {
                PickerOutcome::Open => {}
                PickerOutcome::Cancel => self.picker = None,
                PickerOutcome::Save(config) => {
                    self.picker = None;
                    return Outcome::Save(config);
                }
            }
            return Outcome::Open;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if let Some(pager) = &mut self.full {
            if (control && key.code == KeyCode::Char('o')) || !pager.key(key) {
                self.full = None;
            }
            return Outcome::Open;
        }
        if key.kind == KeyEventKind::Repeat
            && control
            && (key.code == KeyCode::Char('c')
                || (key.code == KeyCode::Char('d') && self.composer.is_empty()))
        {
            return Outcome::Open;
        }
        if self.help && matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
            self.help = false;
            return Outcome::Open;
        }
        if switch_key(key) {
            return Outcome::Switch;
        }
        let c = &mut self.composer;
        match key.code {
            KeyCode::Char('c') if control && !c.is_empty() => {
                c.clear();
            }
            KeyCode::Char('c' | 'd') if control && c.is_empty() => {
                return Outcome::Action(BtwAction::Close);
            }
            KeyCode::Char('s') if control => return Outcome::Action(BtwAction::Share),
            KeyCode::Char('t') if control => {
                self.thinking = !self.thinking;
                self.unfolded.clear();
            }
            KeyCode::Char('o') if control => self.full = Some(Pager::default()),
            KeyCode::Esc if self.view.busy => return Outcome::Action(BtwAction::Stop),
            KeyCode::PageUp | KeyCode::PageDown => {
                self.scroll.key(key);
            }
            KeyCode::Home | KeyCode::End if control => {
                self.scroll.key(key);
            }
            KeyCode::Enter if !shift && !alt => {
                let (text, images) = c.submission_without_image_labels();
                if text.trim() == "/statusline" {
                    c.clear();
                    self.picker = Some(StatusPicker::new(config));
                } else if text.trim() == "/btw" || text.trim().starts_with("/btw ") {
                    self.error = Some("A side conversation is already open. Type a follow-up here, or press ctrl + / to return to main.".into());
                } else if self.view.busy {
                    self.error = Some("Side reply is still running. Wait for it or press Esc in the side panel before sending another question.".into());
                } else if !text.trim().is_empty() || !images.is_empty() {
                    c.clear();
                    self.error = None;
                    self.history = None;
                    self.scroll = Pager::default();
                    // Reserve the request immediately, including the trip to the actor.
                    self.view.busy = true;
                    self.started = std::time::Instant::now();
                    let mut content = vec![InputContent::Text { text }];
                    content.extend(
                        images
                            .into_iter()
                            .map(|(mime_type, data)| InputContent::Image { mime_type, data }),
                    );
                    return Outcome::Action(BtwAction::Send { content });
                }
            }
            KeyCode::Char('s') if alt => {
                let draft = c.clear();
                if let Some(stash) = self.stash.take() {
                    c.restore(stash);
                }
                if !draft.is_empty() {
                    self.stash = Some(draft);
                }
            }
            KeyCode::Char('a') if control => c.home(),
            KeyCode::Char('e') if control => c.end(),
            KeyCode::Char('b') if control => c.left(),
            KeyCode::Char('f') if control => c.right(),
            KeyCode::Char('d') if control => c.delete_forward(),
            KeyCode::Char('w') if control => c.delete_word_backward(),
            KeyCode::Char('u') if control => c.delete_to_line_start(),
            KeyCode::Char('k') if control => c.delete_to_line_end(),
            KeyCode::Char('y') if control => c.yank(),
            KeyCode::Char('j') if control => c.newline(),
            KeyCode::Char('-') if control => c.undo(),
            KeyCode::Char('b') if alt => c.word_backward(),
            KeyCode::Char('f') if alt => c.word_forward(),
            KeyCode::Char('d') if alt => c.delete_word_forward(),
            KeyCode::Char('y') if alt => c.yank_pop(),
            KeyCode::Enter => c.newline(),
            KeyCode::Backspace if alt || control => c.delete_word_backward(),
            KeyCode::Backspace => c.backspace(),
            KeyCode::Delete if alt => c.delete_word_forward(),
            KeyCode::Delete => c.delete_forward(),
            KeyCode::Left if alt || control => c.word_backward(),
            KeyCode::Left => c.left(),
            KeyCode::Right if alt || control => c.word_forward(),
            KeyCode::Right => c.right(),
            KeyCode::Home => c.home(),
            KeyCode::End => c.end(),
            KeyCode::Up | KeyCode::Down => {
                let up = key.code == KeyCode::Up;
                if !c.vertical(self.width.saturating_sub(2).max(1), up) {
                    let rows: Vec<_> = self.view.messages.iter().filter(|m| m.user).collect();
                    if let Some((index, draft)) = &mut self.history {
                        if up {
                            *index = index.saturating_sub(1);
                        } else {
                            *index += 1;
                        }
                        if let Some(row) = rows.get(*index) {
                            c.set_text(&row.text);
                        } else {
                            c.restore(draft.clone());
                            self.history = None;
                        }
                    } else if up && !rows.is_empty() {
                        self.history = Some((rows.len() - 1, c.draft()));
                        c.set_text(&rows[rows.len() - 1].text);
                    }
                }
            }
            KeyCode::Char('?') if !control && !alt && c.is_empty() => self.help = true,
            KeyCode::Char(ch) if !control && !alt => c.insert(ch.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        Outcome::Open
    }
    pub fn render(
        &mut self,
        width: usize,
        rows: usize,
        theme: &Theme,
        main: &Snapshot<'_>,
        config: &StatusLineConfig,
        main_status: &str,
    ) -> Vec<Line<'static>> {
        self.width = width;
        self.cursor = None;
        let side = Snapshot {
            model: Some(&self.view.model),
            thinking: self.view.thinking,
            context: self.view.context.as_ref(),
            total: &self.view.total,
            thread: Some(&self.view.id),
            phase: if self.view.busy {
                Phase::Thinking
            } else if self.view.messages.last().is_some_and(|m| m.error.is_some()) {
                Phase::Error
            } else if self.view.messages.last().is_some_and(|m| m.interrupted) {
                Phase::Cancelled
            } else {
                Phase::Ready
            },
            ..*main
        };
        if let Some(picker) = &mut self.picker {
            let (lines, cursor) = picker.render(&side, width, rows, theme);
            self.cursor = Some(cursor);
            return lines;
        }
        let full = self.full.is_some();
        let mut cells = Vec::new();
        for (reply, row) in self.view.messages.iter().enumerate() {
            if row.user {
                let mut text = row
                    .images
                    .iter()
                    .enumerate()
                    .map(|(i, _)| format!("[Image #{}]", i + 1))
                    .collect::<Vec<_>>();
                text.push(row.text.clone());
                cells.push((reply, Cell::InlineUser(text.join("\n"))));
            } else {
                if !row.thinking.is_empty() {
                    cells.push((reply, Cell::ThinkingSummary(row.thinking.clone())));
                }
                if !row.text.is_empty() {
                    cells.push((reply, Cell::Assistant(row.text.clone())));
                }
                if let Some(error) = &row.error {
                    cells.push((reply, Cell::Error(error.clone())));
                }
            }
        }
        let mut body = Vec::new();
        let mut controls = Vec::new();
        for (reply, cell) in &cells {
            let (block, control) = if full {
                (cell.render(width, theme, View::Transcript, true), None)
            } else {
                let unfolded = self.thinking != self.unfolded.contains(reply);
                cell.folded(width, theme, unfolded, self.thinking)
            };
            if !block.is_empty() {
                body.push(Line::default());
                if let Some(row) = control {
                    controls.push((body.len() + row, *reply));
                }
                body.extend(block);
            }
        }
        if let Some(pager) = &mut self.full {
            let footer = wrap::wrap(
                &[Span::styled(
                    "q / ctrl+c / ctrl+o close transcript · ↑↓ scroll · PgUp/PgDn page",
                    theme.dim,
                )],
                width,
                &[],
                &[],
            );
            let height = rows.saturating_sub(footer.len() + 1);
            let mut lines = vec![Line::from(Span::styled("  Full transcript", theme.dim))];
            if height > 0 {
                lines.extend(pager.viewport(&body, height));
            }
            lines.resize(height + 1, Line::default());
            lines.extend(footer);
            lines.truncate(rows);
            return lines;
        }
        if let Some(error) = &self.error {
            body.extend(Cell::Error(error.clone()).lines(width, theme));
        }
        if let Some(notice) = &self.view.notice {
            body.extend(wrap::wrap(
                &[Span::styled(notice.clone(), theme.dim)],
                width,
                &[Span::raw("  ")],
                &[Span::raw("  ")],
            ));
        }
        if self.view.messages.last().is_some_and(|m| m.interrupted) {
            body.push(Line::from(Span::styled(
                "  Side reply interrupted.",
                theme.dim,
            )));
        }
        let inner = width.saturating_sub(4).max(1);
        let mut footer = Vec::new();
        if self.help {
            for text in [
                "enter send · shift+enter newline",
                "ctrl+t thinking · ctrl+o transcript · ctrl+s share",
                "/statusline configure footer · alt+s stash draft · esc close shortcuts",
            ] {
                footer.extend(wrap::wrap(
                    &[Span::styled(text, theme.dim)],
                    width,
                    &[Span::raw("  ")],
                    &[Span::raw("  ")],
                ));
            }
        }
        footer.push(status::line(config, &side, width, theme));
        let label = if width >= 70 {
            "Side from main thread"
        } else {
            "Side"
        };
        let parent = if main_status.is_empty() {
            String::new()
        } else {
            format!(" · main {main_status}")
        };
        let label = format!("{label}{parent} · ctrl+/ to switch · ctrl+c to close");
        let magenta = Style::default().fg(theme
            .palette
            .tone(crate::palette::Tone::Accent2)
            .unwrap_or(Color::Magenta));
        footer.extend(
            wrap::wrap(
                &[
                    Span::styled("Side", magenta.add_modifier(Modifier::BOLD)),
                    Span::styled(label[4..].to_owned(), magenta),
                ],
                inner,
                &[],
                &[],
            )
            .into_iter()
            .take(2)
            .map(|line| {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(line.spans);
                Line::from(spans)
            }),
        );
        footer.truncate(rows.saturating_sub(3));
        let working = self.view.busy
            && self
                .view
                .messages
                .last()
                .is_none_or(|m| m.user || m.text.trim().is_empty());
        let reserve = footer.len() + 2 + usize::from(working);
        let mut editor = self.composer.render(
            width,
            rows.saturating_sub(reserve).clamp(1, (rows / 2).max(1)),
            Span::raw("› "),
            theme.dim,
        );
        if self.composer.is_empty() {
            editor.lines[0] = wrap::fit(
                vec![
                    Span::raw("› "),
                    Span::styled("Ask a follow-up question", theme.dim),
                ],
                width,
                false,
            );
        }
        let room = rows.saturating_sub(reserve + editor.lines.len());
        let mut lines = if room > 0 {
            self.scroll.viewport(&body, room)
        } else {
            vec![]
        };
        self.controls = (0..lines.len())
            .filter_map(|row| {
                let line = self.scroll.line(row)?;
                let (_, reply) = controls.iter().find(|(at, _)| *at == line)?;
                Some((row, *reply, lines[row].clone()))
            })
            .collect();
        if let Some((row, column)) = self.pointer
            && self.control(row, column).is_some()
        {
            lines[row] = crate::cells::hovered(&lines[row], theme);
        }
        lines.resize(room, Line::default());
        if working {
            let label = self
                .view
                .messages
                .last()
                .and_then(|m| activity::heading(&m.thinking).map(crate::tools::plain))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Working".into());
            lines.push(wrap::fit(
                vec![Span::styled(
                    format!(
                        "• {label} ({} • esc to interrupt)",
                        activity::elapsed(self.started.elapsed())
                    ),
                    theme.dim,
                )],
                width,
                false,
            ));
        }
        let rule = Line::from(Span::styled("─".repeat(width), theme.rule(self.focused)));
        lines.push(rule.clone());
        self.editor_rows = lines.len()..lines.len() + editor.lines.len();
        self.cursor = Some((lines.len() + editor.cursor.0, editor.cursor.1));
        lines.extend(editor.lines);
        lines.push(rule);
        lines.extend(footer);
        lines.truncate(rows);
        lines.resize(rows, Line::default());
        if self
            .cursor
            .is_some_and(|(row, col)| row >= rows || col >= width)
        {
            self.cursor = None;
        }
        lines
    }
}
