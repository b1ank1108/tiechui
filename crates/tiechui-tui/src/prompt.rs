//! Prompts the session waits on, one at a time: permission approvals and MCP elicitations.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span};
use tiechui_protocol::{ApprovalDecision, ApprovalRequest};

use crate::composer::Composer;
use crate::elicitation::Elicitation;
use crate::permission::Permission;
use crate::theme::Theme;
use crate::wrap;

pub enum Outcome {
    Open,
    Answer(ApprovalDecision),
    /// Answers, then interrupts the turn.
    Interrupt(ApprovalDecision),
}

pub enum Prompt {
    PluginPanel(crate::plugin::Panel),
    Information(crate::information::Information),
    Editor(crate::editor::Editor),
    Rewind(crate::rewind::Rewind),
    Ask(crate::ask::Ask),
    Permission(Permission),
    Elicitation(Elicitation),
}

impl Prompt {
    /// `cwd` and `home` shorten paths in permission requests.
    pub fn new(request: ApprovalRequest, cwd: &Path, home: Option<&Path>) -> Self {
        match request {
            ApprovalRequest::Plugin { plugin, prompt } => crate::plugin::prompt(plugin, prompt),
            ApprovalRequest::Information { title, text } => {
                Self::Information(crate::information::Information::new(title, text))
            }
            ApprovalRequest::Editor { title, text } => {
                Self::Editor(crate::editor::Editor::new(title, text))
            }
            ApprovalRequest::Rewind { panel } => Self::Rewind(crate::rewind::Rewind::new(panel)),
            ApprovalRequest::Questions { questions } => Self::Ask(crate::ask::Ask::new(questions)),
            ApprovalRequest::Permission {
                actor,
                tool,
                arguments,
                surface,
                values,
                matched_pattern,
                session_patterns,
                ..
            } => Self::Permission(Permission::new(
                actor.map(String::from),
                tool,
                arguments,
                surface,
                values,
                matched_pattern,
                session_patterns,
                cwd,
                home,
            )),
            ApprovalRequest::Elicitation { server, request } => {
                Self::Elicitation(Elicitation::new(&server, &request))
            }
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        match self {
            Self::PluginPanel(prompt) => prompt.key(key),
            Self::Information(prompt) => prompt.key(key),
            Self::Editor(prompt) => prompt.key(key),
            Self::Rewind(prompt) => prompt.key(key),
            Self::Ask(prompt) => prompt.key(key),
            Self::Permission(prompt) => prompt.key(key),
            Self::Elicitation(prompt) => prompt.key(key),
        }
    }

    pub fn paste(&mut self, text: &str) {
        match self {
            Self::PluginPanel(prompt) => prompt.paste(text),
            Self::Information(_) => {}
            Self::Editor(prompt) => prompt.paste(text),
            Self::Rewind(_) => {}
            Self::Ask(prompt) => prompt.paste(text),
            Self::Permission(prompt) => prompt.paste(text),
            Self::Elicitation(prompt) => prompt.paste(text),
        }
    }

    /// At most `height` rows on the input surface, with the cursor in a text answer.
    pub fn render(
        &mut self,
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        match self {
            Self::PluginPanel(prompt) => prompt.render(width, height, theme),
            Self::Information(prompt) => (prompt.render(width, height, theme), None),
            Self::Editor(prompt) => prompt.render(width, height, theme),
            Self::Rewind(prompt) => (prompt.render(width, height.saturating_sub(2), theme), None),
            Self::Ask(prompt) => prompt.render(width, height.saturating_sub(2), theme),
            Self::Permission(prompt) => prompt.render(width, height, theme),
            Self::Elicitation(prompt) => prompt.render(width, height, theme),
        }
    }
}

/// A one-line answer with the composer's cursor and editing keys. Boxed so the prompts that
/// hold one stay small.
#[derive(Default)]
pub struct Input(Box<Composer>);

impl Input {
    /// `text` with the cursor at its end.
    pub fn new(text: &str) -> Self {
        let mut input = Self::default();
        input.0.insert(text);
        input
    }

    pub fn text(&self) -> &str {
        self.0.text()
    }

    /// A byte offset into `text`.
    pub fn cursor(&self) -> usize {
        self.0.cursor()
    }

