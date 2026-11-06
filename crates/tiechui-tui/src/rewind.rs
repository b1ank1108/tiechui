//! Protocol-only rewind list and confirmation panel.

use crate::{prompt::Outcome, theme::Theme, wrap};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_core::{
    style::{Modifier, Style},
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
        let mut lines = vec![
            Line::from(Span::styled("─".repeat(width), theme.separator)),
            Line::from(Span::styled(
                " Rewind",
                Style::default().add_modifier(Modifier::BOLD),
            )),
        ];
        let short = height < 20;
        let selected = |text: String| {
            wrap::fit(
                vec![Span::styled(
                    text,
                    theme.user().add_modifier(Modifier::BOLD),
                )],
                width,
                false,
            )
        };
        let wrapped = |text: &str, count: usize, style| {
            wrap::wrap(
                &[Span::styled(clean(text), style)],
                width,
                &[Span::raw(" ")],
                &[Span::raw(" ")],
            )
            .into_iter()
            .take(count)
            .collect::<Vec<_>>()
        };
        match self.panel.page {
            RewindPage::Messages => {
                lines.extend(wrapped(
                    "Select the point you want to restore to.",
                    2,
                    Style::default(),
                ));
                lines.push(Line::default());
                let count = ((height.saturating_sub(10)) / 3).clamp(1, 8);
                let start = self
                    .panel
                    .selected
                    .saturating_sub(count / 2)
                    .min(self.panel.rows.len().saturating_sub(count));
                if start > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("   ↑ {start} earlier prompts"),
                        theme.dim,
                    )));
                }
                for (index, row) in self.panel.rows.iter().enumerate().skip(start).take(count) {
                    let text = if row.text.is_empty() {
                        "(attachment)"
                    } else {
                        &row.text
                    };
                    for (line, text) in wrap::wrap(
                        &[Span::raw(clean(text))],
                        width.saturating_sub(4).max(1),
                        &[],
                        &[],
                    )
                    .into_iter()
                    .take(2)
                    .enumerate()
                    {
                        let text = text
                            .spans
                            .iter()
                            .map(|span| span.content.as_ref())
                            .collect::<String>();
                        let marker = if index == self.panel.selected && line == 0 {
                            "›"
                        } else {
                            " "
                        };
                        let label = format!(" {marker} {text}");
                        lines.push(if index == self.panel.selected {
                            selected(label)
                        } else {
                            wrap::fit(vec![Span::raw(label)], width, true)
                        });
                    }
                    let mut spans = vec![Span::styled(
                        format!(
                            "   {}{} · ",
                            time(row.timestamp),
                            if index + 1 == self.panel.rows.len() {
                                "  (latest)"
                            } else {
                                ""
                            }
                        ),
                        theme.dim,
                    )];
                    spans.extend(summary(row.stats.as_ref(), theme));
                    lines.push(wrap::fit(spans, width, true));
                }
                if start + count < self.panel.rows.len() {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "   ↓ {} later prompts",
                            self.panel.rows.len() - start - count
                        ),
                        theme.dim,
                    )));
                }
                lines.push(Line::default());
                lines.extend(wrapped(
                    "↑↓ move · Home/End · Enter to continue · Esc to cancel",
                    2,
                    theme.dim,
                ));
            }
            RewindPage::Checking | RewindPage::Actions => {
                if !short {
                    lines.extend(wrapped(
                        "Restore to the point before you sent this message:",
                        2,
                        Style::default(),
                    ));
                }
                let row = self.panel.rows.get(self.panel.selected);
                let text = row
                    .map(|row| row.text.as_str())
                    .filter(|text| !text.is_empty())
                    .unwrap_or("(attachment)");
                lines.extend(wrapped(
                    text,
                    if height < 28 { 1 } else { 3 },
                    Style::default(),
                ));
                if self.panel.page == RewindPage::Checking {
                    lines.push(Line::from(Span::styled(" Checking checkpoint…", theme.dim)));
                    lines.push(Line::from(Span::styled(" Esc to go back", theme.dim)));
                } else {
                    let mut spans = vec![Span::raw(" ")];
                    spans.extend(summary(self.panel.preview.as_ref(), theme));
                    lines.push(wrap::fit(spans, width, true));
                    if let Some(stats) = &self.panel.preview {
                        if let Some(error) = &stats.error {
                            lines.extend(wrapped(error, if short { 1 } else { 2 }, theme.warning));
                        }
                        if !short {
                            lines
                                .push(Line::from(Span::styled(" checkpoint → current", theme.dim)));
                            let limit = if height >= 28 {
                                3
                            } else if height >= 24 {
                                1
                            } else {
                                0
                            };
                            for (file, counts) in stats.files.iter().take(limit) {
                                let mut spans =
                                    vec![Span::styled(format!("   {}  ", clean(file)), theme.dim)];
                                spans.extend(counts_text(counts.as_ref(), theme));
                                lines.push(wrap::fit(spans, width, true));
                            }
                            if limit > 0 && stats.files.len() > limit {
                                lines.push(Line::from(Span::styled(
                                    format!("   … {} more files", stats.files.len() - limit),
                                    theme.dim,
                                )));
                            }
                        }
                    }
                    self.render_actions(&mut lines, width, height, theme);
                }
            }
            RewindPage::Recovery => {
                lines.extend(wrapped(
                    "Files are checked before recovery. The conversation position is not changed.",
                    3,
                    theme.dim,
                ));
                self.render_actions(&mut lines, width, height, theme);
            }
        }
        lines
            .into_iter()
            .map(|line| wrap::fit(line.spans, width, true))
            .take(height)
            .collect()
    }
    fn render_actions(
        &self,
        lines: &mut Vec<Line<'static>>,
        width: usize,
        height: usize,
        theme: &Theme,
    ) {
        let actions = self.actions();
        if height >= 20 {
            lines.push(Line::default());
        }
        for (index, (_, label, _)) in actions.iter().enumerate() {
            let style = if index == self.action {
                theme.user().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(wrap::fit(
                vec![Span::styled(
                    format!(
                        " {} {}. {label}",
                        if index == self.action { "›" } else { " " },
                        index + 1
                    ),
                    style,
                )],
                width,
                false,
            ));
        }
        if height >= 20 {
            lines.push(Line::default());
            lines.extend(
                wrap::wrap(
                    &[Span::styled(actions[self.action].2, theme.dim)],
                    width,
                    &[Span::raw(" ")],
                    &[Span::raw(" ")],
                )
                .into_iter()
                .take(2),
            );
            lines.extend(
                wrap::wrap(
                    &[Span::styled(
                        "Bash, manual and other-session edits are not guaranteed to rewind.",
                        theme.dim,
                    )],
                    width,
                    &[Span::raw(" ")],
                    &[Span::raw(" ")],
                )
                .into_iter()
                .take(2),
            );
        }
        lines.push(wrap::fit(
            vec![Span::styled(
                if height < 20 {
                    " Enter / number confirms · Esc back"
                } else {
                    " Enter / number to confirm · Esc to go back"
                },
                theme.dim,
            )],
            width,
            true,
        ));
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
fn summary(stats: Option<&CheckpointStats>, theme: &Theme) -> Vec<Span<'static>> {
    let Some(stats) = stats else {
        return vec![Span::styled("Checking…", theme.dim)];
    };
    if stats.error.is_some() {
        return vec![Span::styled(
            "No code restore · checkpoint unavailable or conflicting",
            theme.warning,
        )];
    }
    let mut spans = vec![Span::styled(
        if stats.files.is_empty() {
            "No code changes".into()
        } else {
            format!("{} files changed", stats.files.len())
        },
        theme.dim,
    )];
    if !stats.files.is_empty() {
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
        spans.push(Span::styled(" · ", theme.dim));
        spans.extend(counts_text(
            (unavailable != stats.files.len()).then_some(&sum),
            theme,
        ));
        if unavailable > 0 && unavailable < stats.files.len() {
            spans.push(Span::styled(
                format!(" ({unavailable} uncounted)"),
                theme.dim,
            ));
        }
    }
    spans
}
