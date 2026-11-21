//! Claude items and settings in the shapes the session protocol and its clients know: the native
//! `bash`, `read`, `write` and `edit` tool arguments the desktop renders, the native bash output
//! text, usage per model request, and the launch flags for permission modes and thinking levels.

use std::hash::{BuildHasher, Hasher};

use serde_json::{Value, json};
use tiechui_protocol::{ThinkingLevel, TokenUsage};

use super::wire::Usage;
use crate::tool_output;

/// A tool call as the native tool it corresponds to, so every client renders it alike. Tools
/// without a native counterpart keep Claude's name and input.
pub fn tool_call(name: &str, input: &Value) -> (String, Value) {
    let field = |key: &str| input.get(key).cloned().unwrap_or(Value::Null);
    let present = |keys: &[(&str, &str)]| {
        let fields: serde_json::Map<String, Value> = keys
            .iter()
            .filter(|(from, _)| input.get(from).is_some_and(|value| !value.is_null()))
            .map(|(from, to)| ((*to).to_owned(), field(from)))
            .collect();
        Value::Object(fields)
    };
    match name {
        "Bash" => ("bash".into(), present(&[("command", "command")])),
        "Read" => (
            "read".into(),
            present(&[
                ("file_path", "path"),
                ("offset", "offset"),
                ("limit", "limit"),
            ]),
        ),
        "Write" => (
            "write".into(),
            present(&[("file_path", "path"), ("content", "content")]),
        ),
        "Edit" => (
            "edit".into(),
            json!({"path": field("file_path"), "edits": [edit(input)]}),
        ),
        "MultiEdit" => {
            let edits: Vec<Value> = input
                .get("edits")
                .and_then(Value::as_array)
                .map(|edits| edits.iter().map(edit).collect())
                .unwrap_or_default();
            (
                "edit".into(),
                json!({"path": field("file_path"), "edits": edits}),
            )
        }
        "Glob" => (
            "find".into(),
            present(&[("pattern", "pattern"), ("path", "path")]),
        ),
        "Grep" => ("grep".into(), input.clone()),
        _ => (name.to_owned(), input.clone()),
    }
}

fn edit(input: &Value) -> Value {
    json!({
        "oldText": input.get("old_string").cloned().unwrap_or(Value::Null),
        "newText": input.get("new_string").cloned().unwrap_or(Value::Null),
    })
}

/// The text of a `tool_result` block's content: a string, or text blocks joined.
pub fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A tool's result in the native output shape. For `bash` the text is the native one: the output
/// (or `(no output)`), and for a failure the status line the desktop parses the exit code from.
pub fn tool_result(call_name: &str, content: &Value, is_error: bool) -> Value {
    let text = result_text(content);
    if call_name != "bash" {
        return tool_output(&text, is_error, Value::Null);
    }
    let text = match text.strip_prefix("Exit code ") {
        Some(rest) if is_error => {
            let (code, output) = rest.split_once('\n').unwrap_or((rest, ""));
            let output = output.trim_end();
            match (code.trim().parse::<i64>(), output.is_empty()) {
                (Ok(code), true) => format!("Command exited with code {code}"),
                (Ok(code), false) => format!("{output}\n\nCommand exited with code {code}"),
                (Err(_), _) => text.clone(),
            }
        }
        _ if text.trim().is_empty() || text == "(Bash completed with no output)" => {
            "(no output)".to_owned()
        }
        _ => text,
    };
    tool_output(&text, is_error, Value::Null)
}

/// The usage of one model request. Claude's input excludes cache reads and writes already.
pub fn usage(usage: &Usage) -> TokenUsage {
    TokenUsage {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_input_tokens,
        cache_write: usage.cache_creation_input_tokens,
        reasoning: usage.output_tokens_details.thinking_tokens,
        cache_write_1h: usage.cache_creation.ephemeral_1h_input_tokens,
        cost_nano_usd: None,
    }
}

/// `--permission-mode` for a session mode: `rules` (also the default and any unknown value) asks
/// before changes, `auto` lets Claude's classifier decide, `yolo` allows everything.
pub fn permission_mode(mode: Option<&str>) -> &'static str {
    match mode {
        Some("auto") => "auto",
        Some("yolo") => "bypassPermissions",
        _ => "manual",
    }
}

/// The launch flag for a thinking level: `off` turns thinking off, the others are `--effort`
/// levels (`minimal` is Claude's lowest).
pub fn thinking_flag(level: ThinkingLevel) -> (&'static str, &'static str) {
    match level {
        ThinkingLevel::Off => ("--thinking", "disabled"),
        ThinkingLevel::Minimal | ThinkingLevel::Low => ("--effort", "low"),
        ThinkingLevel::Medium => ("--effort", "medium"),
        ThinkingLevel::High => ("--effort", "high"),
        ThinkingLevel::Xhigh => ("--effort", "xhigh"),
        ThinkingLevel::Max => ("--effort", "max"),
    }
}

/// The variables of the host's own Claude Code session (and of an Electron host) that must not
/// leak into the Claude the driver starts: it would attach to the host's session or run as Node.
pub fn is_host_variable(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    name.starts_with("CLAUDECODE")
        || name.starts_with("CLAUDE_CODE_")
        || name == "ELECTRON_RUN_AS_NODE"
}

/// A random (version 4) UUID, the form `--session-id` takes. The randomness is the standard
/// library's per-process hasher seed, which comes from the operating system.
pub fn new_session_id() -> String {
    let random = std::collections::hash_map::RandomState::new();
    let mut bytes = [0u8; 16];
    for (index, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = random.build_hasher();
        hasher.write_usize(index);
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_distinct_version_4_uuids() {
        let (a, b) = (new_session_id(), new_session_id());
        assert_ne!(a, b);
        for id in [a, b] {
            assert_eq!(id.len(), 36);
            assert_eq!(id.as_bytes()[14], b'4', "{id}");
            assert!(
                matches!(id.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
                "{id}"
            );
            assert!(id.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn host_variables_are_the_claude_code_ones_and_electron_run_as_node() {
        for name in [
            "CLAUDECODE",
            "CLAUDECODE_X",
            "CLAUDE_CODE_ENTRYPOINT",
            "claude_code_sse_port",
            "ELECTRON_RUN_AS_NODE",
        ] {
            assert!(is_host_variable(name), "{name}");
        }
        for name in [
            "CLAUDE_CONFIG_DIR",
            "ANTHROPIC_API_KEY",
            "HOME",
            "CLAUDE_EFFORT",
            "ELECTRON_OTHER",
        ] {
            assert!(!is_host_variable(name), "{name}");
        }
    }

    #[test]
    fn a_failing_bash_result_ends_with_the_status_line_the_desktop_parses() {
        let output = tool_result("bash", &json!("Exit code 2\nls: x: No such file\n"), true);
        assert_eq!(
            output["content"][0]["text"],
            "ls: x: No such file\n\nCommand exited with code 2"
        );
        let empty = tool_result("bash", &json!("(Bash completed with no output)"), false);
        assert_eq!(empty["content"][0]["text"], "(no output)");
    }

    #[test]
    fn multi_edit_becomes_one_edit_call_with_every_edit() {
        let (name, args) = tool_call(
            "MultiEdit",
            &json!({"file_path": "/a", "edits": [
                {"old_string": "1", "new_string": "2"}, {"old_string": "3", "new_string": "4"}]}),
        );
        assert_eq!(name, "edit");
        assert_eq!(
            args,
            json!({"path": "/a", "edits": [
                {"oldText": "1", "newText": "2"}, {"oldText": "3", "newText": "4"}]})
        );
    }
}
