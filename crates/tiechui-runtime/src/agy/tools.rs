use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use tiechui_protocol::SubagentState;

use crate::acp::{AcpSubagent, ToolState};

const SUBAGENT_NAMES: &[&str] = &["invoke_subagent", "start_subagent"];
const SUBAGENT_TITLES: &[&str] = &[
    "Run invoke_subagent",
    "Running invoke_subagent",
    "Run invoke_subagent?",
    "Run start_subagent",
    "Running start_subagent",
    "Run start_subagent?",
];

#[derive(Debug, Clone)]
pub struct PathRoot {
    raw: PathBuf,
    canonical: Option<PathBuf>,
}

impl PathRoot {
    pub fn new(cwd: &Path) -> Self {
        Self {
            raw: cwd.to_path_buf(),
            canonical: std::fs::canonicalize(cwd).ok(),
        }
    }

    pub fn relativize(&self, input: &str) -> String {
        let cleaned = strip_file_uri(input.trim());
        let candidate = Path::new(&cleaned);
        if let Some(rel) = self.strip_base(candidate, &self.raw) {
            return rel;
        }
        if let Some(canon) = &self.canonical
            && let Some(rel) = self.strip_base(candidate, canon)
        {
            return rel;
        }
        cleaned
    }

    fn strip_base(&self, candidate: &Path, base: &Path) -> Option<String> {
        let rel = candidate.strip_prefix(base).ok()?;
        let rendered = rel.to_str()?.replace('\\', "/");
        if rendered.is_empty() {
            Some(".".into())
        } else {
            Some(rendered)
        }
    }
}

fn strip_file_uri(raw: &str) -> String {
    let Some(rest) = raw.strip_prefix("file://") else {
        return raw.to_owned();
    };
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if rest.len() >= 3
        && rest.starts_with('/')
        && rest.as_bytes()[2] == b':'
        && rest.as_bytes()[1].is_ascii_alphabetic()
    {
        return rest[1..].to_owned();
    }
    rest.to_owned()
}

pub fn normalize_tool(state: &mut ToolState, root: Option<&PathRoot>) {
    normalize_locations_and_diffs(state, root);
    if is_subagent_tool(state) {
        normalize_subagent_input(state);
        return;
    }
    match state.name.as_deref() {
        Some("read_file" | "client_view_file") => normalize_read(state, root),
        Some("list_dir") => normalize_list_dir(state, root),
        Some("write_file" | "write_to_file") => normalize_write(state, root),
        Some("edit_file" | "replace_file_content" | "multi_replace_file_content") => {
            normalize_edit(state, root);
        }
        Some("grep_search") => normalize_search(state, "grep", root),
        Some("find_by_name") => normalize_search(state, "find", root),
        Some("run_command") => normalize_command(state),
        _ => {}
    }
}

fn normalize_locations_and_diffs(state: &mut ToolState, root: Option<&PathRoot>) {
    let Some(root) = root else {
        return;
    };
    for loc in &mut state.locations {
        loc.path = root.relativize(&loc.path);
    }
    for block in &mut state.content {
        if block["type"].as_str() == Some("diff")
            && let Some(path) = block["path"].as_str().map(|p| root.relativize(p))
        {
            block["path"] = json!(path);
        }
    }
}

pub fn is_subagent_tool(state: &ToolState) -> bool {
    if state
        .meta
        .as_ref()
        .is_some_and(|m| m["is_mcp_tool_call"] == true)
    {
        return false;
    }
    let by_name = state
        .name
        .as_deref()
        .is_some_and(|n| SUBAGENT_NAMES.contains(&n));
    let by_title = state
        .title
        .as_deref()
        .is_some_and(|t| SUBAGENT_TITLES.contains(&t.trim()));
    by_name || by_title
}

