use tiechui_plugin_api::{
    ToolError, ToolOutput,
    model::Content,
    protocol::{ProgramCallState, ProgramRun},
    serde_json::{Value, json},
    tools::output::OutputAccumulator,
};

pub(super) fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|part| match part {
            Content::Text { text, .. } => Some(text.clone()),
            Content::Image { mime_type, data } => {
                let padding = data.bytes().rev().take_while(|&b| b == b'=').count();
                let bytes = (data.len() * 3 / 4).saturating_sub(padding);
                Some(format!("[image: {mime_type}, {bytes} bytes]"))
            }
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        text.into()
    } else {
        text.chars()
            .take(limit.saturating_sub(1))
            .chain(['…'])
            .collect()
    }
}

pub(super) fn arguments(mut value: Value) -> Value {
    match &mut value {
        Value::String(text) => *text = shorten(text, 200),
        Value::Array(items) => {
            for item in items {
                *item = arguments(item.take());
            }
        }
        Value::Object(fields) => {
            for item in fields.values_mut() {
                *item = arguments(item.take());
            }
        }
        _ => {}
    }
    value
}

pub(super) fn finish(
    result: super::engine::Result,
    record: ProgramRun,
    abandoned: usize,
) -> Result<ToolOutput, ToolError> {
    let is_error = result.value.is_err();
    let mut parts = Vec::new();
    match result.value {
        Ok(Some(value)) if !value.is_empty() => parts.push(value),
        Err(error) => parts.push(error),
        _ => {}
    }
    if !result.console.is_empty() {
        parts.push(format!("console:\n{}", result.console));
    }
    let succeeded = record
        .calls
        .iter()
        .filter(|call| call.state == ProgramCallState::Completed)
        .count();
    let failed: Vec<_> = record
        .calls
        .iter()
        .filter(|call| call.state == ProgramCallState::Failed)
        .collect();
    let cancelled = record
        .calls
        .iter()
        .filter(|call| call.state == ProgramCallState::Cancelled)
        .count();
    let mut summary = format!("tool calls: {succeeded} succeeded");
    if !failed.is_empty() {
        summary.push_str(&format!(", {} failed", failed.len()));
    }
    if cancelled > 0 {
        summary.push_str(&format!(", {cancelled} cancelled"));
    }
    for call in failed.iter().take(10) {
        summary.push('\n');
        summary.push_str(&shorten(
            &format!(
                "✗ {} {}: {}",
                call.name,
                call.arguments,
                call.error.as_deref().unwrap_or("Failed")
            )
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
            200,
        ));
    }
    if failed.len() > 10 {
        summary.push_str(&format!("\n{} more failed calls", failed.len() - 10));
    }
    if abandoned > 0 {
        summary.push_str(&format!(
            "\n{abandoned} calls were still running and were cancelled"
        ));
    }
    parts.push(summary);
    let mut output = OutputAccumulator::new();
    output.append(parts.join("\n\n").as_bytes())?;
    output.finish()?;
    Ok(ToolOutput {
        is_error,
        details: Some(json!(record)),
        ..ToolOutput::text(output.snapshot().text(""))
    })
}
