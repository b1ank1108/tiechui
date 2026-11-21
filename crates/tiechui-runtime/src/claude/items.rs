//! Claude items and settings in the shapes the session protocol and its clients know: the native
//! `bash`, `read`, `write` and `edit` tool arguments the desktop renders, the native bash output
//! text, usage per model request, permission requests as approvals and the answers to them, and
//! the launch flags for permission modes and thinking levels.

use std::hash::{BuildHasher, Hasher};

use serde_json::{Value, json};
use tiechui_protocol::{
    ApprovalDecision, ApprovalRequest, Question, QuestionOption, ThinkingLevel, TokenUsage,
};

use super::wire::{Request, Usage};
use crate::tool_output;

const DENIED: &str = "The user denied this tool call.";
const UNANSWERED: &str = "The user did not answer the questions.";

/// A `can_use_tool` request Claude waits on, kept until it is answered or withdrawn.
pub struct Pending {
    pub request_id: String,
    /// The sub-agent that asked; `None` for the root conversation.
    pub agent: Option<String>,
    input: Value,
    asked: Asked,
}

enum Asked {
    /// A tool call. Allowing it for the session applies these permission updates.
    Tool { grants: Vec<Value> },
    /// `AskUserQuestion`: each question's text under the id the client answers it by.
    Questions(Vec<(String, String)>),
}

impl Pending {
    /// The `can_use_tool` response for the user's decision. Every decision that does not allow
    /// the call is a deny, so Claude carries on with the reason (or a default) as the result.
    pub fn response(&self, decision: &ApprovalDecision) -> Value {
        let allow = |input: Value| json!({"behavior": "allow", "updatedInput": input});
        let deny = |message: &str| json!({"behavior": "deny", "message": message});
        match (&self.asked, decision) {
            (
                Asked::Questions(questions),
                ApprovalDecision::Accept {
                    content: Some(content),
                },
            ) => match answered(&self.input, questions, content) {
                Some(input) => allow(input),
                None => deny(UNANSWERED),
            },
            (Asked::Tool { .. }, ApprovalDecision::AllowOnce | ApprovalDecision::Accept { .. }) => {
                allow(self.input.clone())
            }
            (Asked::Tool { grants }, ApprovalDecision::AllowSession { .. }) => {
                let mut response = allow(self.input.clone());
                if !grants.is_empty() {
                    response["updatedPermissions"] = json!(grants);
                }
                response
            }
            (
                _,
                ApprovalDecision::Deny {
                    reason: Some(reason),
                },
            ) if !reason.trim().is_empty() => deny(reason),
            (Asked::Questions(_), _) => deny(UNANSWERED),
            (Asked::Tool { .. }, _) => deny(DENIED),
        }
    }
}

/// What the user is asked for a `can_use_tool` request about the call `call_id`, and what is
/// kept to answer it. `shown` is the call's input as the card shows it, the request's own input
/// unless that is empty. `AskUserQuestion` becomes question cards; every other tool,
/// `ExitPlanMode` included, a permission card whose text is the command, the file or the full
/// plan. A sub-agent's request names the sub-agent as its actor. The answer always carries the
/// request's own input back.
pub fn ask(
    request_id: &str,
    request: &Request,
    shown: &Value,
    call_id: &str,
) -> (Pending, ApprovalRequest) {
    let tool = request.tool_name.as_deref().unwrap_or_default();
    let (asked, approval) = if tool == "AskUserQuestion" {
        let (ids, questions) = questions(shown);
        (
            Asked::Questions(ids),
            ApprovalRequest::Questions { questions },
        )
    } else {
        let (grants, session_patterns) = session_grants(&request.permission_suggestions);
        let (name, arguments) = tool_call(tool, shown);
        let (surface, value) = subject(&name, &arguments, request.description.as_deref());
        let approval = ApprovalRequest::Permission {
            actor: request.agent_id.as_deref().map(Box::from),
            call_id: call_id.to_owned(),
            tool: name,
            arguments,
            surface: surface.to_owned(),
            values: vec![value],
            matched_pattern: None,
            origin: "claude".into(),
            session_patterns,
            preview: None,
        };
        (Asked::Tool { grants }, approval)
    };
    let pending = Pending {
        request_id: request_id.to_owned(),
        agent: request.agent_id.clone(),
        input: request.input.clone(),
        asked,
    };
    (pending, approval)
}

