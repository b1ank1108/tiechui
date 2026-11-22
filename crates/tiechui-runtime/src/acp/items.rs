//! Pure normalization from ACP session updates, tools, plans, usage, commands, and models to
//! the session shapes Tiechui clients render.

use std::collections::HashSet;

use serde_json::{Map, Value, json};
use tiechui_protocol::{CommandInfo, ModelInfo, ThinkingLevel, TokenUsage};

use super::dialect::{AcpDialect, ToolLocation, ToolState};
use super::wire::{AcpUsageWire, AvailableCommandWire, SessionSetupWire};
use crate::tool_output;

impl ToolState {
    pub fn merge_from(&mut self, raw: &Value) {
        if let Some(id) = non_empty_str(&raw["toolCallId"]) {
            self.id = id;
        }
        merge_opt_str(&mut self.name, &raw["name"]);
        merge_opt_str(&mut self.title, &raw["title"]);
        merge_opt_str(&mut self.kind, &raw["kind"]);
        merge_opt_str(&mut self.status, &raw["status"]);
        self.merge_payloads(raw);
    }

    fn merge_payloads(&mut self, raw: &Value) {
        if let Some(locs) = raw["locations"].as_array() {
            self.locations = parse_locations(locs);
        }
        if let Some(input) = raw.get("rawInput").or_else(|| raw.get("args"))
            && !input.is_null()
        {
            self.raw_input = Some(input.clone());
        }
        if let Some(output) = raw.get("rawOutput")
            && !output.is_null()
        {
            self.raw_output = Some(output.clone());
        }
        if let Some(content) = raw["content"].as_array() {
            self.content = content.clone();
        }
        if let Some(meta) = raw.get("_meta")
            && !meta.is_null()
        {
            self.meta = Some(meta.clone());
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_deref(), Some("completed" | "failed"))
    }
}

fn non_empty_str(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn merge_opt_str(slot: &mut Option<String>, value: &Value) {
    if let Some(text) = non_empty_str(value) {
        *slot = Some(text);
    }
}

fn parse_locations(items: &[Value]) -> Vec<ToolLocation> {
    items
        .iter()
        .filter_map(|item| {
            let path = non_empty_str(&item["path"])?;
            Some(ToolLocation {
                path,
                line: item["line"].as_u64(),
            })
        })
        .collect()
}

pub fn normalized_call(state: &ToolState) -> (String, Value) {
    if let Some(diff) = first_diff(&state.content) {
        return diff_call(state.kind.as_deref(), diff);
    }
    match state.kind.as_deref() {
        Some("execute") => ("bash".into(), bash_args(state)),
        Some("read") => (read_tool_name(state).into(), read_args(state)),
        Some("search") => (search_tool_name(state).into(), search_args(state)),
        Some("write") => ("write".into(), write_args(state)),
        Some("edit") => ("edit".into(), path_args(state)),
        Some("delete" | "move") => (state.kind.clone().unwrap_or_default(), path_args(state)),
        _ => fallback_call(state),
    }
}

fn read_tool_name(state: &ToolState) -> &'static str {
    match state.name.as_deref() {
        Some("ls") => "ls",
        _ => "read",
    }
}

fn search_tool_name(state: &ToolState) -> &'static str {
    match state.name.as_deref() {
        Some("find") => "find",
        _ => "grep",
    }
}

fn fallback_call(state: &ToolState) -> (String, Value) {
    if state
        .raw_input
        .as_ref()
        .and_then(|v| non_empty_str(&v["command"]))
        .is_some()
    {
        return ("bash".into(), bash_args(state));
    }
    let name = state
        .name
        .clone()
        .or_else(|| state.title.clone())
        .or_else(|| state.kind.clone())
        .unwrap_or_else(|| "tool".into());
    let args = state.raw_input.clone().unwrap_or_else(|| json!({}));
    (name, args)
}

pub fn first_diff(content: &[Value]) -> Option<(&str, Option<&str>, &str)> {
    content.iter().find_map(|block| {
        if block["type"].as_str() != Some("diff") {
            return None;
        }
        let path = block["path"].as_str()?;
        let new_text = block["newText"].as_str().unwrap_or_default();
        Some((path, block["oldText"].as_str(), new_text))
    })
}

