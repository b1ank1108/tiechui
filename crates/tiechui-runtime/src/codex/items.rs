//! Codex items and decisions in the shapes the session protocol and its clients know: the
//! native bash output text, the `write` and `edit` tool arguments the desktop renders, usage per
//! model request, and the permission modes as Codex policy.

use serde_json::{Value, json};
use tiechui_protocol::{ApprovalDecision, TokenUsage};

use super::wire::{
    Breakdown, ChangeKind, CommandAction, CommandItem, FileChange, McpItem, OtherItem,
};
use crate::tool_output;

/// The command a user reads. Codex runs `/bin/bash -lc "<command>"`; when it reports the inner
/// command as the only action, that is what is shown.
pub fn command_text(command: &str, actions: &[CommandAction]) -> String {
    match actions {
        [only] => only
            .command
            .as_deref()
            .filter(|inner| !inner.is_empty())
            .unwrap_or(command),
        _ => command,
    }
    .to_owned()
}

/// A command's result in the native bash tool's text: its output (or `(no output)`), and for a
/// failure the status line the desktop parses the exit code from.
pub fn command_result(item: &CommandItem) -> (Value, bool) {
    let output = item.aggregated_output.as_deref().unwrap_or("").trim_end();
    let status = match (item.status.as_str(), item.exit_code) {
        ("declined", _) => Some("Command declined".to_owned()),
        (_, Some(code)) if code != 0 => Some(format!("Command exited with code {code}")),
        ("failed", _) => Some("Command failed".to_owned()),
        _ => None,
    };
    let text = match (&status, output.is_empty()) {
        (None, true) => "(no output)".to_owned(),
        (None, false) => output.to_owned(),
        (Some(status), true) => status.clone(),
        (Some(status), false) => format!("{output}\n\n{status}"),
    };
    let mut value = tool_output(&text, status.is_some(), Value::Null);
    if let Some(ms) = item.duration_ms {
        value["structured_content"] = json!({
            "output": output,
            "exit_code": item.exit_code,
            "wall_time_seconds": (ms as f64 / 100.0).round() / 10.0,
        });
    }
    (value, status.is_some())
}

pub fn tool_output_text(text: &str, is_error: bool) -> Value {
    tool_output(text, is_error, Value::Null)
}

#[derive(Debug, PartialEq)]
pub struct FileCall {
    pub id: String,
    pub name: &'static str,
    pub arguments: Value,
    pub path: String,
    pub summary: String,
}

/// A file change as one call per file. The first call takes the item's id, which approvals name.
pub fn file_calls(item_id: &str, changes: &[FileChange]) -> Vec<FileCall> {
    changes
        .iter()
        .enumerate()
        .map(|(index, change)| {
            let id = if index == 0 {
                item_id.to_owned()
            } else {
                format!("{item_id}:{index}")
            };
            let path = &change.path;
            let (name, arguments, summary) = match &change.kind {
                ChangeKind::Add => (
                    "write",
                    json!({"path": path, "content": change.diff}),
                    format!("Added {path}"),
                ),
                ChangeKind::Delete => ("delete", json!({"path": path}), format!("Deleted {path}")),
                ChangeKind::Update {
                    move_path: Some(to),
                } => (
                    "move",
                    json!({"path": path, "to": to, "edits": edits_from_diff(&change.diff)}),
                    format!("Moved {path} to {to}"),
                ),
                ChangeKind::Update { move_path: None } => (
                    "edit",
                    json!({"path": path, "edits": edits_from_diff(&change.diff)}),
                    format!("Updated {path}"),
                ),
                ChangeKind::Other => (
                    "patch",
                    json!({"path": path, "diff": change.diff}),
                    format!("Changed {path}"),
                ),
            };
            FileCall {
                id,
                name,
                arguments,
                path: path.clone(),
                summary,
            }
        })
        .collect()
}

#[derive(Default)]
struct DiffEdits {
    edits: Vec<Value>,
    old: String,
    new: String,
    in_hunk: bool,
}

