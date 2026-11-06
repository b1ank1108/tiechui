use ratatui_core::text::{Line, Span};
use tiechui_protocol::{ProgramCallState, ProgramRun};

use crate::{
    syntax,
    theme::Theme,
    tools::{self, Tool, View, plain},
    wrap,
};

pub(crate) fn render(tool: &Tool, width: usize, theme: &Theme, view: View) -> Vec<Line<'static>> {
    let expanded = matches!(view, View::Transcript);
    let code = tool.arguments["code"].as_str().unwrap_or_default();
    let title = tool.arguments["description"]
        .as_str()
        .unwrap_or_else(|| code.lines().next().unwrap_or_default());
    let title = plain(title)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut lines = vec![wrap::fit(
        vec![
            Span::styled(
                if tool.done() {
                    "Ran code"
                } else {
                    "Running code"
                },
                if tool.error {
                    theme.error
                } else {
                    theme.secondary
                },
            ),
            Span::raw(format!(" · {title}")),
        ],
        width,
        true,
    )];
    let output = tool.output.as_ref().or(tool.partial.as_ref());
    if let Some(record) = output
        .and_then(|output| serde_json::from_value::<ProgramRun>(output["details"].clone()).ok())
    {
        let skip = if expanded {
            0
        } else {
            record.calls.len().saturating_sub(6)
        };
        if skip > 0 {
            lines.push(wrap::fit(
                vec![Span::styled(format!("  +{skip} more"), theme.dim)],
                width,
                true,
            ));
        }
        for call in &record.calls[skip..] {
            let child = Tool::new(call.id.clone(), call.name.clone(), call.arguments.clone());
            let (marker, style) = match call.state {
                ProgramCallState::Running => ("…", theme.dim),
                ProgramCallState::Completed => ("✓", theme.dim),
                ProgramCallState::Failed => ("✗", theme.error),
                ProgramCallState::Cancelled => ("−", theme.error),
            };
            lines.push(wrap::fit(
                vec![Span::styled(
                    format!("  {marker} {}", tools::summary(&child)),
                    style,
                )],
                width,
                true,
            ));
        }
        let count = |state| {
            record
                .calls
                .iter()
                .filter(|call| call.state == state)
                .count()
        };
        let mut counts = format!("  {} succeeded", count(ProgramCallState::Completed));
        for (state, label) in [
            (ProgramCallState::Failed, "failed"),
            (ProgramCallState::Cancelled, "cancelled"),
            (ProgramCallState::Running, "running"),
        ] {
            if count(state) > 0 {
                counts.push_str(&format!(", {} {label}", count(state)));
            }
        }
        lines.push(wrap::fit(
            vec![Span::styled(counts, theme.dim)],
            width,
            true,
        ));
    }
    if expanded {
        lines.push(Line::default());
        let code = plain(code);
        let rows = syntax::highlight(&code, "js", theme).unwrap_or_else(|| {
            code.lines()
                .map(|line| vec![Span::raw(line.to_owned())])
                .collect()
        });
        for row in rows {
            lines.extend(wrap::wrap(&row, width, &[], &[]));
        }
        if let Some(output) = output {
            lines.push(Line::default());
            for line in tools::output_text(output).lines() {
                lines.extend(wrap::wrap(&[Span::raw(line.to_owned())], width, &[], &[]));
            }
        }
    } else if tool.error && output.is_some_and(|output| output["details"].is_null()) {
        lines.extend(wrap::wrap(
            &[Span::styled(
                tools::output_text(output.unwrap_or(&serde_json::Value::Null)),
                theme.error,
            )],
            width,
            &[],
            &[],
        ));
    }
    lines
}
