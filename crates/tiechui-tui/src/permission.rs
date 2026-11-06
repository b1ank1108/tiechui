//! The permission prompt: the request, its rule and session scope, then the choices, with a
//! reason editor for denials and Ctrl+O for the whole request.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};
use serde_json::Value;
use tiechui_protocol::ApprovalDecision;

use crate::prompt::{Input, Outcome, choice};
use crate::status_picker::surface;
use crate::theme::Theme;
use crate::{syntax, wrap};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Choice {
    Once,
    Session,
    BothDirections,
    Deny,
    Reason,
}

pub struct Permission {
    actor: Option<String>,
    tool: String,
    arguments: Value,
    surface: String,
    values: Vec<String>,
    pattern: Option<String>,
    scope: Vec<String>,
    choices: Vec<(&'static str, &'static str, Choice)>,
    selected: usize,
    expanded: bool,
    scroll: usize,
    reason: Option<Input>,
}

fn cancelled() -> ApprovalDecision {
    ApprovalDecision::Deny {
        reason: Some("Approval cancelled.".into()),
    }
}

impl Permission {
    pub fn new(
        actor: Option<String>,
        tool: String,
        arguments: Value,
        surface: String,
        values: Vec<String>,
        pattern: Option<String>,
        scope: Vec<String>,
    ) -> Self {
        let directional = ["path_", "external_directory_"].iter().any(|family| {
            surface
                .strip_prefix(family)
                .is_some_and(|direction| matches!(direction, "read" | "write"))
        });
        let mut choices = vec![("Yes, proceed", "Yes, proceed", Choice::Once)];
        if !scope.is_empty() {
            choices.push((
                "Yes, allow this scope for this session",
                "Yes, for this session",
                Choice::Session,
            ));
            if directional {
                choices.push((
                    "Yes, allow reads and writes to this scope for this session",
                    "Yes, session read + write",
                    Choice::BothDirections,
                ));
            }
        }
        choices.extend([
            ("No, continue without running it", "No, skip", Choice::Deny),
            (
                "No, and tell the agent what to do differently",
                "No, provide reason",
                Choice::Reason,
            ),
        ]);
        Self {
            actor,
            tool,
            arguments,
            surface,
            values,
            pattern,
            scope,
            choices,
            selected: 0,
            expanded: false,
            scroll: 0,
            reason: None,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Char('c') && control {
            return Outcome::Interrupt(cancelled());
        }
        if let Some(reason) = &mut self.reason {
            match key.code {
                KeyCode::Esc => self.reason = None,
                KeyCode::Enter => {
                    let text = reason.0.trim();
                    return Outcome::Answer(ApprovalDecision::Deny {
                        reason: (!text.is_empty()).then(|| text.to_owned()),
                    });
                }
                _ => {
                    reason.key(&key);
                }
            }
            return Outcome::Open;
        }
        let count = self.choices.len();
        match key.code {
            KeyCode::Esc => return Outcome::Interrupt(cancelled()),
            KeyCode::Enter => return self.submit(),
            KeyCode::Char('o') if control => {
                self.expanded = !self.expanded;
                self.scroll = 0;
            }
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(5),
            KeyCode::PageDown => self.scroll += 5,
            KeyCode::Up | KeyCode::Char('k') => self.selected = (self.selected + count - 1) % count,
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1) % count,
            KeyCode::Char(digit @ '1'..='9') => {
                let index = digit as usize - '1' as usize;
                if index < count {
                    self.selected = index;
                    return self.submit();
                }
            }
            _ => {}
        }
        Outcome::Open
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(reason) = &mut self.reason {
            reason.paste(text);
        }
    }

    fn submit(&mut self) -> Outcome {
        Outcome::Answer(match self.choices[self.selected].2 {
            Choice::Once => ApprovalDecision::AllowOnce,
            Choice::Session => ApprovalDecision::AllowSession {
                both_directions: false,
            },
            Choice::BothDirections => ApprovalDecision::AllowSession {
                both_directions: true,
            },
            Choice::Deny => ApprovalDecision::Deny { reason: None },
            Choice::Reason => {
                self.reason = Some(Input::default());
                return Outcome::Open;
            }
        })
    }