fn diff_call(
    kind: Option<&str>,
    (path, old_text, new_text): (&str, Option<&str>, &str),
) -> (String, Value) {
    if kind == Some("write") {
        return ("write".into(), json!({ "path": path, "content": new_text }));
    }
    let edit = json!({
        "oldText": old_text.unwrap_or_default(),
        "newText": new_text,
    });
    ("edit".into(), json!({ "path": path, "edits": [edit] }))
}

fn bash_args(state: &ToolState) -> Value {
    let command = state
        .raw_input
        .as_ref()
        .and_then(|v| non_empty_str(&v["command"]))
        .or_else(|| state.title.clone())
        .unwrap_or_default();
    let mut map = object_of(state.raw_input.as_ref());
    map.insert("command".into(), json!(command));
    Value::Object(map)
}

fn read_args(state: &ToolState) -> Value {
    let mut map = object_of(state.raw_input.as_ref());
    if !map.contains_key("path")
        && let Some(loc) = state.locations.first()
    {
        map.insert("path".into(), json!(loc.path));
        if let Some(line) = loc.line {
            map.entry("offset").or_insert_with(|| json!(line));
        }
    }
    Value::Object(map)
}

fn search_args(state: &ToolState) -> Value {
    let mut map = object_of(state.raw_input.as_ref());
    if !map.contains_key("pattern")
        && !map.contains_key("query")
        && let Some(title) = &state.title
    {
        map.insert("pattern".into(), json!(title));
    }
    Value::Object(map)
}

fn map_with_path(state: &ToolState) -> Map<String, Value> {
    let mut map = object_of(state.raw_input.as_ref());
    if !map.contains_key("path")
        && let Some(loc) = state.locations.first()
    {
        map.insert("path".into(), json!(loc.path));
    }
    map
}

fn path_args(state: &ToolState) -> Value {
    Value::Object(map_with_path(state))
}

fn write_args(state: &ToolState) -> Value {
    let mut map = map_with_path(state);
    map.entry("content").or_insert_with(|| json!(""));
    Value::Object(map)
}

