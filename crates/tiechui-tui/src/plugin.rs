//! Declarative plugin surfaces consume protocol data only.

use crate::{
    elicitation::Elicitation,
    prompt::{Outcome, Prompt},
    theme::Theme,
    wrap,
};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui_core::text::{Line, Span};
use serde_json::json;
use tiechui_protocol::{PanelNode, PluginPanel, PluginPrompt};

pub fn prompt(plugin: String, prompt: PluginPrompt) -> Prompt {
    let (title, text, schema) = match prompt {
        PluginPrompt::Panel { panel } => return Prompt::PluginPanel(Panel::new(plugin, panel)),
        PluginPrompt::Confirm { title, text } => {
            (title, text, json!({"type":"object","properties":{}}))
        }
        PluginPrompt::Input { title, text } => (
            title,
            text,
            json!({"type":"object","properties":{"value":{"type":"string","title":"Answer"}},"required":["value"]}),
        ),
        PluginPrompt::Select {
            title,
            text,
            options,
        } => (
            title,
            text,
            json!({"type":"object","properties":{"value":{"type":"string","title":"Choose","enum":options}},"required":["value"]}),
        ),
    };
    Prompt::Elicitation(Elicitation::plugin(
        &plugin,
        &json!({"message":format!("{title}\n{text}"),"requestedSchema":schema}),
    ))
}

pub struct Panel {
    title: String,
    nodes: Vec<PanelNode>,
    form: Option<Elicitation>,
    scroll: usize,
}

impl Panel {
    pub fn new(plugin: String, panel: PluginPanel) -> Self {
        fn fields(
            nodes: &[PanelNode],
            properties: &mut serde_json::Map<String, serde_json::Value>,
            required: &mut Vec<String>,
        ) {
            for node in nodes {
                match node {
                    PanelNode::Form { fields } => {
                        for field in fields {
                            properties.insert(
                                field.id.clone(),
                                json!({"type":"string","title":field.label}),
                            );
                            if field.required {
                                required.push(field.id.clone());
                            }
                        }
                    }
                    PanelNode::List { children } => fields(children, properties, required),
                    _ => {}
                }
            }
        }
        let mut properties = serde_json::Map::new();
        let mut required = Vec::new();
        fields(&panel.nodes, &mut properties, &mut required);
        let form = (!properties.is_empty()).then(|| Elicitation::plugin(&plugin, &json!({"requestedSchema":{"type":"object","properties":properties,"required":required}})));
        Self {
            title: format!("{plugin}: {}", panel.title),
            nodes: panel.nodes,
            form,
            scroll: 0,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(4),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_add(4),
            _ => {
                if let Some(form) = &mut self.form {
                    return form.key(key);
                } else if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                    return Outcome::Answer(tiechui_protocol::ApprovalDecision::Cancel);
                }
            }
        }
        Outcome::Open
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(form) = &mut self.form {
            form.paste(text);
        }
    }

    pub fn render(
        &mut self,
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        fn nodes(
            nodes_: &[PanelNode],
            width: usize,
            theme: &Theme,
            lines: &mut Vec<Line<'static>>,
        ) {
            for node in nodes_ {
                match node {
                    PanelNode::Text { text } => {
                        for line in text.lines() {
                            lines.extend(wrap::wrap(
                                &[Span::raw(line.to_owned())],
                                width,
                                &[],
                                &[],
                            ));
                        }
                    }
                    PanelNode::Markdown { text } => {
                        lines.extend(crate::markdown::render(text, width, theme))
                    }
                    PanelNode::List { children } => {
                        let mut nested = Vec::new();
                        nodes(children, width.saturating_sub(2).max(1), theme, &mut nested);
                        for mut line in nested {
                            line.spans.insert(0, Span::raw("• "));
                            lines.push(line);
                        }
                    }
                    PanelNode::Form { .. } => {}
                    PanelNode::Progress {
                        label,
                        value,
                        total,
                    } => {
                        lines.push(wrap::fit(
                            vec![Span::styled(
                                format!("{label}: {value}/{total}"),
                                theme.accent,
                            )],
                            width,
                            false,
                        ));
                    }
                }
            }
        }
        let mut body = Vec::new();
        nodes(&self.nodes, width, theme, &mut body);
        let form = self
            .form
            .as_mut()
            .map(|f| f.render(width, height.saturating_sub(3).min(10), theme));
        let reserved = form.as_ref().map_or(0, |(lines, _)| lines.len());
        let room = height.saturating_sub(reserved + 2);
        self.scroll = self.scroll.min(body.len().saturating_sub(room));
        let mut lines = vec![wrap::fit(
            vec![Span::styled(self.title.clone(), theme.accent)],
            width,
            false,
        )];
        lines.extend(body.into_iter().skip(self.scroll).take(room));
        let mut cursor = None;
        if let Some((form, at)) = form {
            cursor = at.map(|(row, col)| (lines.len() + row, col));
            lines.extend(form);
        }
        lines.push(wrap::fit(
            vec![Span::styled("pgup/pgdn scroll · esc close", theme.dim)],
            width,
            false,
        ));
        lines.truncate(height);
        (lines, cursor.filter(|(row, _)| *row < height))
    }
}
