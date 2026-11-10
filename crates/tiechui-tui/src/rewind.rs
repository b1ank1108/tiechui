//! Protocol-only rewind list and confirmation panel.

use crate::sheet::{self, Row};
use crate::{prompt::Outcome, theme::Theme, wrap};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_core::{
    style::Modifier,
    text::{Line, Span},
};
use tiechui_protocol::{ApprovalDecision, CheckpointStats, RewindPage, RewindPanel};

pub struct Rewind {
    pub panel: RewindPanel,
    action: usize,
}
impl Rewind {
    pub fn new(panel: RewindPanel) -> Self {
        Self { panel, action: 0 }
    }
    pub fn update(&mut self, panel: RewindPanel) {
        if self.panel.page != panel.page || self.panel.selected != panel.selected {
            self.action = 0;
        }
        self.panel = panel;
    }
    fn actions(&self) -> Vec<(&'static str, &'static str, &'static str)> {
        if self.panel.page == RewindPage::Recovery {
            return vec![
                (
                    "recover",
                    "Recover pre-operation files?",
                    "Files are checked before recovery. The conversation position is not changed.",
                ),
                ("cancel", "Never mind", "Keep the recovery journal."),
            ];
        }
        let mut actions = vec![];
        if self
            .panel
            .preview
            .as_ref()
            .is_some_and(|stats| stats.error.is_none() && !stats.files.is_empty())
        {
            actions.push((
                "both",
                "Restore code and conversation",
                "Restore tracked files and return this prompt to the editor.",
            ));
        }
        actions.push((
            "conversation",
            "Restore conversation",
            "Keep current files. Return this prompt to the editor without sending it.",
        ));
        if actions.len() == 2 {
            actions.push((
                "code",
                "Restore code",
                "Restore tracked files. Keep the current conversation.",
            ));
        }
        actions.push((
            "back",
            "Never mind",
            "Return to the message list without changing anything.",
        ));
        actions
    }
    fn answer(&self, action: &str) -> Outcome {
        let row = self.panel.rows.get(self.panel.selected);
        Outcome::Answer(ApprovalDecision::Accept {
            content: Some(
                serde_json::json!({"action":action,"entry_id":row.map(|row| &row.entry_id),"session_id":row.map(|row| &row.session_id)}),
            ),
        })
    }
    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        if key.kind == KeyEventKind::Release
            || (key.kind == KeyEventKind::Repeat
                && matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('1'..='4')
                ))
        {
            return Outcome::Open;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Outcome::Answer(ApprovalDecision::Cancel);
        }
        if key.code == KeyCode::Esc {
            return if matches!(self.panel.page, RewindPage::Messages | RewindPage::Recovery) {
                Outcome::Answer(ApprovalDecision::Cancel)
            } else {
                self.answer("back")
            };
        }
        if self.panel.page == RewindPage::Checking {
            return Outcome::Open;
        }
        let list = self.panel.page == RewindPage::Messages;
        let max = if list {
            self.panel.rows.len()
        } else {
            self.actions().len()
        }
        .saturating_sub(1);
        let index = if list {
            self.panel.selected
        } else {
            self.action
        };
        let next = match key.code {
            KeyCode::Up => index.saturating_sub(1),
            KeyCode::Down => (index + 1).min(max),
            KeyCode::Home => 0,
            KeyCode::End => max,
            KeyCode::PageUp => index.saturating_sub(5),
            KeyCode::PageDown => (index + 5).min(max),
            KeyCode::Enter if list => return self.answer("preview"),
            KeyCode::Enter => return self.answer(self.actions()[self.action].0),
            KeyCode::Char(number @ '1'..='4') if !list => {
                if let Some(action) = self.actions().get(number as usize - '1' as usize) {
                    return self.answer(action.0);
                }
                return Outcome::Open;
            }
            _ => return Outcome::Open,
        };
        if list {
            self.panel.selected = next;
            if next != index {
                return self.answer("move");
            }
        } else {
            self.action = next;
        }
        Outcome::Open
    }
    pub fn render(&self, width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
        let width = width.max(1);
        let room = height.saturating_sub(3).max(1);
        let dim = |text: &str, rows: usize| {
            let indent = [Span::raw("  ")];
            wrap::wrap(
                &[Span::styled(clean(text), theme.dim)],
                width,
                &indent,
                &indent,
            )
            .into_iter()
            .take(rows)
            .collect::<Vec<_>>()
        };
        let mut position = None;
        let range = format!("1-{}", self.actions().len());
        let (title, alt, meta, body, pairs): (_, _, _, Vec<Line<'static>>, Vec<(&str, &str)>) =
            match self.panel.page {
                RewindPage::Messages => {
                    let mut body = dim("Pick a prompt to go back to the point before it.", 2);
                    body.push(Line::default());
                    let rows = &self.panel.rows;
                    let count = room.saturating_sub(body.len()).max(1);
                    let start = self
                        .panel
                        .selected
                        .saturating_sub(count / 2)
                        .min(rows.len().saturating_sub(count));
                    if rows.len() > count {
                        position = Some(format!("{} / {}", self.panel.selected + 1, rows.len()));
                    }
                    let accent = theme.accent.add_modifier(Modifier::BOLD);
                    for (index, row) in rows.iter().enumerate().skip(start).take(count) {
                        let chosen = index == self.panel.selected;
                        let text = if row.text.is_empty() {
                            "(attachment)".into()
                        } else {
                            clean(&row.text)
                        };
                        let mut left = vec![
                            Span::raw("  "),
                            if chosen {
                                Span::styled("› ", accent)
                            } else {
                                Span::raw("  ")
                            },
                            if chosen {
                                Span::styled(text, accent)
                            } else {
                                Span::raw(text)
                            },
                        ];
                        if index + 1 == rows.len() {
                            left.push(Span::styled("  latest", theme.dim));
                        }
                        let mut right = summary(row.stats.as_ref(), width > 90, theme);
                        let at = time(row.timestamp);
                        if !at.is_empty() {
                            right.push(Span::styled(format!("   {at}"), theme.dim));
                        }
                        body.push(sheet::split(left, right, width));
                    }
                    let meta = format!(
                        "{} prompt{}",
                        rows.len(),
                        if rows.len() == 1 { "" } else { "s" }
                    );
                    (
                        "Rewind",
                        None,
                        vec![Span::styled(meta, theme.dim)],
                        body,
                        vec![("enter", "choose"), ("home/end", "jump"), ("esc", "cancel")],
                    )
                }
                RewindPage::Checking | RewindPage::Actions => {
                    let text = self
                        .panel
                        .rows
                        .get(self.panel.selected)
                        .map(|row| row.text.as_str())
                        .filter(|text| !text.is_empty())
                        .unwrap_or("(attachment)");
                    let quote = [Span::raw("  "), Span::styled("│ ", theme.separator)];
                    let mut body = wrap::wrap(
                        &[Span::styled(clean(text), theme.secondary)],
                        width,
                        &quote,
                        &quote,
                    );
                    body.truncate(if height < 28 { 1 } else { 3 });
                    body.push(Line::default());
                    if self.panel.page == RewindPage::Checking {
                        body.extend(dim("Checking checkpoint…", 1));
                        (
                            "Restore to before this prompt?",
                            Some("Restore?"),
                            vec![Span::styled("checking…", theme.dim)],
                            body,
                            vec![("esc", "back")],
                        )
                    } else {
                        let actions = self.actions();
                        let rows: Vec<Row> = actions
                            .iter()
                            .map(|(_, label, _)| Row::new(*label, "", theme))
                            .collect();
                        let options = sheet::options(&rows, self.action, width, false, theme);
                        let mut notes = vec![Line::default()];
                        notes.extend(dim(actions[self.action].2, 2));
                        notes.extend(dim(
                            "Bash, manual and other-session edits are not guaranteed to rewind.",
                            2,
                        ));
                        let stats = self.panel.preview.as_ref();
                        if let Some(error) = stats.and_then(|stats| stats.error.as_ref()) {
                            let indent = [Span::raw("  ")];
                            body.extend(
                                wrap::wrap(
                                    &[Span::styled(clean(error), theme.warning)],
                                    width,
                                    &indent,
                                    &indent,
                                )
                                .into_iter()
                                .take(2),
                            );
                            body.push(Line::default());
                        }
                        // The files fill what the actions and notes leave.
                        let notes_fit = body.len() + options.len() + notes.len() + 2 <= room;
                        let spare = room
                            .saturating_sub(
                                body.len()
                                    + options.len()
                                    + if notes_fit { notes.len() } else { 0 },
                            )
                            .saturating_sub(1);
                        if let Some(stats) = stats.filter(|stats| stats.error.is_none()) {
                            let names: Vec<String> = stats.files.keys().map(|f| clean(f)).collect();
                            let column = names.iter().map(|n| wrap::width(n)).max().unwrap_or(0);
                            let limit = if names.len() > spare {
                                spare.saturating_sub(1)
                            } else {
                                names.len()
                            };
                            for (name, counts) in names.iter().zip(stats.files.values()).take(limit)
                            {
                                let mut spans = vec![Span::raw(format!(
                                    "    {name}{}  ",
                                    " ".repeat(column - wrap::width(name))
                                ))];
                                spans.extend(counts_text(counts.as_ref(), theme));
                                body.push(wrap::fit(spans, width, true));
                            }
                            if limit < names.len() && spare > 0 {
                                body.push(Line::from(Span::styled(
                                    format!("    … {} more files", names.len() - limit),
                                    theme.dim,
                                )));
                            }
                            if limit > 0 || spare > 0 {
                                body.push(Line::default());
                            }
                        }
                        body.extend(options);
                        if notes_fit {
                            body.extend(notes);
                        }
                        (
                            "Restore to before this prompt?",
                            Some("Restore?"),
                            summary(self.panel.preview.as_ref(), false, theme),
                            body,
                            vec![
                                ("enter", "confirm"),
                                (range.as_str(), "pick"),
                                ("esc", "back"),
                            ],
                        )
                    }
                }
                RewindPage::Recovery => {
                    let actions = self.actions();
                    let rows: Vec<Row> = actions
                        .iter()
                        .map(|(_, label, _)| Row::new(*label, "", theme))
                        .collect();
                    let mut body = dim(actions[0].2, 3);
                    body.push(Line::default());
                    body.extend(sheet::options(&rows, self.action, width, false, theme));
                    (
                        "Recovery",
                        None,
                        Vec::new(),
                        body,
                        vec![
                            ("enter", "confirm"),
                            (range.as_str(), "pick"),
                            ("esc", "cancel"),
                        ],
                    )
                }
            };
        let mut body = body;
        body.truncate(room);
        let mut titles = vec![sheet::title(title)];
        titles.extend(alt.map(sheet::title));
        let metas: Vec<Vec<Span<'static>>> = if meta.is_empty() { vec![] } else { vec![meta] };
        sheet::frame(
            theme.accent,
            sheet::rule(theme.accent, &titles, &metas, width),
            body,
            sheet::hints(theme, &pairs, position.as_deref(), width),
            width,
        )
        .into_iter()
        .map(|line| wrap::fit(line.spans, width, true))
        .take(height.max(1))
        .collect()
    }
}
pub(crate) fn clean(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                ' '
            } else {
                c
            }
        })
        .collect()
}
fn time(timestamp: u64) -> String {
    if timestamp == 0 {
        return String::new();
    }
    let seconds = (timestamp / 1000) as libc::time_t;
    let mut result = std::mem::MaybeUninit::<libc::tm>::uninit();
    #[cfg(unix)]
    let converted = !unsafe { libc::localtime_r(&seconds, result.as_mut_ptr()) }.is_null();
    #[cfg(windows)]
    let converted = unsafe { libc::localtime_s(result.as_mut_ptr(), &seconds) } == 0;
    if !converted {
        return String::new();
    }
    let result = unsafe { result.assume_init() };
    format!("{:02}:{:02}", result.tm_hour, result.tm_min)
}
fn counts_text(counts: Option<&tiechui_protocol::LineStats>, theme: &Theme) -> Vec<Span<'static>> {
    match counts {
        Some(counts) => vec![
            Span::styled(format!("+{}", counts.added), theme.success),
            Span::raw(" "),
            Span::styled(format!("−{}", counts.removed), theme.error),
        ],
        None => vec![Span::styled("line counts unavailable", theme.dim)],
    }
}
fn summary(stats: Option<&CheckpointStats>, more: bool, theme: &Theme) -> Vec<Span<'static>> {
    let Some(stats) = stats else {
        return vec![Span::styled("checking…", theme.dim)];
    };
    if stats.error.is_some() {
        return vec![Span::styled("code restore unavailable", theme.warning)];
    }
    let count = stats.files.len();
    if count == 0 {
        return vec![Span::styled("no code changes", theme.dim)];
    }
    let unavailable = stats
        .files
        .values()
        .filter(|counts| counts.is_none())
        .count();
    let sum = stats.files.values().flatten().fold(
        tiechui_protocol::LineStats::default(),
        |mut sum, counts| {
            sum.added += counts.added;
            sum.removed += counts.removed;
            sum
        },
    );
    let mut spans = vec![Span::styled(
        format!("{count} file{} · ", if count == 1 { "" } else { "s" }),
        theme.dim,
    )];
    spans.extend(counts_text((unavailable != count).then_some(&sum), theme));
    if more && unavailable > 0 && unavailable < count {
        spans.push(Span::styled(
            format!(" · {unavailable} uncounted"),
            theme.dim,
        ));
    }
    spans
}