fn object_of(value: Option<&Value>) -> Map<String, Value> {
    match value {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

pub fn normalized_result(tool_name: &str, state: &ToolState) -> (Value, bool) {
    let exit_code = extract_exit_code(state.raw_output.as_ref());
    let failed = state.status.as_deref() == Some("failed") || exit_code.is_some_and(|c| c != 0);
    let raw_text = extract_output_text(state);
    let text = if tool_name == "bash" {
        format_bash_text(&raw_text, exit_code, failed)
    } else {
        raw_text
    };
    (tool_output(&text, failed, Value::Null), failed)
}

fn extract_exit_code(raw_output: Option<&Value>) -> Option<i64> {
    let output = raw_output?;
    output["exitCode"]
        .as_i64()
        .or_else(|| output["exit_code"].as_i64())
}

fn extract_output_text(state: &ToolState) -> String {
    let blocks: Vec<&str> = state
        .content
        .iter()
        .filter(|b| b["type"] == "content" && b["content"]["type"] == "text")
        .filter_map(|b| b["content"]["text"].as_str())
        .collect();
    if !blocks.is_empty() {
        return blocks.join("\n");
    }
    match state.raw_output.as_ref() {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Object(map)) => extract_object_output(map),
        Some(other) => other.to_string(),
    }
}

fn extract_object_output(map: &Map<String, Value>) -> String {
    if map.is_empty() {
        return String::new();
    }
    let streams: Vec<&str> = [
        "stdout",
        "stderr",
        "output",
        "combinedOutput",
        "combined_output",
    ]
    .iter()
    .filter_map(|key| map.get(*key).and_then(Value::as_str))
    .filter(|text| !text.is_empty())
    .collect();
    if !streams.is_empty() {
        streams.join("\n")
    } else {
        Value::Object(map.clone()).to_string()
    }
}

fn format_bash_text(raw: &str, exit_code: Option<i64>, failed: bool) -> String {
    let trimmed = raw.trim_end();
    let status = match (failed, exit_code) {
        (_, Some(code)) if code != 0 => Some(format!("Command exited with code {code}")),
        (true, _) => Some("Command failed".to_owned()),
        _ => None,
    };
    match (status, trimmed.is_empty()) {
        (None, true) => "(no output)".into(),
        (None, false) => trimmed.to_owned(),
        (Some(line), true) => line,
        (Some(line), false) => format!("{trimmed}\n\n{line}"),
    }
}

pub fn plan_summary(kind: &str, update: &Value) -> Option<(String, Value, bool)> {
    match kind {
        "plan" => entries_plan(&update["entries"]),
        "plan_update" => structured_plan(&update["plan"]),
        _ => None,
    }
}

fn entries_plan(entries_val: &Value) -> Option<(String, Value, bool)> {
    let entries = entries_val.as_array()?;
    let lines: Vec<String> = entries
        .iter()
        .map(|entry| {
            let mark = match entry["status"].as_str() {
                Some("completed") => "x",
                Some("in_progress") => "~",
                _ => " ",
            };
            let content = entry["content"].as_str().unwrap_or_default();
            format!("- [{mark}] {content}")
        })
        .collect();
    let all_done = !entries.is_empty() && entries.iter().all(|e| e["status"] == "completed");
    let markdown = lines.join("\n");
    let args = json!({ "plan": markdown, "entries": entries });
    Some((markdown, args, all_done))
}

fn structured_plan(plan: &Value) -> Option<(String, Value, bool)> {
    match plan["type"].as_str()? {
        "items" => entries_plan(&plan["entries"]),
        "markdown" => {
            let content = plan["content"].as_str()?.to_owned();
            let args = json!({ "plan": content });
            Some((content, args, false))
        }
        "file" => {
            let uri = plan["uri"].as_str()?;
            let text = format!("Plan: {uri}");
            let args = json!({ "plan": text, "uri": uri });
            Some((text, args, false))
        }
        _ => None,
    }
}

pub fn map_usage(wire: &AcpUsageWire) -> TokenUsage {
    let cached = wire.cached_read_tokens + wire.cached_write_tokens;
    let sum_parts = wire.input_tokens + cached + wire.output_tokens + wire.thought_tokens;
    let input = if wire.total_tokens > 0 && sum_parts <= wire.total_tokens {
        wire.input_tokens
    } else if wire.total_tokens > 0 && wire.input_tokens + wire.output_tokens == wire.total_tokens {
        wire.input_tokens.saturating_sub(cached)
    } else {
        wire.input_tokens
    };
    TokenUsage {
        input,
        output: wire.output_tokens,
        cache_read: wire.cached_read_tokens,
        cache_write: wire.cached_write_tokens,
        reasoning: wire.thought_tokens,
        ..TokenUsage::default()
    }
}

pub fn map_commands(raw: &[AvailableCommandWire], dialect: &dyn AcpDialect) -> Vec<CommandInfo> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in raw {
        let name = item.name.trim().trim_start_matches('/').to_owned();
        if name.is_empty() || !seen.insert(name.clone()) {
            continue;
        }
        let hint = item
            .input
            .as_ref()
            .map(|i| i.hint.trim().to_owned())
            .filter(|h| !h.is_empty());
        let info = CommandInfo {
            name,
            description: item.description.clone(),
            argument_hint: hint,
        };
        if dialect.filter_command(&info) {
            out.push(info);
        }
    }
    out
}

pub fn model_info_of(provider: &str, id: &str, context_window: u64) -> ModelInfo {
    ModelInfo {
        provider: provider.to_owned(),
        id: id.to_owned(),
        context_window,
        reasoning: false,
        thinking_levels: vec![ThinkingLevel::Off],
    }
}

pub fn extract_models(
    provider: &str,
    setup: &SessionSetupWire,
    requested: Option<&str>,
) -> (ModelInfo, Vec<ModelInfo>) {
    if let Some(state) = &setup.models {
        let list: Vec<ModelInfo> = state
            .available_models
            .iter()
            .map(|m| {
                let win = m
                    .meta
                    .as_ref()
                    .and_then(|meta| meta["totalContextTokens"].as_u64())
                    .unwrap_or(0);
                model_info_of(provider, &m.model_id, win)
            })
            .collect();
        let current_id = if state.current_model_id.is_empty() {
            requested.unwrap_or(provider)
        } else {
            &state.current_model_id
        };
        let current = list
            .iter()
            .find(|m| m.id == current_id)
            .cloned()
            .unwrap_or_else(|| model_info_of(provider, current_id, 0));
        let all = if list.is_empty() {
            vec![current.clone()]
        } else {
            list
        };
        return (current, all);
    }
    let fallback = model_info_of(provider, requested.unwrap_or(provider), 0);
    (fallback.clone(), vec![fallback])
}