impl DiffEdits {
    fn flush(&mut self) {
        if !self.old.is_empty() || !self.new.is_empty() {
            self.edits.push(json!({
                "oldText": std::mem::take(&mut self.old),
                "newText": std::mem::take(&mut self.new),
            }));
        }
    }

    fn is_file_header(&self, line: &str) -> bool {
        !self.in_hunk && (line.starts_with("--- ") || line.starts_with("+++ "))
    }

    fn push_line(&mut self, line: &str) {
        if line.starts_with("@@") {
            self.flush();
            self.in_hunk = true;
        } else if self.is_file_header(line) {
        } else if let Some(removed) = line.strip_prefix('-') {
            self.old.push_str(removed);
            self.old.push('\n');
        } else if let Some(added) = line.strip_prefix('+') {
            self.new.push_str(added);
            self.new.push('\n');
        } else if !line.starts_with('\\') {
            // Context, or Codex's header lines: they end a run of changes.
            self.flush();
        }
    }
}

pub fn edits_from_diff(diff: &str) -> Vec<Value> {
    let mut parsed = DiffEdits::default();
    for line in diff.lines() {
        parsed.push_line(line);
    }
    parsed.flush();
    let mut edits = parsed.edits;
    if edits.is_empty() && !diff.is_empty() {
        edits.push(json!({"oldText": "", "newText": diff}));
    }
    edits
}

pub fn mcp_name(item: &McpItem) -> String {
    format!("mcp__{}__{}", item.server, item.tool)
}

