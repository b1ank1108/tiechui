#![allow(clippy::unwrap_used)]

mod client;
mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use client::Client;
use serde_json::{Value, json};
use tempfile::TempDir;
use tiechui_protocol::app::{SessionEvent, Snapshot};
use tiechui_protocol::{
    ApprovalRequest, CommandInfo, CommandKind, DerivedReason, DerivedTurnMarker, EventKind, Item,
    KnownItem, ModelInfo, PERMISSION_MODE_EVENT, SubagentItem, SubagentState, SubagentStatus,
    ThinkingLevel, TokenUsage, TurnId, TurnStatus,
};

const SESSION: &str = "fake-session";

fn expect(frame: Value) -> Value {
    json!({ "expect": frame })
}

fn emit(frame: Value) -> Value {
    json!({ "emit": frame })
}

fn handshake() -> Vec<Value> {
    vec![
        expect(json!({"type": "control_request", "request": {"subtype": "initialize"}})),
        emit(json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": "$id", "response": {"commands": []}}})),
    ]
}

fn user_message(text: &str) -> Value {
    expect(json!({"type": "user", "message": {"role": "user",
        "content": [{"type": "text", "text": text}]}}))
}

fn frame(kind: &str, fields: Value) -> Value {
    let mut frame = json!({"type": kind, "session_id": SESSION, "parent_tool_use_id": null});
    frame
        .as_object_mut()
        .unwrap()
        .extend(fields.as_object().unwrap().clone());
    emit(frame)
}

fn system(subtype: &str, fields: Value) -> Value {
    let mut fields = fields;
    fields["subtype"] = json!(subtype);
    frame("system", fields)
}

fn stream(msg: &str, event: Value) -> Value {
    frame(
        "stream_event",
        json!({"event": event, "api_message_id": msg}),
    )
}

fn usage_json(input: u64, output: u64, read: u64, write: u64) -> Value {
    json!({"input_tokens": input, "output_tokens": output,
        "cache_read_input_tokens": read, "cache_creation_input_tokens": write})
}

fn message_start(msg: &str) -> Value {
    stream(
        msg,
        json!({"type": "message_start", "message": {"id": msg, "role": "assistant", "content": []}}),
    )
}

fn message_end(msg: &str, usage: Value, stop: &str) -> Vec<Value> {
    vec![
        stream(
            msg,
            json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": usage}),
        ),
        stream(msg, json!({"type": "message_stop"})),
    ]
}

fn streamed_block(msg: &str, index: u64, kind: &str, deltas: &[&str]) -> Vec<Value> {
    let (field, delta) = if kind == "text" {
        ("text", "text_delta")
    } else {
        ("thinking", "thinking_delta")
    };
    let mut steps = vec![stream(
        msg,
        json!({"type": "content_block_start", "index": index,
            "content_block": {"type": kind, field: ""}}),
    )];
    for text in deltas {
        steps.push(stream(
            msg,
            json!({"type": "content_block_delta", "index": index,
                "delta": {"type": delta, field: text}}),
        ));
    }
    steps.push(stream(
        msg,
        json!({"type": "content_block_stop", "index": index}),
    ));
    steps
}

/// A tool call as Claude sends it: a streamed `tool_use` block, then the `assistant` frame
/// carrying its complete input.
fn tool_use(msg: &str, index: u64, id: &str, name: &str, input: &Value) -> Vec<Value> {
    let text = input.to_string();
    let (head, tail) = text.split_at(text.len() / 2);
    vec![
        stream(
            msg,
            json!({"type": "content_block_start", "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
        ),
        stream(
            msg,
            json!({"type": "content_block_delta", "index": index,
                "delta": {"type": "input_json_delta", "partial_json": head}}),
        ),
        stream(
            msg,
            json!({"type": "content_block_delta", "index": index,
                "delta": {"type": "input_json_delta", "partial_json": tail}}),
        ),
        frame(
            "assistant",
            json!({"message": {"id": msg, "role": "assistant", "content": [
                {"type": "tool_use", "id": id, "name": name, "input": input}]}}),
        ),
        stream(msg, json!({"type": "content_block_stop", "index": index})),
    ]
}

fn tool_result(id: &str, content: Value, is_error: bool) -> Value {
    frame(
        "user",
        json!({"message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": content, "is_error": is_error}]}}),
    )
}

fn result_ok(text: &str) -> Value {
    frame(
        "result",
        json!({"subtype": "success", "is_error": false, "result": text,
            "terminal_reason": "completed"}),
    )
}

fn result_error(subtype: &str, errors: &[&str]) -> Value {
    frame(
        "result",
        json!({"subtype": subtype, "is_error": true, "result": null, "errors": errors,
            "terminal_reason": null}),
    )
}

fn recorded_script(frames: &str) -> Vec<Value> {
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend(
        frames
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()),
    );
    steps
}

fn simple_turn(msg: &str, text: &str, usage: Value) -> Vec<Value> {
    let mut steps = vec![
        system("init", json!({"model": "claude-fake"})),
        message_start(msg),
    ];
    steps.extend(streamed_block(msg, 0, "text", &[text]));
    steps.extend(message_end(msg, usage, "end_turn"));
    steps.push(result_ok(text));
    steps
}

struct Fixture {
    root: TempDir,
    cwd: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(root.path().join("home")).unwrap();
        std::fs::create_dir_all(root.path().join("bin")).unwrap();
        let name = if cfg!(windows) {
            "claude.exe"
        } else {
            "claude"
        };
        let fake = root.path().join("bin").join(name);
        std::fs::copy(env!("CARGO_BIN_EXE_fake-claude"), &fake).unwrap();
        // The first run of a new binary can be slow (the OS scans it); keep that out of the
        // tests' short timeouts.
        std::process::Command::new(&fake)
            .arg("--version")
            .output()
            .unwrap();
        Self { root, cwd }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    fn write_script(&self, name: &str, steps: &[Value]) {
        let text: Vec<String> = steps.iter().map(Value::to_string).collect();
        std::fs::write(self.path(name), text.join("\n")).unwrap();
    }

    fn script(&self, steps: Vec<Value>) {
        self.write_script("script.ndjson", &steps);
    }

    fn script_for_launch(&self, zero_based_launch: usize, steps: Vec<Value>) {
        self.write_script(&format!("script.ndjson.{zero_based_launch}"), &steps);
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("fake.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn launches(&self) -> Vec<Vec<String>> {
        self.log()
            .iter()
            .filter_map(|line| line["argv"].as_array())
            .map(|argv| {
                argv.iter()
                    .map(|arg| arg.as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .filter(|argv| argv != &["--version"])
            .collect()
    }

    fn received(&self) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|line| line.get("argv").is_none())
            .collect()
    }

    fn mismatches(&self) -> Vec<String> {
        std::fs::read_to_string(self.path("fake.log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with("MISMATCH") || line.starts_with("EOF"))
            .map(str::to_owned)
            .collect()
    }

    fn client(&self) -> Client {
        self.client_with(&[])
    }

    fn client_with(&self, env: &[(&str, &str)]) -> Client {
        let mut command = self.app_server();
        for (key, value) in env {
            command.env(key, value);
        }
        Client::spawn(command)
    }

    fn app_server(&self) -> tokio::process::Command {
        let bin = self.path("bin");
        let search =
            std::env::join_paths([bin.as_path(), Path::new("/usr/bin"), Path::new("/bin")])
                .unwrap();
        let mut command = support::command();
        command
            .env("PATH", search)
            .env("TIECHUI_HOME", self.path("home"))
            .env("HOME", self.root.path())
            .env("FAKE_CLAUDE_SCRIPT", self.path("script.ndjson"))
            .env("FAKE_CLAUDE_LOG", self.path("fake.log"))
            .current_dir(&self.cwd)
            .arg("app-server");
        command
    }

    #[cfg(unix)]
    fn daemon(&self, socket: &Path) -> tokio::process::Child {
        let mut command = self.app_server();
        command
            .arg("--socket")
            .arg(socket)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true);
        command.spawn().unwrap()
    }

    #[cfg(unix)]
    fn connect(&self, socket: &Path) -> Client {
        let mut command = self.app_server();
        command.arg("--connect").arg(socket);
        Client::spawn(command)
    }
}

fn snapshot(value: Value) -> Snapshot {
    serde_json::from_value(value).unwrap()
}

async fn open(client: &mut Client, fixture: &Fixture, mode: Option<&str>) -> Snapshot {
    snapshot(
        client
            .ok(
                "session/open",
                json!({"cwd": fixture.cwd, "runtime": "claude", "mode": mode}),
            )
            .await,
    )
}

async fn submit(client: &mut Client, thread: &str, text: &str, delivery: &str) {
    client
        .ok(
            "session/submit",
            json!({"thread_id": thread, "op": {"kind": "user_input", "delivery": delivery,
                "content": [{"kind": "text", "text": text}]}}),
        )
        .await;
}

fn is_completed(kind: &EventKind) -> bool {
    matches!(kind, EventKind::TurnCompleted { .. })
}

fn completed_status(events: &[SessionEvent]) -> TurnStatus {
    match &events.last().unwrap().event.event {
        EventKind::TurnCompleted { turn } => turn.status,
        other => panic!("{other:?}"),
    }
}

fn known(events: &[SessionEvent]) -> Vec<KnownItem> {
    events
        .iter()
        .filter_map(|e| match &e.event.event {
            EventKind::ItemCompleted {
                item: Item::Known(item),
                ..
            } => Some(item.clone()),
            _ => None,
        })
        .collect()
}

fn usages(events: &[SessionEvent]) -> Vec<TokenUsage> {
    events
        .iter()
        .filter_map(|e| match &e.event.event {
            EventKind::TokenUsageUpdated { last, .. } => Some(*last),
            _ => None,
        })
        .collect()
}

fn errors(events: &[SessionEvent]) -> Vec<String> {
    event_errors(events.iter().map(|e| &e.event))
}

fn event_errors<'a>(events: impl Iterator<Item = &'a tiechui_protocol::Event>) -> Vec<String> {
    events
        .filter_map(|e| match &e.event {
            EventKind::Error { message } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn arg_after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.iter()
        .position(|arg| arg == flag)
        .and_then(|index| argv.get(index + 1))
        .map(String::as_str)
}

fn saved_claude_session_id(path: &str) -> String {
    fn find(value: &Value) -> Option<String> {
        match value {
            Value::Object(fields) => {
                if fields.get("custom_kind").and_then(Value::as_str) == Some("runtime.thread") {
                    return fields["data"]["id"].as_str().map(str::to_owned);
                }
                fields.values().find_map(find)
            }
            Value::Array(items) => items.iter().find_map(find),
            _ => None,
        }
    }
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|line| find(&line))
        .next_back()
        .unwrap()
}

#[cfg(unix)]
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(unix)]
async fn gone_within(pid: i32, limit: Duration) -> bool {
    let deadline = std::time::Instant::now() + limit;
    while std::time::Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[cfg(unix)]
fn pid_in(path: &Path) -> i32 {
    std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn runtime_entry(list: &Value) -> &Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "claude")
        .unwrap()
}

#[tokio::test]
async fn the_probe_runs_only_version_and_reports_the_version_and_path() {
    let fixture = Fixture::new();
    fixture.script(vec![]);
    let mut client = fixture.client();
    let list = client.ok("runtimes/list", json!({})).await;
    let claude = runtime_entry(&list);
    assert_eq!(claude["label"], "Claude Code");
    assert_eq!(claude["available"], true, "{claude}");
    assert_eq!(claude["version"], "2.1.322");
    assert!(claude["path"].as_str().unwrap().ends_with("claude"));
    assert_eq!(claude["capabilities"]["steering"], "inject");
    for gated in ["commands", "thinking", "model", "permissions"] {
        assert_eq!(claude["capabilities"][gated], true, "{gated}");
    }
    for gated in [
        "branch",
        "fork",
        "rewind",
        "retry",
        "compact",
        "side_chat",
        "shell",
    ] {
        assert_eq!(claude["capabilities"][gated], false, "{gated}");
    }
    let argv: Vec<_> = fixture
        .log()
        .iter()
        .filter_map(|line| line["argv"].as_array().cloned())
        .collect();
    assert!(
        !argv.is_empty() && argv.iter().all(|args| args == &[json!("--version")]),
        "{argv:?}"
    );
}

#[tokio::test]
async fn claude_older_than_the_minimum_is_unavailable_and_the_reason_names_both_versions() {
    let fixture = Fixture::new();
    fixture.script(vec![]);
    let mut client = fixture.client_with(&[("FAKE_CLAUDE_VERSION", "2.0.9")]);
    let list = client.ok("runtimes/list", json!({})).await;
    let claude = runtime_entry(&list);
    assert_eq!(claude["available"], false, "{claude}");
    assert_eq!(claude["version"], "2.0.9");
    let reason = claude["reason"].as_str().unwrap();
    assert!(
        reason.contains("2.0.9") && reason.contains("2.1.322"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_missing_claude_is_unavailable_with_a_reason() {
    let fixture = Fixture::new();
    std::fs::remove_dir_all(fixture.path("bin")).unwrap();
    std::fs::create_dir_all(fixture.path("bin")).unwrap();
    let mut client = fixture.client();
    let list = client.ok("runtimes/list", json!({})).await;
    let claude = runtime_entry(&list);
    assert_eq!(claude["available"], false);
    assert_eq!(claude["reason"], "claude is not on PATH");
}

#[tokio::test]
async fn a_turn_streams_text_and_thinking_maps_tools_counts_usage_once_and_ends_at_result() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("do it"));
    steps.extend([
        system("init", json!({"model": "claude-fake"})),
        system("status", json!({"status": "requesting"})),
        message_start("msg_1"),
    ]);
    steps.extend(streamed_block(
        "msg_1",
        0,
        "thinking",
        &["let me ", "think"],
    ));
    steps.extend(streamed_block("msg_1", 1, "text", &["I will ", "read."]));
    steps.extend(tool_use(
        "msg_1",
        2,
        "toolu_read",
        "Read",
        &json!({"file_path": "/work/a.txt", "offset": 3, "limit": 2}),
    ));
    steps.extend(tool_use(
        "msg_1",
        3,
        "toolu_bash",
        "Bash",
        &json!({"command": "ls", "description": "list"}),
    ));
    steps.extend(message_end(
        "msg_1",
        usage_json(2, 40, 1000, 300),
        "tool_use",
    ));
    steps.push(tool_result("toolu_read", json!("3\tx\n4\ty"), false));
    steps.push(tool_result("toolu_bash", json!("a.txt\nb.txt"), false));
    steps.push(system("status", json!({"status": "requesting"})));
    steps.push(message_start("msg_2"));
    steps.extend(tool_use(
        "msg_2",
        0,
        "toolu_write",
        "Write",
        &json!({"file_path": "/work/c.txt", "content": "hi\n"}),
    ));
    steps.extend(tool_use(
        "msg_2",
        1,
        "toolu_edit",
        "Edit",
        &json!({"file_path": "/work/c.txt", "old_string": "hi", "new_string": "yo", "replace_all": false}),
    ));
    steps.extend(message_end("msg_2", usage_json(2, 10, 1500, 0), "tool_use"));
    let repeated_message_delta = stream(
        "msg_2",
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"},
        "usage": usage_json(2, 10, 1500, 0)}),
    );
    steps.push(repeated_message_delta);
    steps.push(tool_result("toolu_write", json!("File created"), false));
    steps.push(tool_result(
        "toolu_edit",
        json!([{"type": "text", "text": "updated"}]),
        false,
    ));
    steps.push(message_start("msg_3"));
    steps.extend(streamed_block("msg_3", 0, "text", &["done"]));
    steps.extend(message_end("msg_3", usage_json(2, 5, 1700, 0), "end_turn"));
    let gap_before_result = json!({"sleep_ms": 300});
    steps.push(gap_before_result);
    steps.push(result_ok("done"));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    submit(&mut client, &opened.thread_id.0, "do it", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));