fn normalize_subagent_input(state: &mut ToolState) {
    state.name = Some("invoke_subagent".into());
    state.kind = Some("other".into());
    let mut map = input_map(state);
    if let Some(prompt) = extract_subagent_prompt(&map) {
        map.entry("prompt").or_insert_with(|| json!(prompt));
    }
    state.raw_input = Some(Value::Object(map));
}

fn extract_subagent_prompt(map: &Map<String, Value>) -> Option<String> {
    first_str(map, &["Prompt", "prompt", "description", "task"]).or_else(|| {
        map.get("Subagents")?
            .as_array()?
            .first()?
            .get("Prompt")?
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
    })
}

fn normalize_read(state: &mut ToolState, root: Option<&PathRoot>) {
    state.kind = Some("read".into());
    let mut map = input_map(state);
    set_normalized_path(
        &mut map,
        &["Path", "AbsolutePath", "path"],
        &state.locations,
        root,
    );
    if let Some(start) = map.get("StartLine").and_then(Value::as_u64) {
        map.entry("offset").or_insert_with(|| json!(start));
        if let Some(end) = map.get("EndLine").and_then(Value::as_u64)
            && end >= start
        {
            map.entry("limit").or_insert_with(|| json!(end - start + 1));
        }
    }
    state.raw_input = Some(Value::Object(map));
}

fn normalize_list_dir(state: &mut ToolState, root: Option<&PathRoot>) {
    state.name = Some("ls".into());
    state.kind = Some("read".into());
    let mut map = input_map(state);
    set_normalized_path(
        &mut map,
        &["DirectoryPath", "Path", "path"],
        &state.locations,
        root,
    );
    state.raw_input = Some(Value::Object(map));
}

fn normalize_write(state: &mut ToolState, root: Option<&PathRoot>) {
    let mut map = input_map(state);
    let append = map.get("Append").and_then(Value::as_bool).unwrap_or(false);
    state.kind = Some(if append { "edit" } else { "write" }.into());
    set_normalized_path(
        &mut map,
        &["Path", "TargetFile", "path"],
        &state.locations,
        root,
    );
    if let Some(content) = first_str(&map, &["Content", "CodeContent", "content"]) {
        map.insert("content".into(), json!(content));
    }
    state.raw_input = Some(Value::Object(map));
}

fn normalize_edit(state: &mut ToolState, root: Option<&PathRoot>) {
    state.kind = Some("edit".into());
    let mut map = input_map(state);
    set_normalized_path(
        &mut map,
        &["Path", "TargetFile", "path"],
        &state.locations,
        root,
    );
    if !map.contains_key("edits") {
        let edits = extract_input_edits(&map);
        if !edits.is_empty() {
            map.insert("edits".into(), Value::Array(edits));
        }
    }
    state.raw_input = Some(Value::Object(map));
}

fn extract_input_edits(map: &Map<String, Value>) -> Vec<Value> {
    if let Some(chunks) = map.get("ReplacementChunks").and_then(Value::as_array) {
        return chunks
            .iter()
            .map(|c| {
                json!({
                    "oldText": c["TargetContent"].as_str().unwrap_or_default(),
                    "newText": c["ReplacementContent"].as_str().unwrap_or_default(),
                })
            })
            .collect();
    }
    let old = first_str(map, &["Target", "TargetContent"]);
    let new = first_str(map, &["Replacement", "ReplacementContent"]);
    match (old, new) {
        (None, None) => Vec::new(),
        (o, n) => vec![json!({
            "oldText": o.unwrap_or_default(),
            "newText": n.unwrap_or_default(),
        })],
    }
}

fn normalize_search(state: &mut ToolState, tool_name: &str, root: Option<&PathRoot>) {
    state.name = Some(tool_name.into());
    state.kind = Some("search".into());
    let mut map = input_map(state);
    if let Some(pattern) = first_str(&map, &["Query", "Pattern", "pattern", "query"]) {
        map.insert("pattern".into(), json!(pattern));
    }
    set_normalized_path(
        &mut map,
        &["SearchPath", "SearchDirectory", "path"],
        &state.locations,
        root,
    );
    state.raw_input = Some(Value::Object(map));
}