/// What a permission is about, as the card shows it: the command, the file, the plan, or failing
/// those whatever describes the call.
fn subject(tool: &str, arguments: &Value, description: Option<&str>) -> (&'static str, String) {
    let text = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    let (surface, value) = match tool {
        "bash" => ("command", text("command")),
        "ExitPlanMode" => ("plan", text("plan")),
        _ => match text("path").or_else(|| text("notebook_path")) {
            Some(path) => ("path", Some(path)),
            None => (
                "tool",
                ["url", "pattern", "query"]
                    .into_iter()
                    .find_map(text)
                    .or_else(|| description.filter(|d| !d.is_empty()).map(str::to_owned)),
            ),
        },
    };
    (surface, value.unwrap_or_else(|| tool.to_owned()))
}

/// `AskUserQuestion`'s questions as question cards, with ids `q1`, `q2`, … in order.
fn questions(input: &Value) -> (Vec<(String, String)>, Vec<Question>) {
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let list = |value: &Value, key: &str| -> Vec<Value> {
        value
            .get(key)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    list(input, "questions")
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let id = format!("q{}", index + 1);
            let options: Vec<QuestionOption> = list(question, "options")
                .iter()
                .map(|option| QuestionOption {
                    label: text(option, "label"),
                    description: text(option, "description"),
                })
                .collect();
            let card = Question {
                id: id.clone(),
                header: text(question, "header"),
                question: text(question, "question"),
                options: (!options.is_empty()).then_some(options),
            };
            ((id, card.question.clone()), card)
        })
        .unzip()
}

/// The input with the user's answers added as `AskUserQuestion` reads them: each question's text
/// maps to the chosen labels, joined with ", " when several were picked. `None` when nothing
/// was answered.
fn answered(input: &Value, questions: &[(String, String)], content: &Value) -> Option<Value> {
    let answers: serde_json::Map<String, Value> = questions
        .iter()
        .filter_map(|(id, question)| {
            let picked: Vec<&str> = content["answers"][id]["answers"]
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .collect();
            (!picked.is_empty()).then(|| (question.clone(), json!(picked.join(", "))))
        })
        .collect();
    if answers.is_empty() {
        return None;
    }
    let mut input = input.clone();
    input
        .as_object_mut()?
        .insert("answers".into(), Value::Object(answers));
    Some(input)
}