    let items = known(&events);
    let find_call = |name: &str| {
        items
            .iter()
            .find_map(|item| match item {
                KnownItem::ToolCall {
                    name: n, arguments, ..
                } if n == name => Some(arguments.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {name} call in {items:?}"))
    };
    assert_eq!(
        find_call("read"),
        json!({"path": "/work/a.txt", "offset": 3, "limit": 2})
    );
    assert_eq!(find_call("bash"), json!({"command": "ls"}));
    assert_eq!(
        find_call("write"),
        json!({"path": "/work/c.txt", "content": "hi\n"})
    );
    assert_eq!(
        find_call("edit"),
        json!({"path": "/work/c.txt", "edits": [{"oldText": "hi", "newText": "yo"}]})
    );
    let results: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            KnownItem::ToolResult {
                call_id,
                output,
                is_error,
                ..
            } => Some((
                call_id.0.clone(),
                output["content"][0]["text"].as_str().unwrap().to_owned(),
                *is_error,
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        [
            ("toolu_read".to_owned(), "3\tx\n4\ty".to_owned(), false),
            ("toolu_bash".to_owned(), "a.txt\nb.txt".to_owned(), false),
            ("toolu_write".to_owned(), "File created".to_owned(), false),
            ("toolu_edit".to_owned(), "updated".to_owned(), false),
        ]
    );
    let texts: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            KnownItem::AssistantMessage { text, .. } => Some(text.as_str()),
            KnownItem::Thinking { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["let me think", "I will read.", "done"]);

    let deltas: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.event.event {
            EventKind::ItemDelta { delta, .. } => Some(delta.clone()),
            _ => None,
        })
        .collect();
    assert!(deltas.concat().contains("let me think"), "{deltas:?}");
    assert!(deltas.concat().contains("I will read."), "{deltas:?}");

    let used = usages(&events);
    assert_eq!(used.len(), 3, "one usage per message id: {used:?}");
    assert_eq!(
        (
            used[0].input,
            used[0].output,
            used[0].cache_read,
            used[0].cache_write
        ),
        (2, 40, 1000, 300)
    );
    assert_eq!(used[1].output, 10);
    assert_eq!(used[2].output, 5);
    assert_script_followed(&fixture);
}

/// Frames of a real 2.1.322 turn (thinking summary, Write, Bash, Read, Edit, final text) as
/// recorded, with paths and signatures scrubbed.
async fn recorded_tools_turn(fixture: &Fixture) -> Vec<SessionEvent> {
    fixture.script(recorded_script(include_str!(
        "fixtures/claude-tools-turn.ndjson"
    )));
    let mut client = fixture.client();
    let opened = open(&mut client, fixture, None).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    client.until(is_completed).await
}

#[tokio::test]
async fn a_recorded_turn_maps_like_the_synthetic_ones() {
    let fixture = Fixture::new();
    let events = recorded_tools_turn(&fixture).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));
    let items = known(&events);
    let calls: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            KnownItem::ToolCall { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, ["write", "bash", "read", "edit"]);
    let results = items
        .iter()
        .filter(|item| {
            matches!(
                item,
                KnownItem::ToolResult {
                    is_error: false,
                    ..
                }
            )
        })
        .count();
    assert_eq!(results, 4);
    assert!(items.iter().any(|item| matches!(item,
        KnownItem::Thinking { text, .. } if text.contains("make the edit"))));
    assert!(items.iter().any(|item| matches!(item,
        KnownItem::AssistantMessage { text, .. } if text == "done")));
}

/// The values are the recording's `message_delta` totals, and the 1 h cache write of the
/// message's `message_start`, the only frame that breaks the write down.
#[tokio::test]
async fn a_recorded_turn_reports_each_messages_usage_with_its_one_hour_cache_write() {
    let fixture = Fixture::new();
    let events = recorded_tools_turn(&fixture).await;
    let recorded = |output, cache_read, cache_write, reasoning, cache_write_1h| TokenUsage {
        input: 2,
        output,
        cache_read,
        cache_write,
        reasoning,
        cache_write_1h,
        cost_nano_usd: None,
    };
    assert_eq!(
        usages(&events),
        [
            recorded(188, 10624, 11663, 0, 11663),
            recorded(282, 22287, 2165, 187, 2165),
            recorded(143, 24452, 318, 0, 318),
            recorded(3, 24770, 246, 0, 246),
        ]
    );
}

async fn assert_answered_with_an_error(request_id: &str, request: Value) {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend([
        emit(json!({"type": "control_request", "request_id": request_id, "request": request})),
        expect(json!({"type": "control_response", "response": {
            "subtype": "error", "request_id": request_id}})),
    ]);
    steps.extend(simple_turn("msg_1", "ok", usage_json(1, 1, 0, 0)));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert_script_followed(&fixture);
}

#[tokio::test]
async fn control_requests_without_a_mapping_are_answered_so_claude_never_waits() {
    assert_answered_with_an_error(
        "hook-1",
        json!({"subtype": "hook_callback", "callback_id": "x"}),
    )
    .await;
}

#[tokio::test]
async fn a_control_request_that_does_not_parse_is_answered_so_claude_never_waits() {
    let description_not_a_string = json!({"subtype": "can_use_tool", "tool_name": "Bash",
        "input": {"command": "ls"}, "description": 7, "permission_suggestions": [],
        "tool_use_id": "toolu_ls"});
    assert_answered_with_an_error("perm-odd", description_not_a_string).await;
}

/// A `can_use_tool` request as Claude 2.1.322 sends it (recorded shape).
fn can_use_tool(request_id: &str, tool: &str, input: &Value, tool_use_id: &str) -> Value {
    can_use_tool_suggesting(request_id, tool, input, tool_use_id, json!([]))
}

fn can_use_tool_suggesting(
    request_id: &str,
    tool: &str,
    input: &Value,
    tool_use_id: &str,
    suggestions: Value,
) -> Value {
    emit(
        json!({"type": "control_request", "request_id": request_id, "request": {
        "subtype": "can_use_tool", "tool_name": tool, "display_name": tool, "input": input,
        "description": tool, "permission_suggestions": suggestions, "tool_use_id": tool_use_id}}),
    )
}

fn expect_permission_answer(request_id: &str, response: Value) -> Value {
    expect(json!({"type": "control_response", "response": {
        "subtype": "success", "request_id": request_id, "response": response}}))
}

/// The alternatives 2.1.322 suggests for a command that writes a file in the workspace.
fn bash_suggestions(command: &str) -> Value {
    json!([
        {"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": command}],
            "behavior": "allow", "destination": "localSettings"},
        {"type": "addDirectories", "directories": ["/work"], "destination": "session"},
        {"type": "setMode", "mode": "acceptEdits", "destination": "session"},
    ])
}

fn reply(msg: &str, text: &str) -> Vec<Value> {
    let mut steps = vec![message_start(msg)];
    steps.extend(streamed_block(msg, 0, "text", &[text]));
    steps.extend(message_end(msg, usage_json(1, 1, 0, 0), "end_turn"));
    steps.push(result_ok(text));
    steps
}

fn tool_message(msg: &str, id: &str, name: &str, input: &Value) -> Vec<Value> {
    let mut steps = vec![message_start(msg)];
    steps.extend(tool_use(msg, 0, id, name, input));
    steps
}

