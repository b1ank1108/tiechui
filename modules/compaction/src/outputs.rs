use crate::{engine, messages::*, state::State};
use tiechui_plugin_api::{model::*, *};

/// Earlier-turn plain tool results large enough to preview that are neither recorded nor folded.
pub fn eligible(
    details: &std::collections::BTreeMap<String, serde_json::Value>,
    source: &Projection,
    state: &State,
    config: &CompactionConfig,
) -> Vec<String> {
    if !config.output.enabled {
        return vec![];
    }
    let recent = source
        .originals
        .iter()
        .rposition(|m| m.role == Role::User)
        .unwrap_or(0);
    let covered = state.covered();
    source
        .parts
        .iter()
        .filter(|part| {
            let message = &source.originals[part.owner];
            part.result
                && part.owner < recent
                && message.role == Role::Tool
                && message.opaque.is_none()
                && state.refs.contains_key(&part.id)
                && !state.outputs.contains(&part.id)
                && !covered.contains(&part.id)
                && !maintenance(&part.tool)
                && part.tool != "read"
                && !config
                    .protected_tools
                    .iter()
                    .any(|v| engine::matches(&part.tool, v))
                && !details.get(&part.call).is_some_and(|value| {
                    value["outputFile"].as_str().is_some_and(|s| !s.is_empty())
                        || value["fullOutputPath"]
                            .as_str()
                            .is_some_and(|s| !s.is_empty())
                })
                && message.content.iter().all(|content| match content {
                    Content::ToolResult(result) => {
                        !result.is_error
                            && !result.content.is_empty()
                            && result
                                .content
                                .iter()
                                .all(|p| matches!(p, Content::Text { opaque: None, .. }))
                    }
                    _ => false,
                })
                // Existing tool-managed references already provide recovery and must not loop.
                && !part.text.contains("\"outputFile\"")
                && !part.text.contains("Full output:")
                && !part.text.contains("Full text:")
                && part.text.chars().count() >= config.output.min_chars
        })
        .map(|part| part.id.clone())
        .collect()
}

/// Replaces recorded tool results with previews that read_history can expand.
pub fn apply(
    view: &mut [ProjectedMessage],
    source: &Projection,
    state: &State,
    config: &CompactionConfig,
    cancel: &CancellationToken,
) -> Result<(), ToolError> {
    if !config.output.enabled || state.outputs.is_empty() {
        return Ok(());
    }
    for item in view {
        check(cancel)?;
        let Some(part) = item.source.and_then(|index| {
            source
                .parts
                .iter()
                .find(|p| p.owner == index && p.result && state.outputs.contains(&p.id))
        }) else {
            continue;
        };
        let Some(reference) = state.refs.get(&part.id) else {
            continue;
        };
        let chars = part.text.chars().count();
        let head: String = part.text.chars().take(400).collect();
        let tail: String = part.text.chars().skip(chars.saturating_sub(400)).collect();
        for content in &mut item.message.content {
            if let Content::ToolResult(result) = content {
                result.content = vec![plain(format!(
                    "Tool output stored in history ({chars} characters). Full text: read_history {}\nPreview (first 400 and last 400 characters):\n{head}\n…\n{tail}",
                    serde_json::json!({"ref": reference})
                ))];
            }
        }
    }
    Ok(())
}
