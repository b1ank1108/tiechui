//! The MCP elicitation prompt: a form of text, boolean and single-choice fields, or
//! actions alone when the server asks for nothing or for fields the form cannot edit.

use std::collections::HashSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};
use serde_json::{Map, Value};
use tiechui_protocol::ApprovalDecision;

use crate::prompt::{Input, Outcome, choice};
use crate::status_picker::surface;
use crate::theme::Theme;
use crate::wrap;

enum Kind {
    Text(Input),
    Select {
        options: Vec<(String, Value)>,
        cursor: usize,
        /// The chosen option; a schema default counts as chosen.
        chosen: Option<usize>,
    },
}

struct Field {
    id: String,
    label: String,
    prompt: String,
    required: bool,
    kind: Kind,
}

impl Field {
    fn value(&self) -> Option<Value> {
        match &self.kind {
            Kind::Text(input) if !input.0.trim().is_empty() => Some(Value::String(input.0.clone())),
            Kind::Text(_) => None,
            Kind::Select {
                options, chosen, ..
            } => chosen.map(|index| options[index].1.clone()),
        }
    }
}

enum Mode {
    Actions {
        choices: Vec<(&'static str, &'static str, ApprovalDecision)>,
        selected: usize,
    },
    Form {
        fields: Vec<Field>,
        current: usize,
    },
}

pub struct Elicitation {
    label: &'static str,
    server: String,
    message: String,
    mode: Mode,
    error: Option<&'static str>,
}

fn accept() -> ApprovalDecision {
    ApprovalDecision::Accept { content: None }
}