async fn next_approval(client: &mut Client) -> (Vec<SessionEvent>, String, ApprovalRequest) {
    let seen = client
        .until(|kind| matches!(kind, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested {
        approval_id,
        request,
        ..
    } = &seen.last().unwrap().event.event
    else {
        unreachable!()
    };
    let (id, request) = (approval_id.0.clone(), request.clone());
    (seen, id, request)
}

async fn answer(client: &mut Client, thread: &str, approval: &str, decision: Value) {
    client
        .ok(
            "session/submit",
            json!({"thread_id": thread, "op": {"kind": "approval_response",
                "approval_id": approval, "decision": decision}}),
        )
        .await;
}

fn responses_to(fixture: &Fixture, request_id: &str) -> Vec<Value> {
    fixture
        .received()
        .into_iter()
        .filter(|line| {
            line["type"] == "control_response" && line["response"]["request_id"] == request_id
        })
        .map(|line| line["response"]["response"].clone())
        .collect()
}

fn resolved(events: &[SessionEvent], approval: &str) -> Option<usize> {
    events.iter().position(|e| {
        matches!(&e.event.event,
        EventKind::ApprovalResolved { approval_id } if approval_id.0 == approval)
    })
}

fn tool_calls_with_id(events: &[SessionEvent], id: &str) -> usize {
    known(events)
        .iter()
        .filter(|item| matches!(item, KnownItem::ToolCall { id: call, .. } if call.0 == id))
        .count()
}

fn assert_script_followed(fixture: &Fixture) {
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

#[tokio::test]
async fn a_permission_request_waits_on_its_tool_step_and_allowing_writes_the_input_back() {
    let fixture = Fixture::new();
    let input = json!({"command": "touch approved.txt", "description": "Create approved.txt"});
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend(tool_message("msg_1", "toolu_touch", "Bash", &input));
    steps.extend([
        can_use_tool_suggesting(
            "perm-1",
            "Bash",
            &input,
            "toolu_touch",
            bash_suggestions("touch approved.txt"),
        ),
        expect_permission_answer(
            "perm-1",
            json!({"behavior": "allow", "updatedInput": input}),
        ),
    ]);
    steps.extend(message_end("msg_1", usage_json(1, 1, 0, 0), "tool_use"));
    steps.push(tool_result(
        "toolu_touch",
        json!("(Bash completed with no output)"),
        false,
    ));
    steps.extend(reply("msg_2", "created"));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let (seen, approval, request) = next_approval(&mut client).await;
    assert!(approval.starts_with("claude-"), "{approval}");
    assert_eq!(
        tool_calls_with_id(&seen, "toolu_touch"),
        1,
        "the step the card waits on exists before it"
    );
    let ApprovalRequest::Permission {
        actor,
        call_id,
        tool,
        arguments,
        values,
        session_patterns,
        origin,
        ..
    } = request
    else {
        panic!("{request:?}");
    };
    assert_eq!(actor, None);
    assert_eq!(call_id, "toolu_touch");
    assert_eq!(tool, "bash");
    assert_eq!(arguments, json!({"command": "touch approved.txt"}));
    assert_eq!(values, ["touch approved.txt"]);
    assert_eq!(session_patterns, ["Bash(touch approved.txt)"]);
    assert_eq!(origin, "claude");

    answer(
        &mut client,
        &thread,
        &approval,
        json!({"kind": "allow_once"}),
    )
    .await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert!(resolved(&events, &approval).is_some());
    assert_script_followed(&fixture);
    let sent = responses_to(&fixture, "perm-1");
    assert_eq!(
        sent,
        [json!({"behavior": "allow", "updatedInput": input})],
        "allowing once grants nothing more"
    );
}

#[tokio::test]
async fn denying_writes_the_reason_back_so_claude_sees_it() {
    let fixture = Fixture::new();
    let input = json!({"command": "touch denied.txt"});
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend(tool_message("msg_1", "toolu_touch", "Bash", &input));
    steps.extend([
        can_use_tool("perm-1", "Bash", &input, "toolu_touch"),
        expect_permission_answer(
            "perm-1",
            json!({"behavior": "deny", "message": "not that file"}),
        ),
    ]);
    steps.extend(message_end("msg_1", usage_json(1, 1, 0, 0), "tool_use"));
    steps.push(tool_result("toolu_touch", json!("not that file"), true));
    let second = json!({"command": "touch other.txt"});
    steps.extend(tool_message("msg_2", "toolu_other", "Bash", &second));
    steps.extend([
        can_use_tool("perm-2", "Bash", &second, "toolu_other"),
        expect_permission_answer("perm-2", json!({"behavior": "deny"})),
    ]);
    steps.push(tool_result("toolu_other", json!("denied"), true));
    steps.extend(reply("msg_3", "skipped"));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let (_, approval, _) = next_approval(&mut client).await;
    answer(
        &mut client,
        &thread,
        &approval,
        json!({"kind": "deny", "reason": "not that file"}),
    )
    .await;
    let (_, approval, _) = next_approval(&mut client).await;
    answer(
        &mut client,
        &thread,
        &approval,
        json!({"kind": "deny", "reason": null}),
    )
    .await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert_script_followed(&fixture);
    assert_eq!(
        responses_to(&fixture, "perm-1"),
        [json!({"behavior": "deny", "message": "not that file"})]
    );
    let without_reason = responses_to(&fixture, "perm-2");
    assert!(
        without_reason[0]["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty()),
        "{without_reason:?}"
    );
}

#[tokio::test]
async fn allowing_for_the_session_grants_claudes_suggestions_for_this_session_only() {
    let fixture = Fixture::new();
    let bash = json!({"command": "touch a.txt"});
    let write = json!({"file_path": "/work/b.txt", "content": "b\n"});
    let early = json!({"file_path": "/work/c.txt", "content": "c\n"});
    let mut steps = handshake();
    steps.push(user_message("go"));
    let narrowest_rule_for_session = json!({"type": "addRules",
        "rules": [{"toolName": "Bash", "ruleContent": "touch a.txt"}],
        "behavior": "allow", "destination": "session"});
    steps.extend(tool_message("msg_1", "toolu_bash", "Bash", &bash));
    steps.extend([
        can_use_tool_suggesting(
            "perm-1",
            "Bash",
            &bash,
            "toolu_bash",
            bash_suggestions("touch a.txt"),
        ),
        expect_permission_answer(
            "perm-1",
            json!({"behavior": "allow", "updatedInput": bash,
                "updatedPermissions": [narrowest_rule_for_session]}),
        ),
        tool_result("toolu_bash", json!(""), false),
    ]);
    steps.extend(tool_message("msg_2", "toolu_write", "Write", &write));
    steps.extend([
        // The suggestion 2.1.322 sends for a file write.
        can_use_tool_suggesting(
            "perm-2",
            "Write",
            &write,
            "toolu_write",
            json!([{"type": "setMode", "mode": "acceptEdits", "destination": "session"}]),
        ),
        expect_permission_answer(
            "perm-2",
            json!({"behavior": "allow", "updatedInput": write, "updatedPermissions": [
                {"type": "setMode", "mode": "acceptEdits", "destination": "session"}]}),
        ),
        tool_result("toolu_write", json!("File created"), false),
    ]);
    let request_before_its_assistant_frame = [
        message_start("msg_3"),
        can_use_tool("perm-3", "Write", &early, "toolu_early"),
        expect_permission_answer(
            "perm-3",
            json!({"behavior": "allow", "updatedInput": early}),
        ),
    ];
    steps.extend(request_before_its_assistant_frame);
    steps.extend(tool_use("msg_3", 0, "toolu_early", "Write", &early));
    steps.push(tool_result("toolu_early", json!("File created"), false));
    steps.extend(reply("msg_4", "done"));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let session = json!({"kind": "allow_session", "both_directions": false});
    let mut seen = Vec::new();
    let mut patterns = Vec::new();
    for _ in 0..3 {
        let (events, approval, request) = next_approval(&mut client).await;
        seen.extend(events);
        let ApprovalRequest::Permission {
            session_patterns,
            call_id,
            tool,
            values,
            ..
        } = request
        else {
            panic!("{request:?}");
        };
        assert_eq!(tool_calls_with_id(&seen, &call_id), 1, "{call_id}");
        patterns.push((tool, values, session_patterns));
        answer(&mut client, &thread, &approval, session.clone()).await;
    }
    let events = client.until(is_completed).await;
    seen.extend(events);
    assert_eq!(completed_status(&seen), TurnStatus::Completed);
    assert_script_followed(&fixture);
    assert_eq!(
        patterns,
        [
            (
                "bash".to_owned(),
                vec!["touch a.txt".to_owned()],
                vec!["Bash(touch a.txt)".to_owned()]
            ),
            (
                "write".to_owned(),
                vec!["/work/b.txt".to_owned()],
                vec!["acceptEdits".to_owned()]
            ),
            ("write".to_owned(), vec!["/work/c.txt".to_owned()], vec![]),
        ]
    );
    assert_eq!(
        tool_calls_with_id(&seen, "toolu_early"),
        1,
        "the later assistant frame does not repeat the call"
    );
    assert_eq!(
        responses_to(&fixture, "perm-3"),
        [json!({"behavior": "allow", "updatedInput": early})],
        "with nothing to grant, allowing for the session allows once"
    );
}

#[tokio::test]
async fn ask_user_question_becomes_question_cards_and_the_answers_go_back_in_the_input() {
    let fixture = Fixture::new();
    let input = json!({"questions": [
        {"question": "Which color?", "header": "Color", "multiSelect": false, "options": [
            {"label": "Red", "description": "warm"}, {"label": "Blue", "description": "cool"}]},
        {"question": "Which extras?", "header": "Extras", "multiSelect": true, "options": [
            {"label": "Tests", "description": "add tests"},
            {"label": "Docs", "description": "add docs"}]},
    ]});
    let mut answered = input.clone();
    answered["answers"] = json!({"Which color?": "Blue", "Which extras?": "Tests, Docs"});
    let mut steps = handshake();
    steps.push(user_message("ask me"));
    steps.extend(tool_message(
        "msg_1",
        "toolu_ask",
        "AskUserQuestion",
        &input,
    ));
    steps.extend([
        can_use_tool("perm-1", "AskUserQuestion", &input, "toolu_ask"),
        expect_permission_answer(
            "perm-1",
            json!({"behavior": "allow", "updatedInput": answered}),
        ),
        tool_result(
            "toolu_ask",
            json!("User has answered your questions: \"Which color?\"=\"Blue\""),
            false,
        ),
    ]);
    steps.extend(reply("msg_2", "You picked Blue"));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "ask me", "follow_up").await;
    let (_, approval, request) = next_approval(&mut client).await;
    let ApprovalRequest::Questions { questions } = request else {
        panic!("{request:?}");
    };
    let shown: Vec<_> = questions
        .iter()
        .map(|q| {
            let labels: Vec<_> = q
                .options
                .iter()
                .flatten()
                .map(|o| (o.label.as_str(), o.description.as_str()))
                .collect();
            (q.header.as_str(), q.question.as_str(), labels)
        })
        .collect();
    assert_eq!(
        shown,
        [
            (
                "Color",
                "Which color?",
                vec![("Red", "warm"), ("Blue", "cool")]
            ),
            (
                "Extras",
                "Which extras?",
                vec![("Tests", "add tests"), ("Docs", "add docs")]
            ),
        ]
    );
    let answers: serde_json::Map<String, Value> = questions
        .iter()
        .zip([json!(["Blue"]), json!(["Tests", "Docs"])])
        .map(|(q, picked)| (q.id.clone(), json!({"answers": picked})))
        .collect();
    answer(
        &mut client,
        &thread,
        &approval,
        json!({"kind": "accept", "content": {"answers": answers}}),
    )
    .await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert_script_followed(&fixture);
    assert_eq!(
        responses_to(&fixture, "perm-1"),
        [json!({"behavior": "allow", "updatedInput": answered})]
    );
}

fn desktop_card_text(arguments: &Value, values: &[String]) -> String {
    arguments["command"]
        .as_str()
        .map_or_else(|| values.join(" "), str::to_owned)
}

#[tokio::test]
async fn a_plan_is_shown_in_full_and_both_rejecting_and_approving_let_claude_continue() {
    let fixture = Fixture::new();
    let enter = json!({});
    let first = json!({"plan": "# Plan\n\n1. Create a.txt\n2. Run the tests"});
    let second = json!({"plan": "# Plan\n\n1. Create a.txt\n2. Run the tests\n3. Update docs"});
    // As 2.1.322 does it: entering plan mode asks nothing, and the request to leave it carries
    // an empty input, the plan being only in the `tool_use` block before it.
    let exit_plan = |request_id: &str, tool_use_id: &str| {
        emit(
            json!({"type": "control_request", "request_id": request_id, "request": {
            "subtype": "can_use_tool", "tool_name": "ExitPlanMode", "display_name": "ExitPlanMode",
            "input": {}, "tool_use_id": tool_use_id, "requires_user_interaction": true}}),
        )
    };
    let mut steps = handshake();
    steps.push(user_message("plan first"));
    steps.extend(tool_message(
        "msg_1",
        "toolu_enter",
        "EnterPlanMode",
        &enter,
    ));
    steps.push(tool_result(
        "toolu_enter",
        json!("Entered plan mode."),
        false,
    ));
    steps.extend(tool_message("msg_2", "toolu_plan1", "ExitPlanMode", &first));
    steps.extend([
        exit_plan("perm-1", "toolu_plan1"),
        expect_permission_answer(
            "perm-1",
            json!({"behavior": "deny", "message": "Also update the docs"}),
        ),
        tool_result("toolu_plan1", json!("Also update the docs"), true),
    ]);
    steps.extend(tool_message(
        "msg_3",
        "toolu_plan2",
        "ExitPlanMode",
        &second,
    ));
    steps.extend([
        exit_plan("perm-2", "toolu_plan2"),
        expect_permission_answer("perm-2", json!({"behavior": "allow", "updatedInput": {}})),
        tool_result(
            "toolu_plan2",
            json!("User has approved exiting plan mode. You can now proceed."),
            false,
        ),
    ]);
    steps.extend(reply("msg_4", "Starting on the plan"));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "plan first", "follow_up").await;

    let mut plans = Vec::new();
    for decision in [
        json!({"kind": "deny", "reason": "Also update the docs"}),
        json!({"kind": "allow_once"}),
    ] {
        let (_, approval, request) = next_approval(&mut client).await;
        let ApprovalRequest::Permission {
            tool,
            call_id,
            values,
            arguments,
            session_patterns,
            ..
        } = request
        else {
            panic!("{request:?}");
        };
        assert_eq!(tool, "ExitPlanMode");
        assert!(call_id.starts_with("toolu_plan"), "{call_id}");
        assert!(session_patterns.is_empty());
        plans.push(desktop_card_text(&arguments, &values));
        answer(&mut client, &thread, &approval, decision).await;
    }
    assert_eq!(
        plans,
        [
            first["plan"].as_str().unwrap(),
            second["plan"].as_str().unwrap(),
        ]
    );
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert_script_followed(&fixture);
}

#[tokio::test]
async fn a_cancelled_request_is_withdrawn_and_a_late_answer_writes_nothing() {
    let fixture = Fixture::new();
    let input = json!({"command": "touch x"});
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend(tool_message("msg_1", "toolu_touch", "Bash", &input));
    steps.extend([
        can_use_tool("perm-1", "Bash", &input, "toolu_touch"),
        json!({"sleep_ms": 300}),
        emit(json!({"type": "control_cancel_request", "request_id": "perm-1"})),
        tool_result("toolu_touch", json!("cancelled"), true),
        json!({"sleep_ms": 300}),
    ]);
    steps.extend(reply("msg_2", "stopped"));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let (_, approval, _) = next_approval(&mut client).await;
    let events = client
        .until(|kind| matches!(kind, EventKind::ApprovalResolved { .. }))
        .await;
    assert!(resolved(&events, &approval).is_some());
    answer(
        &mut client,
        &thread,
        &approval,
        json!({"kind": "allow_once"}),
    )
    .await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(responses_to(&fixture, "perm-1").is_empty());
}

#[tokio::test]
async fn an_interrupt_withdraws_the_pending_request_before_the_turn_ends() {
    // 2.1.322 cancels the request before it answers the interrupt.
    for cancels_before_interrupt_response in [true, false] {
        let fixture = Fixture::new();
        let input = json!({"command": "touch x"});
        let mut steps = handshake();
        steps.push(user_message("go"));
        steps.extend(tool_message("msg_1", "toolu_touch", "Bash", &input));
        steps.extend([
            can_use_tool("perm-1", "Bash", &input, "toolu_touch"),
            expect(json!({"type": "control_request", "request": {"subtype": "interrupt"}})),
        ]);
        if cancels_before_interrupt_response {
            steps.push(emit(
                json!({"type": "control_cancel_request", "request_id": "perm-1"}),
            ));
        }
        steps.extend([
            emit(json!({"type": "control_response", "response": {
                "subtype": "success", "request_id": "$id", "response": {"still_queued": []}}})),
            tool_result(
                "toolu_touch",
                json!("The user doesn't want to proceed with this tool use."),
                true,
            ),
            frame(
                "result",
                json!({"subtype": "error_during_execution", "is_error": true, "result": null,
                    "errors": [], "terminal_reason": "aborted_tools"}),
            ),
        ]);
        fixture.script(steps);

        let mut client = fixture.client();
        let opened = open(&mut client, &fixture, None).await;
        let thread = opened.thread_id.0.clone();
        submit(&mut client, &thread, "go", "follow_up").await;
        let (seen, approval, _) = next_approval(&mut client).await;
        let turn = seen
            .iter()
            .find_map(|e| match &e.event.event {
                EventKind::TurnStarted { turn } => Some(turn.id.clone()),
                _ => None,
            })
            .unwrap();
        client.ok("session/submit", interrupt(&thread, &turn)).await;
        let events = client.until(is_completed).await;
        assert_eq!(completed_status(&events), TurnStatus::Interrupted);
        let withdrawn = resolved(&events, &approval).expect("withdrawn");
        assert!(
            withdrawn < events.len() - 1,
            "cancels before the interrupt response: {cancels_before_interrupt_response}"
        );
        assert_script_followed(&fixture);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(responses_to(&fixture, "perm-1").is_empty());
    }
}

#[tokio::test]
async fn a_claude_that_exits_with_a_request_pending_withdraws_it() {
    let fixture = Fixture::new();
    let input = json!({"command": "touch x"});
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend(tool_message("msg_1", "toolu_touch", "Bash", &input));
    steps.extend([
        can_use_tool("perm-1", "Bash", &input, "toolu_touch"),
        json!({"sleep_ms": 300}),
        json!({"exit": 1}),
    ]);
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let (_, approval, _) = next_approval(&mut client).await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Failed);
    assert!(resolved(&events, &approval).is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn two_clients_allowing_the_same_request_at_once_write_one_answer() {
    let fixture = Fixture::new();
    let input = json!({"command": "touch approved.txt"});
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend(tool_message("msg_1", "toolu_touch", "Bash", &input));
    steps.extend([
        can_use_tool("perm-1", "Bash", &input, "toolu_touch"),
        expect_permission_answer(
            "perm-1",
            json!({"behavior": "allow", "updatedInput": input}),
        ),
        tool_result("toolu_touch", json!(""), false),
    ]);
    steps.extend(reply("msg_2", "created"));
    fixture.script(steps);

    let socket_dir = tempfile::Builder::new()
        .prefix("tc-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = socket_dir.path().join("app.sock");
    let _daemon = fixture.daemon(&socket);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while tokio::net::UnixStream::connect(&socket).await.is_err() {
        assert!(std::time::Instant::now() < deadline, "no daemon socket");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let mut first = fixture.connect(&socket);
    let mut second = fixture.connect(&socket);
    let opened = open(&mut first, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    let attached = snapshot(
        second
            .ok("session/open", json!({"path": opened.path}))
            .await,
    );
    assert_eq!(attached.thread_id, opened.thread_id);
    submit(&mut first, &thread, "go", "follow_up").await;
    let (_, approval, _) = next_approval(&mut first).await;
    let (_, seen_by_second, _) = next_approval(&mut second).await;
    assert_eq!(approval, seen_by_second);
    let allow = json!({"kind": "allow_once"});
    tokio::join!(
        answer(&mut first, &thread, &approval, allow.clone()),
        answer(&mut second, &thread, &approval, allow),
    );
    let events = first.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(responses_to(&fixture, "perm-1").len(), 1);
    assert_script_followed(&fixture);
}

fn from_subagent(agent: &str, spawning_call: &str, step: Value) -> Value {
    let mut step = step;
    step["emit"]["parent_tool_use_id"] = json!(spawning_call);
    step["emit"]["agent_id"] = json!(agent);
    step
}

fn child_message(agent: &str, call: &str, msg: &str, block: Value) -> Value {
    from_subagent(
        agent,
        call,
        frame(
            "assistant",
            json!({"message": {"id": msg, "role": "assistant", "content": [block]}}),
        ),
    )
}

fn child_tool_use(
    agent: &str,
    call: &str,
    msg: &str,
    id: &str,
    name: &str,
    input: &Value,
) -> Value {
    child_message(
        agent,
        call,
        msg,
        json!({"type": "tool_use", "id": id, "name": name, "input": input}),
    )
}

fn child_text(agent: &str, call: &str, msg: &str, text: &str) -> Value {
    child_message(agent, call, msg, json!({"type": "text", "text": text}))
}

fn child_tool_result(agent: &str, call: &str, id: &str, content: &str) -> Value {
    from_subagent(agent, call, tool_result(id, json!(content), false))
}

fn task_started(agent: &str, call: &str, background: bool) -> Value {
    system(
        "task_started",
        json!({"task_id": agent, "tool_use_id": call, "description": "Touch a file",
            "subagent_type": "general-purpose", "is_backgrounded": background,
            "task_type": "local_agent"}),
    )
}

/// The end of a sub-agent as 2.1.322 reports it: the task's new status, then its summary.
fn task_ended(agent: &str, call: &str, status: &str, summary: &str) -> Vec<Value> {
    vec![
        system(
            "task_updated",
            json!({"task_id": agent, "patch": {"status": status}}),
        ),
        system(
            "task_notification",
            json!({"task_id": agent, "tool_use_id": call, "status": status, "summary": summary}),
        ),
    ]
}

fn child_can_use_tool(request_id: &str, agent: &str, input: &Value, tool_use_id: &str) -> Value {
    let mut step = can_use_tool(request_id, "Bash", input, tool_use_id);
    step["emit"]["request"]["agent_id"] = json!(agent);
    step
}

fn background_spawn(agent: &str, call: &str) -> Vec<Value> {
    let mut steps = vec![system("init", json!({}))];
    steps.extend(tool_message(
        "msg_1",
        call,
        "Agent",
        &json!({"description": "Touch a file", "subagent_type": "general-purpose",
            "prompt": "touch bg.txt", "run_in_background": true}),
    ));
    steps.extend(message_end("msg_1", usage_json(1, 1, 0, 0), "tool_use"));
    steps.push(task_started(agent, call, true));
    steps.push(tool_result(
        call,
        json!("Async agent launched successfully."),
        false,
    ));
    steps.extend(reply("msg_2", "spawned"));
    steps
}

fn subagent_items(events: &[SessionEvent]) -> Vec<SubagentItem> {
    custom_items(events, SubagentItem::from_item)
}

fn subagent_states(events: &[SessionEvent]) -> Vec<SubagentStatus> {
    custom_items(events, SubagentStatus::from_item)
}

fn custom_items<T>(events: &[SessionEvent], parse: impl Fn(&Item) -> Option<T>) -> Vec<T> {
    events
        .iter()
        .filter_map(|e| match &e.event.event {
            EventKind::ItemCompleted { item, .. } => parse(item),
            _ => None,
        })
        .collect()
}

fn states_of(statuses: &[SubagentStatus], agent: &str) -> Vec<(SubagentState, Option<String>)> {
    statuses
        .iter()
        .filter(|status| status.agent_id == agent)
        .map(|status| (status.state, status.message.clone()))
        .collect()
}

fn markers(events: &[SessionEvent]) -> Vec<DerivedTurnMarker> {
    custom_items(events, DerivedTurnMarker::from_item)
}

fn turns_completed(events: &[SessionEvent]) -> Vec<tiechui_protocol::Turn> {
    events
        .iter()
        .filter_map(|e| match &e.event.event {
            EventKind::TurnCompleted { turn } => Some(turn.clone()),
            _ => None,
        })
        .collect()
}

#[derive(Debug, PartialEq)]
struct TurnShape {
    derived: Option<DerivedReason>,
    status: TurnStatus,
    subagent_item_ids: Vec<String>,
}

fn structure<'a>(events: impl Iterator<Item = &'a tiechui_protocol::Event>) -> Vec<TurnShape> {
    let mut turns: Vec<(TurnId, TurnStatus, Vec<Item>)> = vec![];
    for event in events {
        match &event.event {
            EventKind::ThreadStarted { thread, .. } => {
                turns = thread
                    .turns
                    .iter()
                    .map(|t| (t.id.clone(), t.status, t.items.clone()))
                    .collect();
            }
            EventKind::TurnStarted { turn } => {
                turns.push((turn.id.clone(), TurnStatus::InProgress, vec![]));
            }
            EventKind::ItemCompleted { turn_id, item } => {
                let turn = turns.iter_mut().find(|t| t.0 == *turn_id).unwrap();
                turn.2.push(item.clone());
            }
            EventKind::TurnCompleted { turn } => {
                let slot = turns.iter_mut().find(|t| t.0 == turn.id).unwrap();
                *slot = (turn.id.clone(), turn.status, turn.items.clone());
            }
            _ => {}
        }
    }
    turns
        .into_iter()
        .map(|(_, status, items)| {
            let mut ids: Vec<String> = vec![];
            for item in &items {
                if let Item::Custom { kind, payload } = item {
                    let id = format!("{kind}:{}", payload["id"]);
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            }
            TurnShape {
                derived: items
                    .first()
                    .and_then(DerivedTurnMarker::from_item)
                    .map(|marker| marker.reason),
                status,
                subagent_item_ids: ids,
            }
        })
        .collect()
}

#[tokio::test]
async fn a_recorded_foreground_subagent_folds_under_its_agent_call() {
    const AGENT: &str = "a0d05e7f5294d18a0";
    const SPAWN: &str = "toolu_017Umw5jiSeU16kfK4MGLL91";
    const BASH: &str = "toolu_01MZ1aHnrFuDiJiuQtFV6chq";
    let fixture = Fixture::new();
    fixture.script(recorded_script(include_str!(
        "fixtures/claude-subagent-foreground.ndjson"
    )));
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("manual")).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let (before, approval, request) = next_approval(&mut client).await;
    let child_call = format!("{AGENT}:{BASH}");
    let ApprovalRequest::Permission {
        actor,
        call_id,
        values,
        ..
    } = request
    else {
        panic!("{request:?}");
    };
    assert_eq!(actor.as_deref(), Some(AGENT));
    assert_eq!(call_id, child_call);
    assert_eq!(values, ["touch sub.txt"]);
    assert!(
        subagent_items(&before)
            .iter()
            .any(|child| child.id.0 == child_call
                && matches!(&child.item, KnownItem::ToolCall { name, .. } if name == "bash")),
        "the child step the card waits on exists before it"
    );
    answer(
        &mut client,
        &thread,
        &approval,
        json!({"kind": "allow_once"}),
    )
    .await;
    let after = client.until(is_completed).await;
    let events: Vec<SessionEvent> = before.into_iter().chain(after).collect();
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));
    assert!(
        markers(&events).is_empty(),
        "everything stays in the root turn"
    );
    assert_eq!(turns_completed(&events).len(), 1);

    let items = known(&events);
    assert!(items.iter().any(|item| matches!(item,
        KnownItem::ToolCall { id, name, .. } if id.0 == SPAWN && name == "Agent")));
    assert!(items.iter().any(|item| matches!(item,
        KnownItem::ToolResult { call_id, .. } if call_id.0 == SPAWN)));
    let children = subagent_items(&events);
    assert!(
        children
            .iter()
            .all(|child| child.agent_id == AGENT && child.parent_call_id.0 == SPAWN),
        "{children:?}"
    );
    let ids: Vec<&str> = children.iter().map(|child| child.id.0.as_str()).collect();
    assert_eq!(ids, [child_call.clone(), format!("{child_call}:result")]);
    assert!(
        !serde_json::to_string(&children)
            .unwrap()
            .contains("Your only job"),
        "the prompt Claude gave the sub-agent is not a step"
    );
    let statuses = subagent_states(&events);
    assert!(
        statuses
            .iter()
            .all(|status| status.parent_call_id.0 == SPAWN
                && status.name.as_deref() == Some("general-purpose")),
        "{statuses:?}"
    );
    assert_eq!(
        states_of(&statuses, AGENT),
        [
            (SubagentState::Running, None),
            (SubagentState::Waiting, None),
            (SubagentState::Running, None),
            (SubagentState::Completed, Some("sub done".to_owned())),
        ]
    );
    assert_script_followed(&fixture);
    assert_eq!(responses_to(&fixture, "perm-1").len(), 1);
}

#[tokio::test]
async fn a_recorded_background_subagent_asks_after_the_turn_and_claude_then_starts_a_turn() {
    const AGENT: &str = "a7f166598edc427a7";
    const SPAWN: &str = "toolu_01J3nzKk11UiMAwxacu8vxeQ";
    const BASH: &str = "toolu_01SF2XZpjY3cCQfB9iqnoPvx";
    let fixture = Fixture::new();
    fixture.script(recorded_script(include_str!(
        "fixtures/claude-subagent-background.ndjson"
    )));
    fixture.script_for_launch(1, handshake());
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("manual")).await;
    let thread = opened.thread_id.0.clone();
    let mut live: Vec<SessionEvent> = vec![];
    submit(&mut client, &thread, "go", "follow_up").await;
    let root = client.until(is_completed).await;
    assert_eq!(completed_status(&root), TurnStatus::Completed);
    let root_turn = turns_completed(&root)[0].id.clone();
    live.extend(root);

    let (seen, approval, request) = next_approval(&mut client).await;
    let child_call = format!("{AGENT}:{BASH}");
    let derived = seen
        .iter()
        .find_map(|e| match &e.event.event {
            EventKind::TurnStarted { turn } => Some(turn.id.clone()),
            _ => None,
        })
        .expect("the sub-agent's output opens a derived turn");
    assert!(
        seen.iter().any(|e| matches!(&e.event.event,
            EventKind::ApprovalRequested { turn_id: Some(turn), .. } if *turn == derived)),
        "the approval sits in the derived turn"
    );
    let ApprovalRequest::Permission { actor, call_id, .. } = request else {
        panic!("{request:?}");
    };
    assert_eq!(actor.as_deref(), Some(AGENT));
    assert_eq!(call_id, child_call);
    let marker = &markers(&seen)[0];
    assert_eq!(
        (marker.derived_from.clone(), marker.reason),
        (root_turn.clone(), DerivedReason::Subagent)
    );
    live.extend(seen);
    answer(
        &mut client,
        &thread,
        &approval,
        json!({"kind": "allow_once"}),
    )
    .await;
    let closed = client.until(is_completed).await;
    let turn = turns_completed(&closed).pop().unwrap();
    assert_eq!(
        (turn.id.clone(), turn.status),
        (derived.clone(), TurnStatus::Completed),
        "Idle closes the derived turn once the sub-agent is done"
    );
    live.extend(closed);
    let own = client.until(is_completed).await;
    let marker = markers(&own)
        .pop()
        .expect("Claude's own turn is a derived turn");
    assert_eq!(marker.reason, DerivedReason::Runtime);
    assert_eq!(completed_status(&own), TurnStatus::Completed);
    assert!(known(&own).iter().any(|item| matches!(item,
        KnownItem::AssistantMessage { text, .. } if text.contains("bg done"))));
    live.extend(own);

    assert!(errors(&live).is_empty(), "{:?}", errors(&live));
    assert_script_followed(&fixture);
    assert_eq!(responses_to(&fixture, "perm-1").len(), 1);
    assert_eq!(fixture.launches().len(), 1, "the process was never reaped");
    let children = subagent_items(&live);
    let ids: Vec<&str> = children.iter().map(|child| child.id.0.as_str()).collect();
    assert_eq!(
        ids[..2],
        [child_call.clone(), format!("{child_call}:result")]
    );
    assert!(children.iter().any(|child| matches!(&child.item,
        KnownItem::AssistantMessage { text, .. } if text == "bg done")));
    assert!(children.iter().all(|child| child.parent_call_id.0 == SPAWN));
    assert_eq!(
        states_of(&subagent_states(&live), AGENT),
        [
            (SubagentState::Running, None),
            (SubagentState::Waiting, None),
            (SubagentState::Running, None),
            (SubagentState::Completed, Some("bg done".to_owned())),
        ]
    );

    let shape: Vec<_> = structure(opened.events.iter().chain(live.iter().map(|e| &e.event)));
    assert_eq!(
        shape
            .iter()
            .map(|turn| (turn.derived, turn.status))
            .collect::<Vec<_>>(),
        [
            (None, TurnStatus::Completed),
            (Some(DerivedReason::Subagent), TurnStatus::Completed),
            (Some(DerivedReason::Runtime), TurnStatus::Completed),
        ]
    );
    client
        .ok("session/close", json!({"thread_id": thread}))
        .await;
    let mut client = fixture.client();
    let reopened = snapshot(
        client
            .ok("session/open", json!({"path": opened.path}))
            .await,
    );
    assert_eq!(
        structure(reopened.events.iter()),
        shape,
        "reopening rebuilds the fold and the derived turns"
    );
}

#[tokio::test]
async fn a_nested_subagent_hangs_under_the_child_call_that_spawned_it() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.push(system("init", json!({})));
    steps.extend(tool_message(
        "msg_1",
        "toolu_agent",
        "Agent",
        &json!({"description": "outer", "subagent_type": "general-purpose", "prompt": "do"}),
    ));
    steps.extend(message_end("msg_1", usage_json(1, 1, 0, 0), "tool_use"));
    steps.push(task_started("a1", "toolu_agent", false));
    steps.push(from_subagent(
        "a1",
        "toolu_agent",
        frame(
            "user",
            json!({"message": {"role": "user", "content": [{"type": "text", "text": "do"}]}}),
        ),
    ));
    steps.push(child_message(
        "a1",
        "toolu_agent",
        "msg_c1",
        json!({"type": "thinking", "thinking": "plan it", "signature": ""}),
    ));
    steps.push(child_tool_use(
        "a1",
        "toolu_agent",
        "msg_c1",
        "toolu_inner",
        "Agent",
        &json!({"description": "inner", "subagent_type": "Explore", "prompt": "look"}),
    ));
    steps.push(system(
        "task_started",
        json!({"task_id": "a2", "tool_use_id": "toolu_inner", "description": "inner",
            "subagent_type": "Explore", "is_backgrounded": false, "task_type": "local_agent"}),
    ));
    let mut child_stream_event = stream(
        "msg_c2",
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "streamed"}}),
    );
    child_stream_event["emit"]["parent_tool_use_id"] = json!("toolu_inner");
    steps.push(child_stream_event);
    steps.push(child_text("a2", "toolu_inner", "msg_c2", "nested words"));
    steps.extend(task_ended("a2", "toolu_inner", "completed", "nested words"));
    steps.push(child_tool_result(
        "a1",
        "toolu_agent",
        "toolu_inner",
        "nested words",
    ));
    // A background shell command is a task too, but no sub-agent.
    steps.push(system(
        "task_started",
        json!({"task_id": "b1", "tool_use_id": "toolu_shell", "description": "sleep",
            "is_backgrounded": true, "task_type": "local_bash"}),
    ));
    steps.extend(task_ended("a1", "toolu_agent", "completed", "outer done"));
    steps.push(tool_result("toolu_agent", json!("outer done"), false));
    steps.extend(reply("msg_2", "parent words"));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    let children = subagent_items(&events);
    let shape: Vec<(&str, &str, &str)> = children
        .iter()
        .map(|c| {
            (
                c.agent_id.as_str(),
                c.parent_call_id.0.as_str(),
                c.id.0.as_str(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("a1", "toolu_agent", "a1:msg_c1:0"),
            ("a1", "toolu_agent", "a1:toolu_inner"),
            ("a2", "a1:toolu_inner", "a2:msg_c2:0"),
            ("a1", "toolu_agent", "a1:toolu_inner:result"),
        ]
    );
    assert!(matches!(&children[0].item, KnownItem::Thinking { text, .. } if text == "plan it"));
    assert!(matches!(&children[1].item, KnownItem::ToolCall { name, .. } if name == "Agent"));
    let text = serde_json::to_string(&children).unwrap();
    assert!(!text.contains("streamed"), "{text}");
    let statuses = subagent_states(&events);
    assert!(statuses.iter().all(|s| s.agent_id != "b1"), "{statuses:?}");
    let inner: Vec<_> = statuses.iter().filter(|s| s.agent_id == "a2").collect();
    assert!(inner.iter().all(|s| s.parent_call_id.0 == "a1:toolu_inner"
        && s.name.as_deref() == Some("Explore")));
    assert_eq!(
        states_of(&statuses, "a2"),
        [
            (SubagentState::Running, None),
            (SubagentState::Completed, Some("nested words".to_owned())),
        ]
    );
    assert_eq!(
        states_of(&statuses, "a1").last().unwrap(),
        &(SubagentState::Completed, Some("outer done".to_owned()))
    );
}

#[tokio::test]
async fn a_turn_claude_starts_itself_while_a_subagent_waits_leaves_the_approval_open() {
    let fixture = Fixture::new();
    let input = json!({"command": "touch bg.txt"});
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend(background_spawn("a1", "toolu_agent"));
    steps.extend([
        child_tool_use("a1", "toolu_agent", "msg_c1", "toolu_bg", "Bash", &input),
        child_can_use_tool("perm-1", "a1", &input, "toolu_bg"),
        json!({"sleep_ms": 200}),
        // A background shell command finished: Claude runs a turn of its own.
        system("init", json!({})),
        message_start("msg_3"),
    ]);
    steps.extend(streamed_block("msg_3", 0, "text", &["noticed"]));
    steps.extend(message_end("msg_3", usage_json(1, 1, 0, 0), "end_turn"));
    steps.extend([
        result_ok("noticed"),
        expect_permission_answer(
            "perm-1",
            json!({"behavior": "allow", "updatedInput": input}),
        ),
        child_tool_result(
            "a1",
            "toolu_agent",
            "toolu_bg",
            "(Bash completed with no output)",
        ),
    ]);
    steps.extend(task_ended("a1", "toolu_agent", "completed", "bg done"));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("manual")).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    client.until(is_completed).await;
    let (_, approval, _) = next_approval(&mut client).await;
    let noticed = client
        .until(|kind| {
            matches!(kind, EventKind::ItemCompleted { item: Item::Known(
            KnownItem::AssistantMessage { text, .. }), .. } if text == "noticed")
        })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let snap = snapshot(
        client
            .ok("session/snapshot", json!({"thread_id": thread}))
            .await,
    );
    let shape = structure(snap.events.iter());
    assert_eq!(
        shape.last().map(|turn| (turn.derived, turn.status)),
        Some((Some(DerivedReason::Subagent), TurnStatus::InProgress)),
        "Claude's own turn ended inside the derived turn, which stays open: {shape:?}"
    );
    assert!(resolved(&noticed, &approval).is_none());
    answer(
        &mut client,
        &thread,
        &approval,
        json!({"kind": "allow_once"}),
    )
    .await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));
    assert_eq!(
        states_of(&subagent_states(&events), "a1").last().unwrap(),
        &(SubagentState::Completed, Some("bg done".to_owned()))
    );
    assert_script_followed(&fixture);
    assert_eq!(responses_to(&fixture, "perm-1").len(), 1);
}