/// What allowing for the session grants, and how it reads on the card (`Bash(touch a.txt)`,
/// `acceptEdits`, a directory). Claude offers alternatives: for a command that writes a file, a
/// rule for that command, the directory and `acceptEdits` for the whole session. The narrowest
/// is granted: the allow rules when there are any, else the rest. Each is scoped to this session,
/// since a suggestion may name a settings file that would outlive it.
fn session_grants(suggestions: &Value) -> (Vec<Value>, Vec<String>) {
    let (mut rules, mut others) = (Vec::new(), Vec::new());
    for suggestion in suggestions.as_array().into_iter().flatten() {
        let (granted, narrow): (Vec<String>, bool) = match suggestion["type"].as_str() {
            Some("addRules") if suggestion["behavior"] == "allow" => (
                suggestion["rules"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(rule_text)
                    .collect(),
                true,
            ),
            Some("setMode") => (
                suggestion["mode"]
                    .as_str()
                    .map(str::to_owned)
                    .into_iter()
                    .collect(),
                false,
            ),
            Some("addDirectories") => (
                suggestion["directories"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect(),
                false,
            ),
            _ => continue,
        };
        if granted.is_empty() {
            continue;
        }
        let mut grant = suggestion.clone();
        grant["destination"] = json!("session");
        let into = if narrow { &mut rules } else { &mut others };
        into.push((grant, granted));
    }
    let chosen = if rules.is_empty() { others } else { rules };
    let (grants, patterns): (Vec<Value>, Vec<Vec<String>>) = chosen.into_iter().unzip();
    (grants, patterns.concat())
}

/// A permission rule in Claude's own syntax: `Bash(touch a.txt)`, or the bare tool name.
fn rule_text(rule: &Value) -> Option<String> {
    let tool = rule.get("toolName")?.as_str()?;
    Some(match rule.get("ruleContent").and_then(Value::as_str) {
        Some(content) if !content.is_empty() => format!("{tool}({content})"),
        _ => tool.to_owned(),
    })
}

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

    fn request(tool: &str, input: Value, suggestions: Value) -> Request {
        Request {
            subtype: "can_use_tool".into(),
            tool_name: Some(tool.into()),
            input,
            tool_use_id: Some("toolu_1".into()),
            description: None,
            permission_suggestions: suggestions,
            agent_id: None,
        }
    }

    #[test]
    fn allowing_for_the_session_grants_the_narrowest_suggestion_for_this_session_only() {
        let rule = json!({"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "touch a"}],
            "behavior": "allow", "destination": "localSettings"});
        let directory =
            json!({"type": "addDirectories", "directories": ["/w"], "destination": "session"});
        let mode = json!({"type": "setMode", "mode": "acceptEdits", "destination": "session"});
        let deny_rule = json!({"type": "addRules", "rules": [{"toolName": "Bash"}],
            "behavior": "deny", "destination": "session"});

        let (grants, patterns) = session_grants(&json!([rule, directory, mode, deny_rule]));
        let mut scoped = rule.clone();
        scoped["destination"] = json!("session");
        assert_eq!(
            (grants, patterns),
            (vec![scoped], vec!["Bash(touch a)".to_owned()])
        );

        let (grants, patterns) = session_grants(&json!([directory, mode]));
        assert_eq!(grants, [directory.clone(), mode.clone()]);
        assert_eq!(patterns, ["/w", "acceptEdits"]);
        assert_eq!(session_grants(&Value::Null), (vec![], vec![]));
    }

    #[test]
    fn a_permission_answer_carries_the_input_back_or_denies_with_a_message() {
        let input = json!({"command": "touch a"});
        let (pending, _) = ask(
            "r1",
            &request("Bash", input.clone(), json!([])),
            &input,
            "toolu_1",
        );
        let allowed = json!({"behavior": "allow", "updatedInput": input});
        assert_eq!(pending.response(&ApprovalDecision::AllowOnce), allowed);
        // With nothing to grant, allowing for the session allows this call alone.
        let session = ApprovalDecision::AllowSession {
            both_directions: false,
        };
        assert_eq!(pending.response(&session), allowed);
        for decision in [
            ApprovalDecision::Cancel,
            ApprovalDecision::Decline,
            ApprovalDecision::Deny {
                reason: Some(" ".into()),
            },
        ] {
            assert_eq!(
                pending.response(&decision),
                json!({"behavior": "deny", "message": DENIED})
            );
        }
    }

    #[test]
    fn questions_left_unanswered_deny_the_question_call() {
        let input = json!({"questions": [{"question": "Which?", "header": "Pick",
            "options": [{"label": "A", "description": ""}], "multiSelect": false}]});
        let (pending, approval) = ask(
            "r1",
            &request("AskUserQuestion", input.clone(), Value::Null),
            &input,
            "toolu_1",
        );
        assert!(matches!(approval, ApprovalRequest::Questions { .. }));
        let unanswered = json!({"behavior": "deny", "message": UNANSWERED});
        for content in [
            None,
            Some(json!({"answers": {}})),
            Some(json!({"answers": {"q1": {"answers": []}}})),
        ] {
            assert_eq!(
                pending.response(&ApprovalDecision::Accept { content }),
                unanswered
            );
        }
        assert_eq!(pending.response(&ApprovalDecision::AllowOnce), unanswered);
    }
}