    fn title(&self) -> &'static str {
        if self.surface == "bash" {
            "Would you like to run the following command?"
        } else if matches!(self.tool.as_str(), "write" | "edit") {
            "Would you like to make the following edits?"
        } else {
            "Would you like to allow this operation?"
        }
    }

    /// The request, then any proposed file contents, as unwrapped rows.
    fn evidence(&self, theme: &Theme) -> Vec<Vec<Span<'static>>> {
        let mut rows = Vec::new();
        // The matched value alone; the others are its other spellings.
        let value = self.values.first().map_or("", String::as_str);
        if self.surface == "bash" {
            let command = value;
            let highlighted = syntax::highlight(command, "bash", theme).unwrap_or_else(|| {
                command
                    .split('\n')
                    .map(|line| vec![Span::raw(line.to_owned())])
                    .collect()
            });
            for (index, row) in highlighted.into_iter().enumerate() {
                let mut spans = vec![Span::raw(if index == 0 { "$ " } else { "  " })];
                spans.extend(row);
                rows.push(spans);
            }
        } else if value == "*" {
            rows.push(vec![Span::raw(format!("{} {}", self.tool, self.arguments))]);
        } else {
            rows.push(vec![Span::raw(value.to_owned())]);
        }
        let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
        let mut proposed = Vec::new();
        match self.tool.as_str() {
            "write" if self.arguments["content"].is_string() => {
                proposed.push("Proposed content:".to_owned());
                proposed.extend(
                    text(&self.arguments["content"])
                        .split('\n')
                        .map(str::to_owned),
                );
            }
            "edit" => {
                let edits = match &self.arguments["edits"] {
                    Value::Array(edits) => edits.clone(),
                    Value::Object(_) => vec![self.arguments["edits"].clone()],
                    _ => vec![self.arguments.clone()],
                };
                proposed.push("Proposed replacements:".to_owned());
                for edit in edits {
                    for (sign, key) in [("- ", "oldText"), ("+ ", "newText")] {
                        proposed.extend(text(&edit[key]).split('\n').map(|l| format!("{sign}{l}")));
                    }
                }
            }
            _ => {}
        }
        rows.extend(proposed.into_iter().map(|line| {
            let style = if line.starts_with("+ ") {
                theme.success
            } else if line.starts_with("- ") {
                theme.error
            } else {
                Style::default()
            };
            vec![Span::styled(line, style)]
        }));
        rows
    }

    pub fn render(
        &mut self,
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let height = height.saturating_sub(2).max(4);
        let compact = width < 50 || height < 24;
        let body = width.saturating_sub(2).max(1);
        let indent = [Span::raw("  ")];
        let wrapped = |spans: Vec<Span<'static>>| wrap::wrap(&spans, body, &indent, &indent);
        let fitted = |spans: Vec<Span<'static>>| {
            let mut line = vec![Span::raw("  ")];
            line.extend(spans);
            wrap::fit(line, body, true)
        };
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let rule = format!(
            "{} · {}",
            self.surface,
            self.pattern.as_deref().unwrap_or("default: ask")
        );
        let scope = (!self.scope.is_empty()).then(|| {
            let grants: Vec<String> = self
                .scope
                .iter()
                .map(|pattern| format!("{}: {pattern}", self.surface))
                .collect();
            format!(
                "Session scope{}: {}",
                if self.actor.is_some() {
                    " (this child only)"
                } else {
                    ""
                },
                grants.join("; ")
            )
        });
        let mut header = vec![Line::default()];
        header.extend(wrapped(vec![Span::styled(self.title(), bold)]));
        header.push(Line::default());
        header.extend(wrapped(vec![
            Span::raw("Thread: "),
            Span::styled(self.actor.clone().unwrap_or_else(|| "Main".into()), bold),
        ]));
        header.extend(wrapped(vec![
            Span::raw("Permission rule: "),
            Span::styled(rule.clone(), theme.accent),
        ]));
        if let Some(scope) = &scope {
            header.extend(wrapped(vec![Span::styled(scope.clone(), theme.dim)]));
        }
        header.push(Line::default());
        let evidence = self.evidence(theme);
        let mut full = header.clone();
        for row in &evidence {
            full.extend(wrapped(row.clone()));
        }
        full.extend(
            self.scope.iter().flat_map(|pattern| {
                wrapped(vec![Span::raw(format!("{}: {pattern}", self.surface))])
            }),
        );
        let summary = if compact {
            let mut summary = vec![
                fitted(vec![Span::styled(self.title(), bold)]),
                fitted(vec![Span::raw(format!(
                    "Thread: {}",
                    self.actor.as_deref().unwrap_or("Main")
                ))]),
                fitted(vec![Span::raw(format!(
                    "Rule: {} · {}",
                    self.surface,
                    self.pattern.as_deref().unwrap_or("ask")
                ))]),
            ];
            if let Some(scope) = scope {
                summary.push(fitted(vec![Span::raw(scope)]));
            }
            summary.extend(evidence.iter().take(3).map(|row| fitted(row.clone())));
            summary
        } else {
            let mut summary = header.clone();
            summary.extend(full.iter().skip(header.len()).take(5).cloned());
            summary
        };

        let mut cursor = None;
        let mut options = Vec::new();
        if let Some(reason) = &self.reason {
            options.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Tell the agent what to do differently:", theme.dim),
            ]));
            let (lines, (row, column)) = reason.render("", body, theme);
            // The last four rows, where the cursor is.
            let skip = lines.len().saturating_sub(4);
            cursor = Some((options.len() + row - skip, column));
            options.extend(lines.into_iter().skip(skip));
        } else {
            for (index, (label, short, _)) in self.choices.iter().enumerate() {
                let selected = index == self.selected;
                if compact {
                    let mut line = choice(index, short, None, selected, usize::MAX, theme);
                    options.push(wrap::fit(line.remove(0).spans, body, true));
                } else {
                    options.extend(choice(index, label, None, selected, body, theme));
                }
            }
        }
        let tip = if self.reason.is_some() {
            "enter to send · esc to go back · ctrl+c to interrupt"
        } else if compact {
            "↑/↓ · enter · esc · ctrl+o details"
        } else {
            "Press enter to confirm or esc to interrupt · ↑/↓ · ctrl+o details"
        };
        let footer: Vec<Line<'static>> = wrapped(vec![Span::styled(tip, theme.dim)])
            .into_iter()
            .take(2)
            .collect();
        if options.len() + footer.len() + 3 > height && self.reason.is_none() {
            let (label, _, _) = self.choices[self.selected];
            options = vec![fitted(vec![Span::styled(
                format!("{}/{} · {label}", self.selected + 1, self.choices.len()),
                theme.accent.add_modifier(Modifier::BOLD),
            )])];
        }
        let budget = height
            .saturating_sub(options.len() + footer.len() + 1)
            .max(1);
        self.scroll = self.scroll.min(full.len().saturating_sub(budget));
        let metadata: Vec<Line<'static>> = if self.expanded {
            full.into_iter().skip(self.scroll).take(budget).collect()
        } else if summary.len() > budget {
            let mut lines: Vec<_> = summary.into_iter().take(budget - 1).collect();
            lines.push(fitted(vec![Span::styled(
                "… ctrl+o for full request",
                theme.dim,
            )]));
            lines
        } else {
            summary
        };
        let top = metadata.len() + 1;
        let mut rows = metadata;
        rows.push(Line::default());
        rows.extend(options);
        rows.extend(footer);
        let rows = rows
            .into_iter()
            .map(|line| surface(line, width, theme))
            .collect();
        (rows, cursor.map(|(row, column)| (top + row, column)))
    }
}