#[tokio::test]
async fn a_result_with_no_turn_running_changes_nothing() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("first"));
    steps.extend(simple_turn("msg_1", "one", usage_json(1, 1, 0, 0)));
    steps.push(result_ok("stray"));
    steps.push(user_message("second"));
    steps.extend(simple_turn("msg_2", "two", usage_json(1, 1, 0, 0)));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "first", "follow_up").await;
    client.until(is_completed).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    submit(&mut client, &thread, "second", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert!(known(&events).iter().any(|item| matches!(item,
        KnownItem::AssistantMessage { text, .. } if text == "two")));
    assert!(markers(&events).is_empty(), "no derived turn: {events:?}");
    assert_eq!(turns_completed(&events).len(), 1);
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));
}

#[tokio::test]
async fn a_claude_that_exits_while_a_subagent_waits_withdraws_the_approval_and_interrupts_it() {
    let fixture = Fixture::new();
    let input = json!({"command": "touch bg.txt"});
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend(background_spawn("a1", "toolu_agent"));
    steps.extend([
        child_tool_use("a1", "toolu_agent", "msg_c1", "toolu_bg", "Bash", &input),
        child_can_use_tool("perm-1", "a1", &input, "toolu_bg"),
        json!({"sleep_ms": 300}),
        json!({"exit": 1}),
    ]);
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("manual")).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    client.until(is_completed).await;
    let (_, approval, _) = next_approval(&mut client).await;
    let events = client.until(is_completed).await;
    assert!(resolved(&events, &approval).is_some());
    assert_eq!(
        states_of(&subagent_states(&events), "a1").last().unwrap().0,
        SubagentState::Interrupted
    );
    let unfinished: Vec<_> = subagent_items(&events)
        .into_iter()
        .filter(|c| matches!(&c.item, KnownItem::ToolResult { is_error: true, .. }))
        .collect();
    assert_eq!(unfinished.len(), 1, "the waiting call is settled");
    assert_eq!(completed_status(&events), TurnStatus::Completed);
}