impl Elicitation {
    pub fn plugin(plugin: &str, request: &Value) -> Self {
        let mut prompt = Self::new(plugin, request);
        prompt.label = "Plugin: ";
        prompt
    }
    pub fn new(server: &str, request: &Value) -> Self {
        let schema = &request["requestedSchema"];
        let empty = schema.is_null()
            || (schema["type"] == "object"
                && schema["properties"].as_object().is_some_and(Map::is_empty));
        let mode = match fields(schema).filter(|_| !empty) {
            Some(fields) => Mode::Form { fields, current: 0 },
            None if empty => Mode::Actions {
                choices: vec![
                    ("Allow", "Allow this request and continue", accept()),
                    (
                        "Deny",
                        "Decline this request and continue",
                        ApprovalDecision::Decline,
                    ),
                    ("Cancel", "Cancel this request", ApprovalDecision::Cancel),
                ],
                selected: 0,
            },
            None => Mode::Actions {
                choices: vec![
                    ("Yes, provide the requested info", "", accept()),
                    ("No, but continue without it", "", ApprovalDecision::Decline),
                    ("Cancel this request", "", ApprovalDecision::Cancel),
                ],
                selected: 0,
            },
        };
        Self {
            label: "Server: ",
            server: server.to_owned(),
            message: request["message"].as_str().unwrap_or_default().to_owned(),
            mode,
            error: None,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc || (key.code == KeyCode::Char('c') && control) {
            return Outcome::Answer(ApprovalDecision::Cancel);
        }
        match &mut self.mode {
            Mode::Actions { choices, selected } => {
                let count = choices.len();
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => *selected = (*selected + count - 1) % count,
                    KeyCode::Down | KeyCode::Char('j') => *selected = (*selected + 1) % count,
                    KeyCode::Char(digit @ '1'..='9') if (digit as usize - '1' as usize) < count => {
                        return Outcome::Answer(choices[digit as usize - '1' as usize].2.clone());
                    }
                    KeyCode::Enter => return Outcome::Answer(choices[*selected].2.clone()),
                    _ => {}
                }
                Outcome::Open
            }
            Mode::Form { fields, current } => {
                let count = fields.len();
                let field = &mut fields[*current];
                let mut step = None;
                match &mut field.kind {
                    Kind::Select {
                        options,
                        cursor,
                        chosen,
                    } => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            *cursor = (*cursor + options.len() - 1) % options.len();
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            *cursor = (*cursor + 1) % options.len();
                        }
                        KeyCode::Char(digit @ '1'..='9')
                            if (digit as usize - '1' as usize) < options.len() =>
                        {
                            *cursor = digit as usize - '1' as usize;
                            *chosen = Some(*cursor);
                        }
                        KeyCode::Char(' ') => *chosen = Some(*cursor),
                        KeyCode::Backspace | KeyCode::Delete => *chosen = None,
                        KeyCode::Left | KeyCode::Char('h') => step = Some(false),
                        KeyCode::Right | KeyCode::Char('l') => step = Some(true),
                        KeyCode::Enter => *chosen = Some(*cursor),
                        _ => {}
                    },
                    Kind::Text(input) => match key.code {
                        KeyCode::Char('p') if control => step = Some(false),
                        KeyCode::Char('n') if control => step = Some(true),
                        _ => {
                            input.key(&key);
                        }
                    },
                }
                match key.code {
                    KeyCode::PageUp => step = Some(false),
                    KeyCode::PageDown => step = Some(true),
                    KeyCode::Enter if *current + 1 < count => step = Some(true),
                    KeyCode::Enter => return self.submit(),
                    _ => {}
                }
                if let Some(next) = step {
                    *current = if next {
                        (*current + 1).min(count - 1)
                    } else {
                        current.saturating_sub(1)
                    };
                    self.error = None;
                }
                Outcome::Open
            }
        }
    }

    fn submit(&mut self) -> Outcome {
        let Mode::Form { fields, current } = &mut self.mode else {
            return Outcome::Open;
        };
        if let Some(missing) = fields
            .iter()
            .position(|field| field.required && field.value().is_none())
        {
            self.error = Some("Answer required fields before submitting.");
            *current = missing;
            return Outcome::Open;
        }
        let content: Map<String, Value> = fields
            .iter()
            .filter_map(|field| field.value().map(|value| (field.id.clone(), value)))
            .collect();
        Outcome::Answer(ApprovalDecision::Accept {
            content: Some(Value::Object(content)),
        })
    }

    pub fn paste(&mut self, text: &str) {
        if let Mode::Form { fields, current } = &mut self.mode
            && let Kind::Text(input) = &mut fields[*current].kind
        {
            input.paste(text);
        }
    }

    pub fn render(
        &mut self,
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let body = width.saturating_sub(2).max(1);
        let indent = [Span::raw("  ")];
        let text = |text: &str, style: Style| {
            text.split('\n')
                .flat_map(|line| {
                    wrap::wrap(
                        &[Span::styled(line.to_owned(), style)],
                        body,
                        &indent,
                        &indent,
                    )
                })
                .collect::<Vec<_>>()
        };
        let mut rows = vec![Line::default()];
        rows.extend(wrap::wrap(
            &[
                Span::raw(self.label),
                Span::styled(
                    self.server.clone(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
            ],
            body,
            &indent,
            &indent,
        ));
        if !self.message.trim().is_empty() {
            rows.push(Line::default());
            rows.extend(text(&self.message, Style::default()));
        }
        rows.push(Line::default());
        let mut cursor = None;
        let tips = match &self.mode {
            Mode::Actions { choices, selected } => {
                for (index, (label, detail, _)) in choices.iter().enumerate() {
                    let detail = (!detail.is_empty()).then_some(*detail);
                    rows.extend(choice(
                        index,
                        label,
                        detail,
                        index == *selected,
                        body,
                        theme,
                    ));
                }
                vec!["enter to submit", "esc to cancel"]
            }
            Mode::Form { fields, current } => {
                let field = &fields[*current];
                let mut heading = field.label.clone();
                if fields.len() > 1 {
                    heading = format!("{}/{} · {heading}", current + 1, fields.len());
                }
                if field.required {
                    heading.push_str(" (required)");
                }
                rows.extend(text(
                    &heading,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                if field.prompt != field.label {
                    rows.extend(text(&field.prompt, theme.dim));
                }
                match &field.kind {
                    Kind::Text(input) => {
                        let placeholder = if field.required {
                            "Type your answer"
                        } else {
                            "Type your answer (optional)"
                        };
                        let (lines, (row, column)) = input.render(placeholder, body, theme);
                        cursor = Some((rows.len() + row, column));
                        rows.extend(lines);
                    }
                    Kind::Select {
                        options,
                        cursor: at,
                        chosen,
                    } => {
                        for (index, (label, _)) in options.iter().enumerate() {
                            let detail = (*chosen == Some(index)).then_some("✓");
                            rows.extend(choice(index, label, detail, index == *at, body, theme));
                        }
                    }
                }
                let last = *current + 1 == fields.len();
                let mut tips = vec![match (fields.len(), last) {
                    (1, _) => "enter to submit",
                    (_, true) => "enter to submit all",
                    _ => "enter to submit answer",
                }];
                if fields.len() > 1 {
                    tips.push(match field.kind {
                        Kind::Select { .. } => "←/→ to navigate fields",
                        Kind::Text(_) => "ctrl+p / ctrl+n change field",
                    });
                }
                tips.push("esc to cancel");
                tips
            }
        };
        rows.push(Line::default());
        if let Some(error) = self.error {
            rows.extend(text(error, theme.accent.add_modifier(Modifier::BOLD)));
        }
        rows.extend(text(&tips.join(" | "), theme.dim));
        // The answer and the keys stay visible: the message gives way first.
        let skip = rows.len().saturating_sub(height.max(1));
        let rows = rows
            .into_iter()
            .skip(skip)
            .map(|line| surface(line, width, theme))
            .collect();
        (
            rows,
            cursor
                .filter(|(row, _)| *row >= skip)
                .map(|(row, column)| (row - skip, column)),
        )
    }
}

/// Supported fields: strings, booleans, and single-choice enums.
fn fields(schema: &Value) -> Option<Vec<Field>> {
    if schema["type"] != "object" {
        return None;
    }
    let required: HashSet<&str> = schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let fields = schema["properties"]
        .as_object()?
        .iter()
        .map(|(id, property)| field(id, property, required.contains(id.as_str())))
        .collect::<Option<Vec<_>>>()?;
    (!fields.is_empty()).then_some(fields)
}

fn field(id: &str, property: &Value, required: bool) -> Option<Field> {
    let label = property["title"].as_str().unwrap_or(id).to_owned();
    let prompt = property["description"]
        .as_str()
        .map_or_else(|| label.clone(), str::to_owned);
    let select = |options: Vec<(String, Value)>| {
        let chosen = options
            .iter()
            .position(|(_, value)| Some(value) == property.get("default"));
        Kind::Select {
            cursor: chosen.unwrap_or(0),
            chosen,
            options,
        }
    };
    let kind = match property["type"].as_str()? {
        "boolean" => select(vec![
            ("True".into(), Value::Bool(true)),
            ("False".into(), Value::Bool(false)),
        ]),
        "string" if property["enum"].is_array() => {
            let names = property["enumNames"].as_array();
            let options = property["enum"]
                .as_array()?
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let name = names
                        .and_then(|names| names.get(index))
                        .unwrap_or(value)
                        .as_str()?;
                    Some((name.to_owned(), Value::String(value.as_str()?.to_owned())))
                })
                .collect::<Option<Vec<_>>>()?;
            select(options)
        }
        "string" if property["oneOf"].is_array() => {
            let options = property["oneOf"]
                .as_array()?
                .iter()
                .map(|entry| {
                    let value = entry["const"].as_str()?;
                    let title = entry["title"].as_str().unwrap_or(value);
                    Some((title.to_owned(), Value::String(value.to_owned())))
                })
                .collect::<Option<Vec<_>>>()?;
            select(options)
        }
        "string" => Kind::Text(Input::default()),
        _ => return None,
    };
    Some(Field {
        id: id.to_owned(),
        label,
        prompt,
        required,
        kind,
    })
}
