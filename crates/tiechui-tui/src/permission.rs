//! The permission prompt: the request under a warning rule naming its rule, then the choices
//! with the session scope in them, a reason editor for denials and Ctrl+O for the whole request.

use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::style::Modifier;
use ratatui_core::text::{Line, Span};
use serde_json::Value;
use tiechui_protocol::ApprovalDecision;

use crate::prompt::{Input, Outcome};
use crate::sheet::{self, List, Row};
use crate::theme::Theme;
use crate::{diff, wrap};

/// Request rows shown before ctrl+o, on roomy and on small terminals.
const SUMMARY: [usize; 2] = [8, 3];

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
    pattern: Option<String>,
    /// The session scope, paths relative to the working directory.
    scope: Vec<String>,
    /// `read` or `write` for path surfaces.
    direction: Option<&'static str>,
    /// The matched value, paths relative to the working directory.
    shown: String,
    choices: Vec<Choice>,
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
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        actor: Option<String>,
        tool: String,
        arguments: Value,
        surface: String,
        values: Vec<String>,
        pattern: Option<String>,
        scope: Vec<String>,
        cwd: &Path,
        home: Option<&Path>,
    ) -> Self {
        let direction = ["path_", "external_directory_"].iter().find_map(|family| {
            match surface.strip_prefix(family)? {
                "read" => Some("read"),
                "write" => Some("write"),
                _ => None,
            }
        });
        let place = |value: &str| {
            let path = Path::new(value);
            match path.strip_prefix(cwd) {
                Ok(child) if !child.as_os_str().is_empty() => child.display().to_string(),
                _ => crate::status::compact_path(path, home),
            }
        };
        let paths = direction.is_some() || matches!(tool.as_str(), "write" | "edit");
        let shown = values.first().map_or_else(String::new, |value| {
            if paths { place(value) } else { value.clone() }
        });
        let scope: Vec<String> = if paths {
            scope.iter().map(|pattern| place(pattern)).collect()
        } else {
            scope
        };
        let mut choices = vec![Choice::Once];
        if !scope.is_empty() {
            choices.push(Choice::Session);
            if direction.is_some() {
                choices.push(Choice::BothDirections);
            }
        }
        choices.extend([Choice::Deny, Choice::Reason]);
        Self {
            actor,
            tool,
            arguments,
            surface,
            pattern,
            scope,
            direction,
            shown,
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
        Outcome::Answer(match self.choices[self.selected] {
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
            "Run this command?"
        } else if matches!(self.tool.as_str(), "write" | "edit") {
            "Apply these edits?"
        } else {
            "Allow this operation?"
        }
    }

    fn label(&self, choice: Choice, theme: &Theme) -> Vec<Span<'static>> {
        let grant = |verb: String| {
            let whom = self.actor.as_ref().map_or_else(
                || " for this session".into(),
                |a| format!(" for {a} in this session"),
            );
            vec![
                Span::raw(format!("Yes, and allow {verb}")),
                Span::styled(self.scope.join(", "), theme.accent),
                Span::raw(whom),
            ]
        };
        match choice {
            Choice::Once => vec![Span::raw("Yes")],
            Choice::Session => grant(match self.direction {
                Some("read") => "reads from ".into(),
                Some(_) => "writes to ".into(),
                None => String::new(),
            }),
            Choice::BothDirections => grant("reads and writes to ".into()),
            Choice::Deny => vec![Span::raw("No, continue without it")],
            Choice::Reason => vec![Span::raw("No, and tell the agent what to do instead")],
        }
    }

    /// The request, then any proposed file contents as a tinted diff.
    fn evidence(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let indent = [Span::raw("  ")];
        let wrapped = |spans: Vec<Span<'static>>| wrap::wrap(&spans, width, &indent, &indent);
        if self.surface == "bash" {
            return sheet::command(&self.shown, width, theme);
        }
        if self.shown == "*" {
            return wrapped(vec![Span::raw(format!("{} {}", self.tool, self.arguments))]);
        }
        let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
        let mut proposed: Vec<(bool, String)> = Vec::new();
        match self.tool.as_str() {
            "write" if self.arguments["content"].is_string() => {
                let content = text(&self.arguments["content"]);
                proposed.extend(content.split('\n').map(|l| (true, l.to_owned())));
            }
            "edit" => {
                let edits = match &self.arguments["edits"] {
                    Value::Array(edits) => edits.clone(),
                    Value::Object(_) => vec![self.arguments["edits"].clone()],
                    _ => vec![self.arguments.clone()],
                };
                for edit in edits {
                    for (added, key) in [(false, "oldText"), (true, "newText")] {
                        let lines = text(&edit[key]);
                        proposed.extend(lines.split('\n').map(|l| (added, l.to_owned())));
                    }
                }
            }
            _ => {}
        }
        let mut header = vec![Span::raw(self.shown.clone())];
        if proposed.is_empty() {
            return wrapped(header);
        }
        let added = proposed.iter().filter(|(added, _)| *added).count();
        header.extend([
            Span::raw("  "),
            Span::styled(format!("+{added}"), theme.success),
            Span::raw(" "),
            Span::styled(format!("−{}", proposed.len() - added), theme.error),
        ]);
        let rows: Vec<(bool, &str)> = proposed.iter().map(|(a, l)| (*a, l.as_str())).collect();
        let mut lines = wrapped(header);
        lines.extend(
            diff::signed(&rows, Some(&self.shown), width.saturating_sub(1), theme)
                .into_iter()
                .map(|line| {
                    let mut spans = vec![Span::raw(" ")];
                    spans.extend(line.spans);
                    Line::from(spans)
                }),
        );
        lines
    }

    pub fn render(
        &mut self,
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let height = height.saturating_sub(2).max(6);
        let compact = width < 50 || height < 22;
        let mut cursor = None;
        let mut options = Vec::new();
        if let Some(reason) = &self.reason {
            options.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("Tell the agent what to do instead", theme.dim),
            ]));
            let (lines, (row, column)) = reason.render("", width, theme);
            // The last four rows, where the cursor is.
            let skip = lines.len().saturating_sub(4);
            cursor = Some((options.len() + row - skip, column));
            options.extend(lines.into_iter().skip(skip));
        } else {
            let rows: Vec<Row> = self
                .choices
                .iter()
                .map(|choice| Row {
                    label: self.label(*choice, theme),
                    detail: Vec::new(),
                    current: false,
                })
                .collect();
            let list = List::new(&rows, rows.len(), width, false);
            for (index, row) in rows.iter().enumerate() {
                let selected = index == self.selected;
                if compact {
                    let mut line = list.row(index, row, selected, usize::MAX, theme);
                    options.push(wrap::fit(line.remove(0).spans, width, true));
                } else {
                    options.extend(list.row(index, row, selected, width, theme));
                }
            }
            if options.len() + 5 > height {
                let label: String = rows[self.selected]
                    .label
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                options = vec![wrap::fit(
                    vec![
                        Span::raw("  "),
                        Span::styled(
                            format!("› {}/{} · {label}", self.selected + 1, self.choices.len()),
                            theme.accent.add_modifier(Modifier::BOLD),
                        ),
                    ],
                    width,
                    true,
                )];
            }
        }
        let evidence = self.evidence(width, theme);
        let budget = height.saturating_sub(options.len() + 4).max(1);
        self.scroll = self.scroll.min(evidence.len().saturating_sub(budget));
        let limit = budget.min(SUMMARY[usize::from(compact)]);
        let cut = !self.expanded && evidence.len() > limit;
        let shown: Vec<Line<'static>> = if self.expanded {
            evidence
                .into_iter()
                .skip(self.scroll)
                .take(budget)
                .collect()
        } else if cut {
            let hidden = evidence.len() + 1 - limit;
            let mut lines: Vec<_> = evidence.into_iter().take(limit - 1).collect();
            lines.push(Line::from(vec![
                Span::styled(format!("  … {hidden} more lines · "), theme.dim),
                Span::styled("ctrl+o", theme.secondary),
            ]));
            lines
        } else {
            evidence
        };
        let top = shown.len() + 2;
        let mut body = shown;
        body.push(Line::default());
        body.extend(options);
        let range = format!("1-{}", self.choices.len());
        let pairs: Vec<(&str, &str)> = if self.reason.is_some() {
            vec![("enter", "send"), ("esc", "back"), ("ctrl+c", "interrupt")]
        } else {
            let mut pairs = vec![("enter", "confirm"), (range.as_str(), "pick")];
            if self.expanded {
                pairs.extend([("pgup/pgdn", "scroll"), ("ctrl+o", "less")]);
            } else if cut {
                pairs.push(("ctrl+o", "full request"));
            }
            pairs.push(("esc", "interrupt"));
            pairs
        };
        let rule = format!(
            "{} · {}",
            self.surface,
            self.pattern.as_deref().unwrap_or("default: ask")
        );
        let mut metas = Vec::new();
        if let Some(actor) = &self.actor {
            let actor = Span::styled(actor.clone(), theme.accent2);
            metas.push(vec![
                actor.clone(),
                Span::styled(" · ", theme.dim),
                Span::styled(rule.clone(), theme.dim),
            ]);
            metas.push(vec![actor]);
        } else {
            metas.push(vec![Span::styled(rule, theme.dim)]);
        }
        let rows = sheet::frame(
            theme.warning,
            sheet::rule(theme.warning, &[sheet::title(self.title())], &metas, width),
            body,
            sheet::hints(theme, &pairs, None, width),
            width,
        );
        (rows, cursor.map(|(row, column)| (top + row, column)))
    }
}