#[cfg(unix)]
#[tokio::test]
async fn closing_the_session_while_a_subagent_waits_withdraws_the_approval_and_leaves_no_process() {
    let fixture = Fixture::new();
    let (pid, kid) = (fixture.path("fake.pid"), fixture.path("child.pid"));
    let input = json!({"command": "touch bg.txt"});
    let mut steps = vec![json!({"self_pid": pid}), json!({"grandchild": kid})];
    steps.extend(handshake());
    steps.push(user_message("go"));
    steps.extend(background_spawn("a1", "toolu_agent"));
    steps.extend([
        child_tool_use("a1", "toolu_agent", "msg_c1", "toolu_bg", "Bash", &input),
        child_can_use_tool("perm-1", "a1", &input, "toolu_bg"),
        json!({"hang": true}),
    ]);
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("manual")).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    client.until(is_completed).await;
    let (_, approval, _) = next_approval(&mut client).await;
    let (fake, grandchild) = (pid_in(&pid), pid_in(&kid));
    client
        .ok("session/close", json!({"thread_id": opened.thread_id}))
        .await;
    let events = client
        .until(|kind| matches!(kind, EventKind::ApprovalResolved { .. }))
        .await;
    assert!(resolved(&events, &approval).is_some());
    assert!(gone_within(fake, Duration::from_secs(6)).await);
    assert!(gone_within(grandchild, Duration::from_secs(6)).await);
    let status = client.finish().await;
    assert!(status.success(), "{status:?}");
}

#[tokio::test]
async fn a_new_session_launches_with_its_id_and_flags_and_saves_the_id_as_the_thread() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend(simple_turn("msg_1", "ok", usage_json(1, 1, 0, 0)));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = snapshot(
        client
            .ok(
                "session/open",
                json!({"cwd": fixture.cwd, "runtime": "claude", "model": "haiku",
                    "thinking": "high", "mode": "yolo"}),
            )
            .await,
    );
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    client.until(is_completed).await;
    let launches = fixture.launches();
    assert_eq!(launches.len(), 1);
    let argv = &launches[0];
    for flag in ["-p", "--verbose", "--include-partial-messages"] {
        assert!(argv.iter().any(|arg| arg == flag), "{flag} in {argv:?}");
    }
    assert_eq!(arg_after(argv, "--input-format"), Some("stream-json"));
    assert_eq!(arg_after(argv, "--output-format"), Some("stream-json"));
    assert_eq!(arg_after(argv, "--permission-prompt-tool"), Some("stdio"));
    assert_eq!(
        arg_after(argv, "--permission-mode"),
        Some("bypassPermissions")
    );
    assert_eq!(arg_after(argv, "--thinking-display"), Some("summarized"));
    assert_eq!(arg_after(argv, "--model"), Some("haiku"));
    assert_eq!(arg_after(argv, "--effort"), Some("high"));
    assert!(!argv.iter().any(|arg| arg == "--resume"), "{argv:?}");
    let id = arg_after(argv, "--session-id").unwrap();
    assert_eq!(id.len(), 36, "{id}");
    assert_eq!(saved_claude_session_id(&opened.path), id);
}

#[tokio::test]
async fn permission_modes_and_thinking_levels_map_to_flags() {
    for (mode, expected) in [
        (None, "manual"),
        (Some("rules"), "manual"),
        (Some("auto"), "auto"),
        (Some("yolo"), "bypassPermissions"),
    ] {
        let fixture = Fixture::new();
        fixture.script(handshake());
        let mut client = fixture.client();
        open(&mut client, &fixture, mode).await;
        let launches = fixture.launches();
        assert_eq!(
            arg_after(&launches[0], "--permission-mode"),
            Some(expected),
            "{mode:?}"
        );
        assert_eq!(arg_after(&launches[0], "--effort"), None);
        assert_eq!(arg_after(&launches[0], "--model"), None);
    }
    for (level, effort) in [
        ("minimal", "low"),
        ("low", "low"),
        ("medium", "medium"),
        ("xhigh", "xhigh"),
        ("max", "max"),
    ] {
        let fixture = Fixture::new();
        fixture.script(handshake());
        let mut client = fixture.client();
        client
            .ok(
                "session/open",
                json!({"cwd": fixture.cwd, "runtime": "claude", "thinking": level}),
            )
            .await;
        let launches = fixture.launches();
        assert_eq!(arg_after(&launches[0], "--effort"), Some(effort), "{level}");
    }
}

/// `--thinking disabled` cannot be undone during the session, a zero thinking-token cap can:
/// `off` is the cap, set right after `initialize`.
#[tokio::test]
async fn thinking_off_is_a_thinking_token_cap_sent_after_initialize_not_a_flag() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.extend(answered(json!({"subtype": "set_max_thinking_tokens",
        "max_thinking_tokens": 0})));
    steps.push(user_message("go"));
    steps.extend(simple_turn("msg_1", "ok", usage_json(1, 1, 0, 0)));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = snapshot(
        client
            .ok(
                "session/open",
                json!({"cwd": fixture.cwd, "runtime": "claude", "thinking": "off"}),
            )
            .await,
    );
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    client.until(is_completed).await;
    let argv = &fixture.launches()[0];
    for flag in ["--thinking", "--effort"] {
        assert!(!argv.iter().any(|arg| arg == flag), "{flag} in {argv:?}");
    }
    assert_script_followed(&fixture);
}

#[tokio::test]
async fn reopening_resumes_the_same_session_and_usage_is_not_counted_twice() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("remember 4217"));
    steps.extend(simple_turn("msg_1", "noted", usage_json(2, 7, 100, 50)));
    fixture.script(steps);
    let (path, id) = {
        let mut client = fixture.client();
        let opened = open(&mut client, &fixture, None).await;
        submit(
            &mut client,
            &opened.thread_id.0,
            "remember 4217",
            "follow_up",
        )
        .await;
        let events = client.until(is_completed).await;
        assert_eq!(usages(&events).len(), 1);
        client
            .ok("session/close", json!({"thread_id": opened.thread_id}))
            .await;
        let id = saved_claude_session_id(&opened.path);
        (opened.path, id)
    };
    let msg_1_usage_before_input = message_end("msg_1", usage_json(2, 7, 100, 50), "end_turn");
    let mut steps = handshake();
    steps.push(message_start("msg_1"));
    steps.extend(msg_1_usage_before_input);
    steps.push(user_message("what was it"));
    steps.extend(simple_turn("msg_2", "4217", usage_json(1, 3, 200, 0)));
    fixture.script(steps);
    let mut client = fixture.client();
    let reopened = snapshot(client.ok("session/open", json!({"path": path})).await);
    let launches = fixture.launches();
    assert_eq!(launches.len(), 2);
    assert_eq!(arg_after(&launches[1], "--resume"), Some(id.as_str()));
    assert!(
        !launches[1].iter().any(|arg| arg == "--session-id"),
        "{:?}",
        launches[1]
    );
    assert!(
        event_errors(reopened.events.iter()).is_empty(),
        "{:?}",
        event_errors(reopened.events.iter())
    );
    submit(
        &mut client,
        &reopened.thread_id.0,
        "what was it",
        "follow_up",
    )
    .await;
    let events = client.until(is_completed).await;
    let used = usages(&events);
    assert_eq!(used.len(), 1, "msg_1 was already counted: {used:?}");
    assert_eq!(used[0].output, 3);
    let EventKind::TokenUsageUpdated { total, .. } = events
        .iter()
        .rev()
        .find(|e| matches!(e.event.event, EventKind::TokenUsageUpdated { .. }))
        .unwrap()
        .event
        .event
        .clone()
    else {
        unreachable!()
    };
    assert_eq!((total.output, total.cache_read), (10, 300), "{total:?}");
    assert_eq!(saved_claude_session_id(&path), id, "resume keeps the id");
    assert_script_followed(&fixture);
}