    /// Changes whenever the text does, but not when only the cursor moves.
    pub fn revision(&self) -> u64 {
        self.0.revision()
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    /// False for keys the caller handles.
    pub fn key(&mut self, key: &KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Backspace if control || alt => self.0.delete_word_backward(),
            KeyCode::Backspace => self.0.backspace(),
            KeyCode::Delete => self.0.delete_forward(),
            KeyCode::Left if control || alt => self.0.word_backward(),
            KeyCode::Right if control || alt => self.0.word_forward(),
            KeyCode::Left => self.0.left(),
            KeyCode::Right => self.0.right(),
            KeyCode::Home => self.0.home(),
            KeyCode::End => self.0.end(),
            KeyCode::Char('a') if control => self.0.home(),
            KeyCode::Char('e') if control => self.0.end(),
            KeyCode::Char('u') if control => self.0.delete_to_line_start(),
            KeyCode::Char('k') if control => self.0.delete_to_line_end(),
            KeyCode::Char('w') if control => self.0.delete_word_backward(),
            KeyCode::Char('z') if control => self.0.undo(),
            KeyCode::Char(c) if !control && !alt => self.0.insert(&c.to_string()),
            _ => return false,
        }
        true
    }

    pub fn paste(&mut self, text: &str) {
        self.0.insert(&text.replace(['\r', '\n'], " "));
    }

    /// `› text` wrapped under `indent`, or the dim placeholder, with the cursor in it.
    pub fn render(
        &self,
        placeholder: &str,
        width: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, (usize, usize)) {
        let first = [
            Span::raw("  "),
            Span::styled("› ", theme.accent.add_modifier(Modifier::BOLD)),
        ];
        if self.0.is_empty() {
            let mut spans = first.to_vec();
            spans.push(Span::styled(placeholder.to_owned(), theme.dim));
            return (vec![wrap::fit(spans, width, true)], (0, 4));
        }
        let avail = width.saturating_sub(4).max(1);
        let (mut rows, (mut row, mut column)) = self.0.rows(avail);
        if column >= avail {
            // A full last row puts the cursor on a virtual row below it.
            row += 1;
            column = 0;
            rows.insert(row, self.cursor()..self.cursor());
        }
        let lines = rows
            .into_iter()
            .enumerate()
            .map(|(index, range)| {
                let mut spans = if index == 0 {
                    first.to_vec()
                } else {
                    vec![Span::raw("    ")]
                };
                spans.push(Span::raw(self.text()[range].to_owned()));
                Line::from(spans)
            })
            .collect();
        (lines, (row, 4 + column))
    }
}

/// `1. label  detail` wrapped under the selection marker; the selected row is accented and
/// bold, the detail dim.
pub fn choice(
    index: usize,
    label: &str,
    detail: Option<&str>,
    selected: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let style = if selected {
        theme.accent.add_modifier(Modifier::BOLD)
    } else {
        Default::default()
    };
    let marker = if selected { "› " } else { "  " };
    let mut spans = vec![Span::styled(format!("{}. {label}", index + 1), style)];
    if let Some(detail) = detail {
        spans.push(Span::styled(format!("  {detail}"), theme.dim));
    }
    wrap::wrap(
        &spans,
        width,
        &[Span::raw("  "), Span::styled(marker, style)],
        &[Span::raw("    ")],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::ColorLevel;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn an_input_edits_at_its_cursor_and_shows_it_on_wrapped_rows() {
        let mut input = Input::new("keep the fixture safe");
        input.key(&press(KeyCode::Left, KeyModifiers::ALT));
        input.key(&press(KeyCode::Left, KeyModifiers::ALT));
        input.paste("test\n");
        assert_eq!(input.text(), "keep the test fixture safe");
        input.key(&press(KeyCode::Char('k'), KeyModifiers::CONTROL));
        input.key(&press(KeyCode::Home, KeyModifiers::NONE));
        input.key(&press(KeyCode::Delete, KeyModifiers::NONE));
        input.key(&press(KeyCode::Char('K'), KeyModifiers::NONE));
        assert_eq!(input.text(), "Keep the test ");
        assert!(!input.key(&press(KeyCode::Enter, KeyModifiers::NONE)));
        input.key(&press(KeyCode::End, KeyModifiers::NONE));
        input.key(&press(KeyCode::Left, KeyModifiers::CONTROL));
        let theme = Theme::fixture(ColorLevel::TrueColor, false);
        let (lines, cursor) = input.render("", 12, &theme);
        let rows: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(rows, ["  › Keep the ", "    test "]);
        assert_eq!(cursor, (1, 4));
    }
}