fn normalize_command(state: &mut ToolState) {
    state.kind = Some("execute".into());
    let mut map = input_map(state);
    if let Some(cmd) = first_str(
        &map,
        &["CommandLine", "command_line", "commandLine", "command"],
    ) {
        map.insert("command".into(), json!(cmd));
    }
    state.raw_input = Some(Value::Object(map));
    unwrap_command_envelope(state);
}

fn unwrap_command_envelope(state: &mut ToolState) {
    let mut parsed_code = None;
    for block in &mut state.content {
        if let Some(raw) = block["content"]["text"].as_str()
            && let Some((clean, code)) = strip_agy_command_wrapper(raw)
        {
            block["content"]["text"] = json!(clean);
            parsed_code = parsed_code.or(code);
        }
    }
    if let Some(Value::String(raw)) = state.raw_output.clone()
        && let Some((clean, code)) = strip_agy_command_wrapper(&raw)
    {
        parsed_code = parsed_code.or(code);
        state.raw_output = Some(match parsed_code {
            Some(exit) => json!({ "output": clean, "exitCode": exit }),
            None => Value::String(clean),
        });
    }
}

fn strip_agy_command_wrapper(text: &str) -> Option<(String, Option<i64>)> {
    if !text.starts_with("Created At: ") {
        return None;
    }
    let exit_code = text.lines().find_map(parse_exit_line);
    if let Some((_, body)) = text.split_once("Command output:\n") {
        let cleaned = body.trim_end_matches(['\r', '\n']).to_owned();
        return Some((cleaned, exit_code));
    }
    if text.contains("No output from the command.") {
        return Some((String::new(), exit_code));
    }
    None
}

fn parse_exit_line(line: &str) -> Option<i64> {
    let rest = line.trim().strip_prefix("Command exited with code ")?;
    rest.trim_end_matches('.').parse().ok()
}

pub fn tool_subagent(state: &ToolState) -> Option<AcpSubagent> {
    if !is_subagent_tool(state) {
        return None;
    }
    let sub_state = match state.status.as_deref() {
        Some("completed") => SubagentState::Completed,
        Some("failed") => SubagentState::Failed,
        _ => SubagentState::Running,
    };
    let map = input_map(state);
    let name = subagent_name(&map);
    let message = state
        .is_terminal()
        .then(|| extract_subagent_result(state))
        .flatten();
    Some(AcpSubagent {
        agent_id: state.id.clone(),
        name: Some(name),
        state: sub_state,
        message,
    })
}

fn subagent_name(map: &Map<String, Value>) -> String {
    if let Some(label) = first_str(map, &["Role", "TypeName", "subagent_type", "name"]) {
        return label;
    }
    if let Some(items) = map.get("Subagents").and_then(Value::as_array)
        && !items.is_empty()
    {
        return format!("{} subagents", items.len());
    }
    "subagent".into()
}

fn extract_subagent_result(state: &ToolState) -> Option<String> {
    let parts: Vec<&str> = state
        .content
        .iter()
        .filter_map(|b| b["content"]["text"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if !parts.is_empty() {
        return Some(parts.join("\n"));
    }
    state
        .raw_output
        .as_ref()
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn input_map(state: &ToolState) -> Map<String, Value> {
    match state.raw_input.as_ref() {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

fn first_str(map: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        map.get(*key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}

fn set_normalized_path(
    map: &mut Map<String, Value>,
    keys: &[&str],
    locations: &[crate::acp::ToolLocation],
    root: Option<&PathRoot>,
) {
    let found = first_str(map, keys).or_else(|| locations.first().map(|l| l.path.clone()));
    if let Some(raw_path) = found {
        let normalized = match root {
            Some(r) => r.relativize(&raw_path),
            None => raw_path,
        };
        map.insert("path".into(), json!(normalized));
    }
}