#[tokio::test]
async fn a_failed_resume_starts_a_new_session_and_says_so() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("hello"));
    steps.extend(simple_turn("msg_1", "hi", usage_json(1, 1, 0, 0)));
    fixture.script(steps);
    let (path, old) = {
        let mut client = fixture.client();
        let opened = open(&mut client, &fixture, None).await;
        submit(&mut client, &opened.thread_id.0, "hello", "follow_up").await;
        client.until(is_completed).await;
        client
            .ok("session/close", json!({"thread_id": opened.thread_id}))
            .await;
        let old = saved_claude_session_id(&opened.path);
        (opened.path, old)
    };
    fixture.script_for_launch(
        1,
        vec![
            expect(json!({"type": "control_request", "request": {"subtype": "initialize"}})),
            result_error(
                "error_during_execution",
                &["No conversation found with session ID: x"],
            ),
            json!({"stderr": "No conversation found with session ID: x"}),
            json!({"exit": 1}),
        ],
    );
    let mut steps = handshake();
    steps.push(user_message("again"));
    steps.extend(simple_turn("msg_9", "fresh", usage_json(1, 1, 0, 0)));
    fixture.script_for_launch(2, steps);
    let mut client = fixture.client();
    let reopened = snapshot(client.ok("session/open", json!({"path": path})).await);
    assert!(
        reopened.events.iter().any(|e| matches!(&e.event,
            EventKind::Error { message } if message.contains("could not resume"))),
        "{:?}",
        reopened.events
    );
    let launches = fixture.launches();
    assert_eq!(launches.len(), 3, "{launches:?}");
    assert_eq!(arg_after(&launches[1], "--resume"), Some(old.as_str()));
    let fresh = arg_after(&launches[2], "--session-id").unwrap();
    assert_ne!(fresh, old);
    assert_eq!(saved_claude_session_id(&path), fresh);
    submit(&mut client, &reopened.thread_id.0, "again", "follow_up").await;
    client.until(is_completed).await;
}

#[tokio::test]
async fn a_resume_that_finds_nothing_after_the_process_died_starts_anew_under_the_same_id() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.push(json!({"exit": 1}));
    fixture.script_for_launch(0, steps);
    fixture.script_for_launch(
        1,
        vec![
            expect(json!({"type": "control_request", "request": {"subtype": "initialize"}})),
            result_error(
                "error_during_execution",
                &["No conversation found with session ID: x"],
            ),
            json!({"exit": 1}),
        ],
    );
    let mut steps = handshake();
    steps.push(user_message("again"));
    steps.extend(simple_turn("msg_1", "fresh", usage_json(1, 1, 0, 0)));
    fixture.script_for_launch(2, steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Failed);
    submit(&mut client, &thread, "again", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    let launches = fixture.launches();
    assert_eq!(launches.len(), 3, "{launches:?}");
    let id = arg_after(&launches[0], "--session-id").unwrap();
    assert_eq!(arg_after(&launches[1], "--resume"), Some(id));
    assert_eq!(arg_after(&launches[2], "--session-id"), Some(id));
    assert_eq!(saved_claude_session_id(&opened.path), id);
    assert_script_followed(&fixture);
}

#[tokio::test]
async fn a_message_written_during_a_turn_joins_it_and_does_not_start_another() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("first"));
    steps.extend([
        system("init", json!({})),
        message_start("msg_1"),
        user_message("also this"),
    ]);
    steps.extend(streamed_block("msg_1", 0, "text", &["both"]));
    steps.extend(message_end("msg_1", usage_json(1, 1, 0, 0), "end_turn"));
    steps.push(result_ok("both"));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "first", "follow_up").await;
    client
        .until(|kind| matches!(kind, EventKind::TurnStarted { .. }))
        .await;
    submit(&mut client, &thread, "also this", "steer").await;
    let events = client.until(is_completed).await;
    assert!(
        known(&events).iter().any(|item| matches!(item,
            KnownItem::UserMessage { content, .. } if format!("{content:?}").contains("also this"))),
        "the steered message joined the turn"
    );
    let writes = fixture
        .received()
        .iter()
        .filter(|line| line["type"] == "user")
        .count();
    assert_eq!(writes, 2);
    assert_script_followed(&fixture);
}

fn interrupt(thread: &str, turn: &TurnId) -> Value {
    json!({"thread_id": thread, "op": {"kind": "interrupt", "turn_id": turn}})
}

#[cfg(unix)]
#[tokio::test]
async fn an_interrupt_is_a_control_request_and_the_aborted_result_ends_the_turn_without_a_reap() {
    let fixture = Fixture::new();
    let pid = fixture.path("fake.pid");
    let mut steps = vec![json!({"self_pid": pid})];
    steps.extend(handshake());
    steps.push(user_message("go"));
    steps.extend([
        message_start("msg_1"),
        expect(json!({"type": "control_request", "request": {"subtype": "interrupt"}})),
        emit(json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": "$id", "response": {"still_queued": []}}})),
        frame(
            "result",
            json!({"subtype": "error_during_execution", "is_error": true, "result": null,
                "errors": ["[ede_diagnostic] result_type=user"], "terminal_reason": "aborted_streaming"}),
        ),
        user_message("after"),
    ]);
    steps.extend(simple_turn("msg_2", "after", usage_json(1, 1, 0, 0)));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let started = client
        .until(|kind| matches!(kind, EventKind::TurnStarted { .. }))
        .await;
    let EventKind::TurnStarted { turn } = &started.last().unwrap().event.event else {
        unreachable!()
    };
    client
        .ok("session/submit", interrupt(&thread, &turn.id))
        .await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Interrupted);
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(alive(pid_in(&pid)));
    submit(&mut client, &thread, "after", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert_eq!(fixture.launches().len(), 1);
    assert_script_followed(&fixture);
}

#[tokio::test]
async fn interrupting_a_turn_claude_started_itself_sends_the_interrupt_and_ends_it_promptly() {
    let fixture = Fixture::new();
    let input = json!({"command": "sleep 40"});
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend(simple_turn("msg_1", "one", usage_json(1, 1, 0, 0)));
    // As recorded with 2.1.322: after an interrupted turn Claude acted on the steer in a turn of
    // its own and ran the command again.
    steps.extend([
        json!({"sleep_ms": 200}),
        system("init", json!({})),
        message_start("msg_2"),
    ]);
    steps.extend(tool_use("msg_2", 0, "toolu_long", "Bash", &input));
    steps.extend(message_end("msg_2", usage_json(1, 1, 0, 0), "tool_use"));
    steps.extend([
        expect(json!({"type": "control_request", "request": {"subtype": "interrupt"}})),
        emit(json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": "$id", "response": {"still_queued": []}}})),
        tool_result(
            "toolu_long",
            json!("The user doesn't want to proceed with this tool use."),
            true,
        ),
        frame(
            "result",
            json!({"subtype": "error_during_execution", "is_error": true, "result": null,
                "errors": [], "terminal_reason": "aborted_tools"}),
        ),
        user_message("after"),
    ]);
    steps.extend(simple_turn("msg_3", "after", usage_json(1, 1, 0, 0)));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    client.until(is_completed).await;
    let running = client
        .until(|kind| {
            matches!(
                kind,
                EventKind::ItemCompleted {
                    item: Item::Known(KnownItem::ToolCall { .. }),
                    ..
                }
            )
        })
        .await;
    assert_eq!(
        markers(&running)
            .iter()
            .map(|marker| marker.reason)
            .collect::<Vec<_>>(),
        vec![DerivedReason::Runtime]
    );
    let EventKind::ItemCompleted { turn_id, .. } = &running.last().unwrap().event.event else {
        unreachable!()
    };
    let derived = turn_id.clone();
    let asked = std::time::Instant::now();
    client
        .ok("session/submit", interrupt(&thread, &derived))
        .await;
    let events = client.until(is_completed).await;
    assert!(
        asked.elapsed() < tiechui_runtime::INTERRUPT_WINDOW / 2,
        "{:?}",
        asked.elapsed()
    );
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));
    let ended = turns_completed(&events);
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].id, derived);
    submit(&mut client, &thread, "after", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert_eq!(fixture.launches().len(), 1);
    assert_script_followed(&fixture);
}

#[cfg(unix)]
#[tokio::test]
async fn an_error_result_fails_the_turn_and_the_process_group_is_reaped_within_the_grace() {
    let fixture = Fixture::new();
    let (pid, child) = (fixture.path("fake.pid"), fixture.path("child.pid"));
    let mut steps = vec![json!({"self_pid": pid}), json!({"grandchild": child})];
    steps.extend(handshake());
    steps.push(user_message("go"));
    steps.extend([
        message_start("msg_1"),
        result_error("error_during_execution", &["API Error: 529 overloaded"]),
        json!({"hang": true}),
    ]);
    fixture.script_for_launch(0, steps);
    let mut steps = handshake();
    steps.push(user_message("retry"));
    steps.extend(simple_turn("msg_2", "recovered", usage_json(1, 1, 0, 0)));
    fixture.script_for_launch(1, steps);
    fixture.script(vec![]);
    let mut client = fixture.client_with(&[("CHUI_CLAUDE_REAP_TIMEOUT_MS", "800")]);
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Failed);
    assert!(
        errors(&events).iter().any(|m| m.contains("529 overloaded")),
        "{:?}",
        errors(&events)
    );
    let (fake, grandchild) = (pid_in(&pid), pid_in(&child));
    assert!(alive(fake), "still alive right after the result");
    let began = std::time::Instant::now();
    assert!(gone_within(fake, Duration::from_secs(10)).await, "reaped");
    assert!(
        gone_within(grandchild, Duration::from_secs(10)).await,
        "grandchild reaped"
    );
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "{:?}",
        began.elapsed()
    );
    submit(&mut client, &thread, "retry", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    let launches = fixture.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    let id = saved_claude_session_id(&opened.path);
    assert_eq!(arg_after(&launches[1], "--resume"), Some(id.as_str()));
}

#[tokio::test]
async fn a_follow_up_queued_before_an_error_result_runs_in_a_new_process_that_resumes() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend([
        system("init", json!({})),
        message_start("msg_1"),
        json!({"sleep_ms": 1000}),
        result_error("error_during_execution", &["API Error: 529 overloaded"]),
        json!({"hang": true}),
    ]);
    fixture.script_for_launch(0, steps);
    let mut steps = handshake();
    steps.push(user_message("next"));
    steps.extend(simple_turn("msg_2", "resumed", usage_json(1, 1, 0, 0)));
    fixture.script_for_launch(1, steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    client
        .until(|kind| matches!(kind, EventKind::TurnStarted { .. }))
        .await;
    submit(&mut client, &thread, "next", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Failed);
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    let launches = fixture.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    let id = arg_after(&launches[0], "--session-id").unwrap();
    assert_eq!(arg_after(&launches[1], "--resume"), Some(id));
    assert_script_followed(&fixture);
}

#[cfg(unix)]
#[tokio::test]
async fn after_claude_stops_taking_input_the_next_turn_runs_in_a_new_process_that_resumes() {
    let fixture = Fixture::new();
    let input = json!({"command": "touch a.txt"});
    let mut steps = handshake();
    steps.push(user_message("one"));
    steps.extend(tool_message("msg_1", "toolu_touch", "Bash", &input));
    steps.extend([
        json!({"close_stdin": true}),
        can_use_tool("perm-1", "Bash", &input, "toolu_touch"),
        json!({"sleep_ms": 1000}),
        result_ok("done"),
        json!({"hang": true}),
    ]);
    fixture.script_for_launch(0, steps);
    let mut steps = handshake();
    steps.push(user_message("two"));
    steps.extend(simple_turn("msg_2", "resumed", usage_json(1, 1, 0, 0)));
    fixture.script_for_launch(1, steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "one", "follow_up").await;
    let (_, approval, _) = next_approval(&mut client).await;
    let answer_write_fails = json!({"kind": "allow_once"});
    answer(&mut client, &thread, &approval, answer_write_fails).await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    submit(&mut client, &thread, "two", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    let launches = fixture.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    let id = arg_after(&launches[0], "--session-id").unwrap();
    assert_eq!(arg_after(&launches[1], "--resume"), Some(id));
    assert_script_followed(&fixture);
}

#[cfg(unix)]
#[tokio::test]
async fn closing_the_session_leaves_no_claude_or_grandchild_behind() {
    let fixture = Fixture::new();
    let (pid, child) = (fixture.path("fake.pid"), fixture.path("child.pid"));
    let mut steps = vec![json!({"self_pid": pid}), json!({"grandchild": child})];
    steps.extend(handshake());
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let (fake, grandchild) = (pid_in(&pid), pid_in(&child));
    assert!(alive(fake) && alive(grandchild));
    client
        .ok("session/close", json!({"thread_id": opened.thread_id}))
        .await;
    assert!(gone_within(fake, Duration::from_secs(6)).await);
    assert!(gone_within(grandchild, Duration::from_secs(6)).await);
    let status = client.finish().await;
    assert!(status.success(), "{status:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_claude_that_never_answers_initialize_times_out_and_is_killed() {
    let fixture = Fixture::new();
    let pid = fixture.path("fake.pid");
    fixture.script(vec![
        json!({"self_pid": pid}),
        expect(json!({"type": "control_request"})),
        json!({"hang": true}),
    ]);
    let mut client = fixture.client_with(&[("CHUI_CLAUDE_HANDSHAKE_TIMEOUT_MS", "1500")]);
    let error = client
        .call(
            "session/open",
            json!({"cwd": fixture.cwd, "runtime": "claude"}),
        )
        .await
        .unwrap_err();
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("timed out"), "{message}");
    assert!(gone_within(pid_in(&pid), Duration::from_secs(6)).await);
}

#[tokio::test]
async fn host_environment_variables_do_not_reach_claude() {
    let fixture = Fixture::new();
    fixture.script(handshake());
    let log = fixture.path("env.log");
    let mut client = fixture.client_with(&[
        ("FAKE_CLAUDE_ENV_LOG", log.to_str().unwrap()),
        ("CLAUDECODE", "1"),
        ("CLAUDE_CODE_ENTRYPOINT", "cli"),
        ("CLAUDE_CODE_SSE_PORT", "1234"),
        ("CLAUDE_CODE_SESSION_ID", "outer"),
        ("ELECTRON_RUN_AS_NODE", "1"),
        ("CLAUDE_CONFIG_DIR", "/keep/config"),
        ("ANTHROPIC_API_KEY", "keep-key"),
    ]);
    open(&mut client, &fixture, None).await;
    let env = std::fs::read_to_string(&log).unwrap();
    let names: Vec<&str> = env
        .lines()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name))
        .collect();
    for stripped in [
        "CLAUDECODE",
        "CLAUDE_CODE_ENTRYPOINT",
        "CLAUDE_CODE_SSE_PORT",
        "CLAUDE_CODE_SESSION_ID",
        "ELECTRON_RUN_AS_NODE",
    ] {
        assert!(!names.contains(&stripped), "{stripped} leaked:\n{env}");
    }
    assert!(env.contains("CLAUDE_CONFIG_DIR=/keep/config"), "{env}");
    assert!(env.contains("ANTHROPIC_API_KEY=keep-key"), "{env}");
    assert!(
        env.contains(&format!("HOME={}", fixture.root.path().display())),
        "HOME is left alone:\n{env}"
    );
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[tokio::test]
async fn a_claude_turn_gets_snapshots_and_can_be_undone() {
    let fixture = Fixture::new();
    git(&fixture.cwd, &["init", "-q"]);
    std::fs::create_dir_all(fixture.cwd.join("src")).unwrap();
    std::fs::write(fixture.cwd.join("src/hello.ts"), "original\n").unwrap();
    git(&fixture.cwd, &["add", "."]);
    git(&fixture.cwd, &["commit", "-q", "-m", "init"]);
    let mut steps = handshake();
    steps.push(user_message("edit"));
    steps.push(json!({"write": {"path": "src/hello.ts", "content": "changed by claude\n"}}));
    steps.extend(simple_turn("msg_1", "edited", usage_json(1, 1, 0, 0)));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("yolo")).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "edit", "follow_up").await;
    let events = client.until(is_completed).await;
    let item = events
        .iter()
        .find_map(|e| match &e.event.event {
            EventKind::ItemCompleted {
                item: Item::Known(KnownItem::UserMessage { id, .. }),
                ..
            } => Some(id.0.clone()),
            _ => None,
        })
        .unwrap();
    let at = json!({"thread_id": thread, "item_id": item});
    let rewind = client.ok("turn/rewind_check", at.clone()).await;
    assert_eq!(rewind["mode"], "snapshot", "{rewind}");
    assert!(rewind["snapshot_ref"].as_str().is_some(), "{rewind}");
    let check = client.ok("turn/check", at.clone()).await;
    assert_eq!(check["safe"], json!(["src/hello.ts"]), "{check}");
    client.ok("turn/undo", at).await;
    assert_eq!(
        std::fs::read_to_string(fixture.cwd.join("src/hello.ts")).unwrap(),
        "original\n"
    );
}

