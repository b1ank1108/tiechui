//! Plain goal reports; objectives are data, never terminal control sequences or Markdown.
use crate::{theme::Theme, tools, wrap};
use ratatui_core::{
    style::{Modifier, Style},
    text::{Line, Span},
};

pub fn report(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    tools::plain(text)
        .lines()
        .flat_map(|line| {
            let fields = ["Status: ", "Objective: ", "Time used: ", "Tokens used: "];
            let spans = if line == "Goal" {
                vec![Span::styled(
                    line.to_owned(),
                    Style::default().add_modifier(Modifier::BOLD),
                )]
            } else if let Some(field) = fields.iter().find(|field| line.starts_with(**field)) {
                vec![
                    Span::styled((*field).to_owned(), theme.dim),
                    Span::raw(line[field.len()..].to_owned()),
                ]
            } else if line.starts_with("/goal") || line.starts_with("Commands:") {
                vec![Span::styled(line.to_owned(), theme.dim)]
            } else {
                vec![Span::raw(line.to_owned())]
            };
            wrap::wrap(&spans, width, &[], &[])
        })
        .collect()
}