pub fn mcp_result(item: &McpItem) -> (Value, bool) {
    let is_error = item.status == "failed" || item.error.is_some();
    let text = match (&item.error, &item.result) {
        (Some(error), _) => error.message.clone(),
        (None, Some(result)) => result
            .content
            .iter()
            .map(|part| match part["text"].as_str() {
                Some(text) => text.to_owned(),
                None => part.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        (None, None) => String::new(),
    };
    let mut value = tool_output(&text, is_error, Value::Null);
    if let Some(structured) = item
        .result
        .as_ref()
        .and_then(|r| r.structured_content.clone())
    {
        value["structured_content"] = structured;
    }
    (value, is_error)
}

pub fn other_call(kind: &str, item: &OtherItem) -> Option<(String, Value, String, bool)> {
    match kind {
        "webSearch" => {
            let query = item.query.clone().unwrap_or_default();
            Some((
                "web_search".into(),
                json!({"query": query}),
                format!("Searched the web for: {query}"),
                false,
            ))
        }
        "dynamicToolCall" => {
            let text = item
                .content_items
                .iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            Some((
                item.tool.clone().unwrap_or_else(|| "tool".into()),
                item.arguments.clone().unwrap_or(Value::Null),
                text,
                item.success == Some(false),
            ))
        }
        "imageView" => {
            let path = item.path.clone().unwrap_or_default();
            Some((
                "view_image".into(),
                json!({"path": path}),
                format!("Viewed {path}"),
                false,
            ))
        }
        _ => None,
    }
}

/// One model request's usage. Codex counts cached tokens inside the input; the session counts
/// them apart.
pub fn usage(last: &Breakdown) -> TokenUsage {
    let cached = last.cached_input_tokens;
    let written = last.cache_write_input_tokens;
    TokenUsage {
        input: last.input_tokens.saturating_sub(cached + written),
        output: last.output_tokens,
        cache_read: cached,
        cache_write: written,
        reasoning: last.reasoning_output_tokens,
        ..TokenUsage::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub approval: &'static str,
    pub reviewer: &'static str,
    pub sandbox: &'static str,
    pub sandbox_type: &'static str,
}

impl Policy {
    /// `rules` asks about everything and the sandbox is read-only; `auto` lets Codex's reviewer
    /// approve what stays in the workspace; `yolo` asks nothing. No or an unknown mode is `rules`.
    pub fn of(mode: Option<&str>) -> Self {
        match mode {
            Some("auto") => Self {
                approval: "on-request",
                reviewer: "auto_review",
                sandbox: "workspace-write",
                sandbox_type: "workspaceWrite",
            },
            Some("yolo") => Self {
                approval: "never",
                reviewer: "user",
                sandbox: "danger-full-access",
                sandbox_type: "dangerFullAccess",
            },
            _ => Self {
                approval: "untrusted",
                reviewer: "user",
                sandbox: "read-only",
                sandbox_type: "readOnly",
            },
        }
    }

    pub fn sandbox_policy(&self) -> Value {
        json!({"type": self.sandbox_type})
    }
}

pub fn review_decision(decision: &ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::AllowOnce | ApprovalDecision::Accept { .. } => "accept",
        ApprovalDecision::AllowSession { .. } => "acceptForSession",
        ApprovalDecision::Deny { .. } | ApprovalDecision::Decline => "decline",
        ApprovalDecision::Cancel => "cancel",
    }
}

pub fn permissions_response(requested: &Value, decision: &ApprovalDecision) -> Value {
    match decision {
        ApprovalDecision::AllowOnce | ApprovalDecision::Accept { .. } => {
            json!({"permissions": requested, "scope": "turn"})
        }
        ApprovalDecision::AllowSession { .. } => {
            json!({"permissions": requested, "scope": "session"})
        }
        _ => json!({"permissions": {}, "scope": "turn"}),
    }
}

pub fn elicitation_response(decision: &ApprovalDecision) -> Value {
    match decision {
        ApprovalDecision::Accept { content } => {
            json!({"action": "accept", "content": content.clone().unwrap_or(Value::Null)})
        }
        ApprovalDecision::AllowOnce | ApprovalDecision::AllowSession { .. } => {
            json!({"action": "accept"})
        }
        ApprovalDecision::Deny { .. } | ApprovalDecision::Decline => json!({"action": "decline"}),
        ApprovalDecision::Cancel => json!({"action": "cancel"}),
    }
}

pub fn requested_paths(permissions: &Value) -> Vec<String> {
    let files = &permissions["fileSystem"];
    let mut paths: Vec<String> = ["read", "write"]
        .iter()
        .flat_map(|key| files[*key].as_array().into_iter().flatten())
        .filter_map(|path| path.as_str().map(str::to_owned))
        .collect();
    if permissions["network"]["enabled"] == true {
        paths.push("network".into());
    }
    paths
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn change(path: &str, kind: Value, diff: &str) -> FileChange {
        serde_json::from_value(json!({"path": path, "kind": kind, "diff": diff})).unwrap()
    }

    #[test]
    fn the_inner_command_is_shown_when_it_is_the_only_action() {
        let one = vec![CommandAction {
            command: Some("ls -la".into()),
        }];
        assert_eq!(command_text("/bin/bash -lc \"ls -la\"", &one), "ls -la");
        assert_eq!(command_text("a && b", &[]), "a && b");
        let two = vec![
            CommandAction { command: None },
            CommandAction { command: None },
        ];
        assert_eq!(command_text("a && b", &two), "a && b");
    }

    #[test]
    fn a_diff_becomes_one_edit_per_run_of_changed_lines() {
        let diff = "--- a/x\n+++ b/x\n@@ -1,6 +1,6 @@\n keep\n-one\n-two\n+uno\n mid\n-three\n+tres\n\\ No newline at end of file\n";
        assert_eq!(
            edits_from_diff(diff),
            vec![
                json!({"oldText": "one\ntwo\n", "newText": "uno\n"}),
                json!({"oldText": "three\n", "newText": "tres\n"}),
            ]
        );
        assert_eq!(
            edits_from_diff("plain"),
            vec![json!({"oldText": "", "newText": "plain"})]
        );
        assert!(edits_from_diff("").is_empty());
    }

    #[test]
    fn file_changes_become_the_calls_the_desktop_renders() {
        let calls = file_calls(
            "fc",
            &[
                change("/a", json!({"type": "add"}), "hi\n"),
                change(
                    "/b",
                    json!({"type": "update", "move_path": null}),
                    "@@ -1 +1 @@\n-x\n+y\n",
                ),
                change("/c", json!({"type": "update", "move_path": "/d"}), ""),
                change("/e", json!({"type": "delete"}), ""),
                change("/f", json!({"type": "somethingNew"}), "?"),
            ],
        );
        let shape: Vec<_> = calls.iter().map(|c| (c.id.as_str(), c.name)).collect();
        assert_eq!(
            shape,
            [
                ("fc", "write"),
                ("fc:1", "edit"),
                ("fc:2", "move"),
                ("fc:3", "delete"),
                ("fc:4", "patch")
            ]
        );
        assert_eq!(calls[0].arguments, json!({"path": "/a", "content": "hi\n"}));
        assert_eq!(
            calls[1].arguments,
            json!({"path": "/b", "edits": [{"oldText": "x\n", "newText": "y\n"}]})
        );
    }

    fn command(status: &str, code: Option<i64>, output: Option<&str>) -> CommandItem {
        serde_json::from_value(json!({"id": "c", "command": "x", "status": status,
            "exitCode": code, "aggregatedOutput": output, "durationMs": 1250}))
        .unwrap()
    }

    #[test]
    fn a_command_result_has_the_native_text_shape() {
        let (ok, failed) = command_result(&command("completed", Some(0), Some("a\nb\n")));
        assert!(!failed);
        assert_eq!(ok["content"][0]["text"], "a\nb");
        assert_eq!(ok["structured_content"]["wall_time_seconds"], 1.3);
        let (quiet, _) = command_result(&command("completed", Some(0), Some("")));
        assert_eq!(quiet["content"][0]["text"], "(no output)");
        let (bad, failed) = command_result(&command("failed", Some(2), Some("boom\n")));
        assert!(failed && bad["is_error"] == true);
        assert_eq!(
            bad["content"][0]["text"],
            "boom\n\nCommand exited with code 2"
        );
        let (declined, failed) = command_result(&command("declined", None, None));
        assert!(failed);
        assert_eq!(declined["content"][0]["text"], "Command declined");
    }

    #[test]
    fn usage_counts_cached_input_apart() {
        let breakdown = Breakdown {
            input_tokens: 1000,
            cached_input_tokens: 400,
            cache_write_input_tokens: 100,
            output_tokens: 50,
            reasoning_output_tokens: 20,
            total_tokens: 1050,
        };
        let usage = usage(&breakdown);
        assert_eq!(
            (
                usage.input,
                usage.cache_read,
                usage.cache_write,
                usage.output,
                usage.reasoning
            ),
            (500, 400, 100, 50, 20)
        );
    }

    #[test]
    fn modes_map_to_policy_and_a_missing_or_unknown_mode_asks() {
        assert_eq!(Policy::of(None), Policy::of(Some("rules")));
        assert_eq!(Policy::of(Some("bogus")), Policy::of(Some("rules")));
        assert_eq!(Policy::of(Some("rules")).approval, "untrusted");
        assert_eq!(Policy::of(Some("auto")).reviewer, "auto_review");
        assert_eq!(
            Policy::of(Some("yolo")).sandbox_policy(),
            json!({"type": "dangerFullAccess"})
        );
    }

    #[test]
    fn decisions_map_onto_codex_decisions() {
        let deny = ApprovalDecision::Deny { reason: None };
        assert_eq!(review_decision(&ApprovalDecision::AllowOnce), "accept");
        assert_eq!(
            review_decision(&ApprovalDecision::AllowSession {
                both_directions: false
            }),
            "acceptForSession"
        );
        assert_eq!(review_decision(&deny), "decline");
        assert_eq!(review_decision(&ApprovalDecision::Cancel), "cancel");
        let requested = json!({"network": {"enabled": true}});
        assert_eq!(
            permissions_response(&requested, &ApprovalDecision::AllowOnce)["scope"],
            "turn"
        );
        assert_eq!(
            permissions_response(&requested, &deny)["permissions"],
            json!({})
        );
        assert_eq!(elicitation_response(&deny), json!({"action": "decline"}));
        assert_eq!(
            elicitation_response(&ApprovalDecision::Accept {
                content: Some(json!({"a": 1}))
            }),
            json!({"action": "accept", "content": {"a": 1}})
        );
        assert_eq!(requested_paths(&requested), ["network"]);
    }
}