fn control(request: Value) -> Value {
    expect(json!({"type": "control_request", "request": request}))
}

fn control_ok(response: Value) -> Value {
    emit(json!({"type": "control_response", "response": {
        "subtype": "success", "request_id": "$id", "response": response}}))
}

fn control_failed(error: &str, code: &str) -> Value {
    emit(json!({"type": "control_response", "response": {
        "subtype": "error", "request_id": "$id", "error": error, "error_code": code}}))
}

fn answered(request: Value) -> Vec<Value> {
    vec![control(request), control_ok(Value::Null)]
}

fn effort(level: &str) -> Vec<Value> {
    answered(json!({"subtype": "apply_flag_settings", "settings": {"effortLevel": level}}))
}

fn thinking_cap(tokens: Value) -> Vec<Value> {
    answered(json!({"subtype": "set_max_thinking_tokens", "max_thinking_tokens": tokens}))
}

fn set_model(model: &str, resolved: &str) -> Vec<Value> {
    vec![
        control(json!({"subtype": "set_model", "model": model})),
        control_ok(json!({"model": resolved})),
    ]
}

fn listed_model(value: &str, resolved: &str, levels: &[&str]) -> Value {
    json!({"value": value, "resolvedModel": resolved, "displayName": value,
        "description": format!("{value} for tests"), "supportsEffort": !levels.is_empty(),
        "supportedEffortLevels": levels, "supportsAdaptiveThinking": true,
        "supportsThinkingOff": false})
}

fn listed_command(name: &str, description: &str, hint: &str) -> Value {
    json!({"name": name, "description": description, "argumentHint": hint})
}

fn builtin_command(name: &str) -> Value {
    json!({"name": name, "description": "built in", "argumentHint": "", "builtin": true,
        "aliases": [format!("{name}-alias")]})
}

/// `default` takes every effort; `lite` takes `low` and `medium` and one Claude level tiechui has
/// no name for.
fn catalogue() -> Value {
    json!({
        "models": [
            listed_model("default", "claude-fake-5", &["low", "medium", "high", "xhigh", "max"]),
            listed_model("lite", "claude-lite-5", &["low", "medium", "turbo"]),
        ],
        "commands": [
            listed_command("fix", "fix things (project)", "<bug>"),
            listed_command("review", "look it over (user)", ""),
            listed_command("deploy", "ship it (project)", "[env]"),
            builtin_command("compact"),
            listed_command("plug:tool", "a plugin skill (plugin)", ""),
        ],
    })
}

fn listing_handshake() -> Vec<Value> {
    vec![
        expect(json!({"type": "control_request", "request": {"subtype": "initialize"}})),
        emit(json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": "$id", "response": catalogue()}})),
    ]
}

/// `fix` is a project skill, `review` a user skill (`HOME` is the fixture's root); `deploy` is a
/// custom command and has no skill directory.
fn write_skill_dirs(fixture: &Fixture) {
    for dir in [
        fixture.cwd.join(".claude/skills/fix"),
        fixture.path(".claude/skills/review"),
    ] {
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\ndescription: test\n---\nDo it.\n",
        )
        .unwrap();
    }
    let commands = fixture.cwd.join(".claude/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(commands.join("deploy.md"), "Ship it.\n").unwrap();
}

fn claude_model(id: &str, levels: &[ThinkingLevel]) -> ModelInfo {
    ModelInfo {
        provider: "claude".into(),
        id: id.into(),
        context_window: 0,
        reasoning: true,
        thinking_levels: levels.to_vec(),
    }
}

fn menu_entry(name: &str, description: &str, hint: Option<&str>, kind: CommandKind) -> CommandInfo {
    CommandInfo {
        name: name.into(),
        description: description.into(),
        argument_hint: hint.map(str::to_owned),
        kind,
    }
}

async fn submit_op(client: &mut Client, thread: &str, op: Value) {
    client
        .ok("session/submit", json!({"thread_id": thread, "op": op}))
        .await;
}

fn set_model_op(model: &str) -> Value {
    json!({"kind": "set_model", "provider": "claude", "model": model})
}

fn set_thinking_op(level: &str) -> Value {
    json!({"kind": "set_thinking", "level": level})
}

fn run_command_op(name: &str, arguments: &str) -> Value {
    json!({"kind": "run_command", "name": name, "arguments": arguments})
}

async fn until_error(client: &mut Client, needle: &str) -> String {
    let events = client
        .until(|kind| matches!(kind, EventKind::Error { message } if message.contains(needle)))
        .await;
    match &events.last().unwrap().event.event {
        EventKind::Error { message } => message.clone(),
        other => panic!("{other:?}"),
    }
}

async fn until_model(client: &mut Client, id: &str) -> ThinkingLevel {
    let events = client
        .until(|kind| matches!(kind, EventKind::SessionUpdated { model, .. } if model.id == id))
        .await;
    match &events.last().unwrap().event.event {
        EventKind::SessionUpdated { thinking, .. } => *thinking,
        other => panic!("{other:?}"),
    }
}

async fn until_thinking(client: &mut Client, level: ThinkingLevel) {
    client
        .until(
            |kind| matches!(kind, EventKind::SessionUpdated { thinking, .. } if *thinking == level),
        )
        .await;
}

async fn until_turn_started(client: &mut Client) {
    client
        .until(|kind| matches!(kind, EventKind::TurnStarted { .. }))
        .await;
}

fn entries(path: &str, kind: &str) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|entry| entry["kind"] == kind)
        .collect()
}

fn session_state(events: &[tiechui_protocol::Event]) -> (ModelInfo, ThinkingLevel) {
    events
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            EventKind::SessionUpdated {
                model, thinking, ..
            } => Some((model.clone(), *thinking)),
            _ => None,
        })
        .unwrap()
}

fn command_lists(events: &[SessionEvent]) -> Vec<Vec<CommandInfo>> {
    events
        .iter()
        .filter_map(|e| match &e.event.event {
            EventKind::CommandsUpdated { commands } => Some(commands.clone()),
            _ => None,
        })
        .collect()
}

/// The catalogue and the commands, from the snapshot when they arrived before it and from the
/// events after it otherwise.
async fn listed(client: &mut Client, opened: &Snapshot) -> (Vec<ModelInfo>, Vec<CommandInfo>) {
    fn note(
        kind: &EventKind,
        models: &mut Option<Vec<ModelInfo>>,
        commands: &mut Option<Vec<CommandInfo>>,
    ) {
        match kind {
            EventKind::ModelsUpdated { models: listed } if listed.len() > 1 => {
                *models = Some(listed.clone());
            }
            EventKind::CommandsUpdated { commands: listed } if !listed.is_empty() => {
                *commands = Some(listed.clone());
            }
            _ => {}
        }
    }
    let (mut models, mut commands) = (None, None);
    for event in &opened.events {
        note(&event.event, &mut models, &mut commands);
    }
    while models.is_none() || commands.is_none() {
        let events = client
            .until(|kind| {
                matches!(
                    kind,
                    EventKind::ModelsUpdated { .. } | EventKind::CommandsUpdated { .. }
                )
            })
            .await;
        note(
            &events.last().unwrap().event.event,
            &mut models,
            &mut commands,
        );
    }
    (models.unwrap(), commands.unwrap())
}

async fn open_with(client: &mut Client, fixture: &Fixture, extra: Value) -> Snapshot {
    let mut params = json!({"cwd": fixture.cwd, "runtime": "claude"});
    params
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    snapshot(client.ok("session/open", params).await)
}

#[tokio::test]
async fn the_catalogue_and_the_menu_are_listed_with_skills_told_apart_and_kept_current() {
    use CommandKind::{Command, Skill};
    use ThinkingLevel::{High, Low, Max, Medium, Off, Xhigh};
    let fixture = Fixture::new();
    write_skill_dirs(&fixture);
    let mut steps = listing_handshake();
    steps.push(user_message("go"));
    steps.push(system(
        "init",
        json!({"model": "claude-fake", "skills": ["fix", "review", "plug:tool"]}),
    ));
    steps.extend(reply("msg_1", "ok"));
    steps.push(system(
        "commands_changed",
        json!({"commands": [
            listed_command("fix", "fix things (project)", "<bug>"),
            listed_command("plug:tool", "a plugin skill (plugin)", ""),
            listed_command("lint", "lint it (project)", ""),
            builtin_command("compact"),
        ]}),
    ));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let (models, commands) = listed(&mut client, &opened).await;
    let default = claude_model("default", &[Off, Low, Medium, High, Xhigh, Max]);
    assert_eq!(
        models,
        [default.clone(), claude_model("lite", &[Off, Low, Medium])]
    );
    let fix = menu_entry("fix", "fix things (project)", Some("<bug>"), Skill);
    assert_eq!(
        commands,
        [
            fix.clone(),
            menu_entry("review", "look it over (user)", None, Skill),
            menu_entry("deploy", "ship it (project)", Some("[env]"), Command),
            menu_entry("plug:tool", "a plugin skill (plugin)", None, Command),
        ],
        "no built-ins; a skill directory makes a skill"
    );
    assert_eq!(
        session_state(&opened.events),
        (default, High),
        "Claude's default model at the effort it runs without one"
    );

    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    let events = client
        .until(|kind| {
            matches!(kind, EventKind::CommandsUpdated { commands }
                if commands.iter().any(|command| command.name == "lint"))
        })
        .await;
    let plugin = menu_entry("plug:tool", "a plugin skill (plugin)", None, Skill);
    let lists = command_lists(&events);
    assert_eq!(
        lists[lists.len() - 2..],
        [
            vec![
                fix.clone(),
                menu_entry("review", "look it over (user)", None, Skill),
                menu_entry("deploy", "ship it (project)", Some("[env]"), Command),
                plugin.clone(),
            ],
            vec![
                fix,
                plugin,
                menu_entry("lint", "lint it (project)", None, Command)
            ],
        ],
        "`init` names the skills Claude loaded; `commands_changed` replaces the menu"
    );
    assert_script_followed(&fixture);
}

#[tokio::test]
async fn a_model_switch_is_a_set_model_request_recorded_in_the_session_and_refusals_say_why() {
    let fixture = Fixture::new();
    let mut steps = listing_handshake();
    steps.extend([
        control(json!({"subtype": "set_model", "model": "nope"})),
        control_failed("Model nope was not found", "not_found"),
        control(json!({"subtype": "set_model", "model": "locked"})),
        control_failed(
            "Your organization does not allow locked",
            "restricted_by_org",
        ),
        control(json!({"subtype": "set_model", "model": "gone"})),
        control_failed("Invalid API key · Please run /login", "auth_failed"),
        control(json!({"subtype": "set_model", "model": "odd"})),
        control_failed("API error: 503 · model not changed", "check_failed"),
    ]);
    steps.extend(set_model("lite", "claude-lite-5"));
    // The session shows high clamped to lite's levels, so Claude is told to run medium.
    steps.extend(effort("medium"));
    steps.push(user_message("go"));
    steps.extend(simple_turn("msg_1", "ok", usage_json(1, 1, 0, 0)));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();

    submit_op(&mut client, &thread, set_model_op("nope")).await;
    assert_eq!(
        until_error(&mut client, "nope").await,
        "claude 找不到模型 nope"
    );
    submit_op(&mut client, &thread, set_model_op("locked")).await;
    let refused = until_error(&mut client, "locked").await;
    assert!(refused.contains("受限"), "{refused}");
    submit_op(&mut client, &thread, set_model_op("gone")).await;
    let refused = until_error(&mut client, "登录").await;
    assert!(refused.contains("未登录"), "{refused}");
    submit_op(&mut client, &thread, set_model_op("odd")).await;
    until_error(&mut client, "API error: 503 · model not changed").await;
    submit_op(&mut client, &thread, set_model_op("lite")).await;
    assert_eq!(
        until_model(&mut client, "lite").await,
        ThinkingLevel::Medium
    );
    submit(&mut client, &thread, "go", "follow_up").await;
    client.until(is_completed).await;

    assert_script_followed(&fixture);
    let models: Vec<_> = entries(&opened.path, "model_change")
        .iter()
        .map(|entry| entry["model"].clone())
        .collect();
    assert_eq!(models, [json!({"provider": "claude", "model": "lite"})]);
    assert!(entries(&opened.path, "thinking_change").is_empty());
}

