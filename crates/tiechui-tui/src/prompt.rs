//! Prompts the session waits on, one at a time: permission approvals and MCP elicitations.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span};
use tiechui_protocol::{ApprovalDecision, ApprovalRequest};

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

/// A one-line answer edited at its end.
#[derive(Default)]
pub struct Input(pub String);

impl Input {
    /// False for keys the caller handles.
    pub fn key(&mut self, key: &KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Backspace => {
                self.0.pop();
            }
            KeyCode::Char('u') if control => self.0.clear(),
            KeyCode::Char('w') if control => {
                let kept = self.0.trim_end().rfind(' ').map_or(0, |at| at + 1);
                self.0.truncate(kept);
            }
            KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.0.push(c);
            }
            _ => return false,
        }
        true
    }

    pub fn paste(&mut self, text: &str) {
        self.0.push_str(&text.replace(['\r', '\n'], " "));
    }

    /// `› text` wrapped under `indent`, or the dim placeholder; the cursor follows the text.
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
        let rest = [Span::raw("    ")];
        if self.0.is_empty() {
            let mut spans = first.to_vec();
            spans.push(Span::styled(placeholder.to_owned(), theme.dim));
            return (vec![wrap::fit(spans, width, true)], (0, 4));
        }
        let lines = wrap::wrap(&[Span::raw(self.0.clone())], width, &first, &rest);
        let last = lines.len() - 1;
        let column = wrap::line_width(&lines[last]).min(width.saturating_sub(1));
        (lines, (last, column))
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