#[tokio::test]
async fn effort_and_thinking_switches_are_control_requests_that_apply_from_the_next_turn() {
    let fixture = Fixture::new();
    let mut steps = listing_handshake();
    steps.extend([
        user_message("one"),
        system("init", json!({})),
        message_start("msg_1"),
    ]);
    // Taken while the turn runs.
    steps.extend(effort("high"));
    steps.extend(streamed_block("msg_1", 0, "text", &["one"]));
    steps.extend(message_end("msg_1", usage_json(1, 1, 0, 0), "end_turn"));
    steps.push(result_ok("one"));
    steps.extend(thinking_cap(json!(0)));
    steps.push(user_message("two"));
    steps.extend(simple_turn("msg_2", "two", usage_json(1, 1, 0, 0)));
    // Leaving `off` lifts the cap before the effort.
    steps.extend(thinking_cap(Value::Null));
    steps.extend(effort("medium"));
    steps.push(user_message("three"));
    steps.extend(simple_turn("msg_3", "three", usage_json(1, 1, 0, 0)));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open_with(&mut client, &fixture, json!({"thinking": "low"})).await;
    assert_eq!(session_state(&opened.events).1, ThinkingLevel::Low);
    assert_eq!(arg_after(&fixture.launches()[0], "--effort"), Some("low"));
    let thread = opened.thread_id.0.clone();

    submit(&mut client, &thread, "one", "follow_up").await;
    until_turn_started(&mut client).await;
    submit_op(&mut client, &thread, set_thinking_op("high")).await;
    until_thinking(&mut client, ThinkingLevel::High).await;
    submit_op(&mut client, &thread, set_thinking_op("minimal")).await;
    let refused = until_error(&mut client, "minimal").await;
    assert!(
        refused.contains("low, medium, high, xhigh, max"),
        "{refused}"
    );
    client.until(is_completed).await;
    submit_op(&mut client, &thread, set_thinking_op("off")).await;
    until_thinking(&mut client, ThinkingLevel::Off).await;
    submit(&mut client, &thread, "two", "follow_up").await;
    client.until(is_completed).await;
    submit_op(&mut client, &thread, set_thinking_op("medium")).await;
    until_thinking(&mut client, ThinkingLevel::Medium).await;
    submit(&mut client, &thread, "three", "follow_up").await;
    client.until(is_completed).await;

    assert_script_followed(&fixture);
    let levels: Vec<_> = entries(&opened.path, "thinking_change")
        .iter()
        .map(|entry| entry["level"].clone())
        .collect();
    assert_eq!(levels, [json!("high"), Value::Null, json!("medium")]);
}

#[tokio::test]
async fn a_model_without_the_requested_effort_runs_the_nearest_one_and_the_request_comes_back() {
    let fixture = Fixture::new();
    let mut steps = listing_handshake();
    steps.extend(effort("high"));
    steps.extend(set_model("lite", "claude-lite-5"));
    steps.extend(effort("medium"));
    steps.extend(set_model("default", "claude-fake-5"));
    steps.extend(effort("high"));
    steps.push(user_message("go"));
    steps.extend(simple_turn("msg_1", "ok", usage_json(1, 1, 0, 0)));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open_with(&mut client, &fixture, json!({"thinking": "low"})).await;
    let thread = opened.thread_id.0.clone();

    submit_op(&mut client, &thread, set_thinking_op("high")).await;
    until_thinking(&mut client, ThinkingLevel::High).await;
    submit_op(&mut client, &thread, set_model_op("lite")).await;
    assert_eq!(
        until_model(&mut client, "lite").await,
        ThinkingLevel::Medium
    );
    submit_op(&mut client, &thread, set_model_op("default")).await;
    assert_eq!(
        until_model(&mut client, "default").await,
        ThinkingLevel::High
    );
    submit(&mut client, &thread, "go", "follow_up").await;
    client.until(is_completed).await;
    assert_script_followed(&fixture);
}

fn switched_to(mode: &'static str) -> impl Fn(&EventKind) -> bool {
    move |kind| {
        matches!(kind, EventKind::Extension { name, data }
            if name == PERMISSION_MODE_EVENT && *data == mode)
    }
}

#[tokio::test]
async fn an_idle_permission_switch_restarts_claude_on_the_same_session_with_its_model_and_effort() {
    let fixture = Fixture::new();
    let mut steps = listing_handshake();
    steps.extend(set_model("lite", "claude-lite-5"));
    steps.push(user_message("one"));
    steps.extend(simple_turn("msg_1", "one", usage_json(2, 7, 100, 50)));
    fixture.script_for_launch(0, steps);
    let mut steps = listing_handshake();
    steps.push(message_start("msg_1"));
    steps.extend(message_end("msg_1", usage_json(2, 7, 100, 50), "end_turn"));
    steps.push(user_message("two"));
    steps.extend(simple_turn("msg_2", "two", usage_json(1, 3, 200, 0)));
    fixture.script_for_launch(1, steps);
    let mut client = fixture.client();
    let opened = open_with(&mut client, &fixture, json!({"thinking": "medium"})).await;
    let thread = opened.thread_id.0.clone();

    submit_op(&mut client, &thread, set_model_op("lite")).await;
    until_model(&mut client, "lite").await;
    submit(&mut client, &thread, "one", "follow_up").await;
    client.until(is_completed).await;
    submit_op(&mut client, &thread, run_command_op("permissions", "auto")).await;
    client.until(switched_to("auto")).await;
    submit(&mut client, &thread, "two", "follow_up").await;
    let events = client.until(is_completed).await;

    assert_script_followed(&fixture);
    let launches = fixture.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    let id = arg_after(&launches[0], "--session-id").unwrap();
    let argv = &launches[1];
    assert_eq!(arg_after(argv, "--permission-mode"), Some("auto"));
    assert_eq!(arg_after(argv, "--resume"), Some(id));
    assert!(!argv.iter().any(|arg| arg == "--session-id"), "{argv:?}");
    assert_eq!(arg_after(argv, "--model"), Some("lite"));
    assert_eq!(arg_after(argv, "--effort"), Some("medium"));
    assert_eq!(saved_claude_session_id(&opened.path), id);
    let used = usages(&events);
    assert_eq!(used.len(), 1, "msg_1 was already counted: {used:?}");
    let replies: Vec<_> = known(&events)
        .into_iter()
        .filter_map(|item| match item {
            KnownItem::AssistantMessage { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(replies, ["two"]);
}

#[tokio::test]
async fn a_permission_switch_is_refused_while_a_turn_runs_an_approval_waits_or_an_agent_runs() {
    let fixture = Fixture::new();
    let input = json!({"command": "touch a.txt", "description": "Create a.txt"});
    let mut steps = listing_handshake();
    steps.extend([
        user_message("one"),
        system("init", json!({})),
        message_start("msg_a"),
        user_message("go on"),
    ]);
    steps.extend(streamed_block("msg_a", 0, "text", &["done"]));
    steps.extend(message_end("msg_a", usage_json(1, 1, 0, 0), "end_turn"));
    steps.push(result_ok("done"));
    steps.extend([user_message("two"), system("init", json!({}))]);
    steps.extend(tool_message("msg_b", "toolu_touch", "Bash", &input));
    steps.extend([
        can_use_tool("perm-1", "Bash", &input, "toolu_touch"),
        expect_permission_answer(
            "perm-1",
            json!({"behavior": "allow", "updatedInput": input}),
        ),
    ]);
    steps.extend(message_end("msg_b", usage_json(1, 1, 0, 0), "tool_use"));
    steps.push(tool_result("toolu_touch", json!("ok"), false));
    steps.extend(reply("msg_c", "touched"));
    steps.push(user_message("three"));
    steps.extend(background_spawn("agent-1", "toolu_agent"));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    let switch = run_command_op("permissions", "yolo");

    submit(&mut client, &thread, "one", "follow_up").await;
    until_turn_started(&mut client).await;
    submit_op(&mut client, &thread, switch.clone()).await;
    until_error(&mut client, "回合运行中无法切换权限").await;
    submit(&mut client, &thread, "go on", "steer").await;
    client.until(is_completed).await;

    submit(&mut client, &thread, "two", "follow_up").await;
    let (_, approval, _) = next_approval(&mut client).await;
    submit_op(&mut client, &thread, switch.clone()).await;
    until_error(&mut client, "有待处理的审批").await;
    answer(
        &mut client,
        &thread,
        &approval,
        json!({"kind": "allow_once"}),
    )
    .await;
    client.until(is_completed).await;

    submit(&mut client, &thread, "three", "follow_up").await;
    client.until(is_completed).await;
    submit_op(&mut client, &thread, switch).await;
    let events = client
        .until(|kind| matches!(kind, EventKind::Error { message } if message.contains("后台子代理仍在运行")))
        .await;
    assert!(
        !events.iter().any(|e| switched_to("yolo")(&e.event.event)),
        "no switch was made"
    );
    assert_eq!(fixture.launches().len(), 1);
    assert_script_followed(&fixture);
}

async fn submit_content(client: &mut Client, thread: &str, content: Value, delivery: &str) {
    client
        .ok(
            "session/submit",
            json!({"thread_id": thread, "op": {"kind": "user_input", "delivery": delivery,
                "content": content}}),
        )
        .await;
}

#[tokio::test]
async fn the_last_skill_mentioned_opens_the_message_and_commands_run_as_prompts() {
    let image = json!({"type": "image",
        "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}});
    let fixture = Fixture::new();
    write_skill_dirs(&fixture);
    let mut steps = listing_handshake();
    steps.extend([
        user_message("/fix please $review this, then it"),
        system("init", json!({})),
        message_start("msg_1"),
        user_message("/review and"),
    ]);
    steps.extend(streamed_block("msg_1", 0, "text", &["ok"]));
    steps.extend(message_end("msg_1", usage_json(1, 1, 0, 0), "end_turn"));
    steps.push(result_ok("ok"));
    let mut turn = 1;
    for sent in [
        "/review the diff",
        "/fix the bug, then $review it",
        "/deploy now, with $review",
        "keep $deploy, src/fix, /fixer and price$review",
    ] {
        turn += 1;
        steps.push(user_message(sent));
        steps.extend(simple_turn(
            &format!("msg_{turn}"),
            "ok",
            usage_json(1, 1, 0, 0),
        ));
    }
    steps.push(expect(json!({"type": "user", "message": {"role": "user",
        "content": [image, {"type": "text", "text": "/review look at"}]}})));
    steps.extend(simple_turn("msg_7", "ok", usage_json(1, 1, 0, 0)));
    for (sent, msg) in [("/fix the tests", "msg_8"), ("/deploy", "msg_9")] {
        steps.push(user_message(sent));
        steps.extend(simple_turn(msg, "ok", usage_json(1, 1, 0, 0)));
    }
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    listed(&mut client, &opened).await;

    submit(
        &mut client,
        &thread,
        "please $review this, then /fix it",
        "follow_up",
    )
    .await;
    until_turn_started(&mut client).await;
    submit(&mut client, &thread, "and $review", "steer").await;
    client.until(is_completed).await;
    for typed in [
        "$review the diff",
        "/fix the bug, then $review it",
        "/deploy now, with $review",
        "keep $deploy, src/fix, /fixer and price$review",
    ] {
        submit(&mut client, &thread, typed, "follow_up").await;
        client.until(is_completed).await;
    }
    submit_content(
        &mut client,
        &thread,
        json!([{"kind": "text", "text": "look at $review"},
            {"kind": "image", "mime_type": "image/png", "data": "aGk="}]),
        "follow_up",
    )
    .await;
    client.until(is_completed).await;
    submit_op(&mut client, &thread, run_command_op("nope", "")).await;
    until_error(&mut client, "nope").await;
    submit_op(
        &mut client,
        &thread,
        json!({"kind": "command_input", "name": "fix", "data": {}}),
    )
    .await;
    until_error(&mut client, "fix").await;
    submit_op(&mut client, &thread, run_command_op("fix", "  the tests ")).await;
    client.until(is_completed).await;
    submit_op(&mut client, &thread, run_command_op("deploy", "")).await;
    client.until(is_completed).await;
    assert_script_followed(&fixture);
}

#[tokio::test]
async fn reopening_and_a_restart_after_an_error_keep_the_switched_model_and_thinking() {
    let fixture = Fixture::new();
    let mut steps = listing_handshake();
    steps.extend(set_model("lite", "claude-lite-5"));
    steps.extend(thinking_cap(json!(0)));
    steps.push(user_message("one"));
    steps.extend(simple_turn("msg_1", "one", usage_json(1, 1, 0, 0)));
    fixture.script_for_launch(0, steps);
    let mut steps = listing_handshake();
    steps.extend(thinking_cap(json!(0)));
    steps.extend([
        user_message("two"),
        system("init", json!({})),
        message_start("msg_2"),
        result_error("error_during_execution", &["API Error: 529 overloaded"]),
    ]);
    fixture.script_for_launch(1, steps);
    let mut steps = listing_handshake();
    steps.extend(thinking_cap(json!(0)));
    steps.push(user_message("three"));
    steps.extend(simple_turn("msg_3", "three", usage_json(1, 1, 0, 0)));
    fixture.script_for_launch(2, steps);

    let path = {
        let mut client = fixture.client();
        let opened = open_with(&mut client, &fixture, json!({"thinking": "low"})).await;
        let thread = opened.thread_id.0.clone();
        submit_op(&mut client, &thread, set_model_op("lite")).await;
        until_model(&mut client, "lite").await;
        submit_op(&mut client, &thread, set_thinking_op("off")).await;
        until_thinking(&mut client, ThinkingLevel::Off).await;
        submit(&mut client, &thread, "one", "follow_up").await;
        client.until(is_completed).await;
        client
            .ok("session/close", json!({"thread_id": thread}))
            .await;
        opened.path
    };
    let mut client = fixture.client();
    let reopened = snapshot(client.ok("session/open", json!({"path": path})).await);
    let (model, thinking) = session_state(&reopened.events);
    assert_eq!((model.id.as_str(), thinking), ("lite", ThinkingLevel::Off));
    let thread = reopened.thread_id.0.clone();
    submit(&mut client, &thread, "two", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Failed);
    submit(&mut client, &thread, "three", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);

    assert_script_followed(&fixture);
    let launches = fixture.launches();
    assert_eq!(launches.len(), 3, "{launches:?}");
    let id = arg_after(&launches[0], "--session-id").unwrap();
    for argv in &launches[1..] {
        assert_eq!(arg_after(argv, "--resume"), Some(id), "{argv:?}");
        assert_eq!(arg_after(argv, "--model"), Some("lite"), "{argv:?}");
        for flag in ["--thinking", "--effort"] {
            assert!(!argv.iter().any(|arg| arg == flag), "{flag} in {argv:?}");
        }
    }
}
