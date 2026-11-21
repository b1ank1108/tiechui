#![allow(clippy::unwrap_used)]

mod client;
mod support;

use std::path::{Path, PathBuf};
use std::time::Duration;

use client::Client;
use serde_json::{Value, json};
use tempfile::TempDir;
use tiechui_protocol::app::{SessionEvent, Snapshot};
use tiechui_protocol::{EventKind, Item, KnownItem, TokenUsage, TurnId, TurnStatus};

const SESSION: &str = "fake-session";

fn expect(frame: Value) -> Value {
    json!({ "expect": frame })
}

fn emit(frame: Value) -> Value {
    json!({ "emit": frame })
}

/// The `initialize` exchange every launch starts with.
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

/// A streamed text or thinking block.
fn block(msg: &str, index: u64, kind: &str, deltas: &[&str]) -> Vec<Value> {
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

/// A turn that answers with one short text.
fn simple_turn(msg: &str, text: &str, usage: Value) -> Vec<Value> {
    let mut steps = vec![
        system("init", json!({"model": "claude-fake"})),
        message_start(msg),
    ];
    steps.extend(block(msg, 0, "text", &[text]));
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
        std::fs::remove_file(root.path().join("fake.log")).ok();
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

    /// The script of the launch with this 0-based index.
    fn script_for_launch(&self, launch: usize, steps: Vec<Value>) {
        self.write_script(&format!("script.ndjson.{launch}"), &steps);
    }

    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("fake.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// The argument vectors of every `claude` launch (not `--version`).
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
        for (key, value) in env {
            command.env(key, value);
        }
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

/// The Claude session id the session file saved as its `runtime.thread` entry.
fn saved_thread(path: &str) -> String {
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
    for gated in [
        "branch",
        "fork",
        "rewind",
        "retry",
        "compact",
        "side_chat",
        "commands",
        "thinking",
        "model",
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
    steps.extend(block("msg_1", 0, "thinking", &["let me ", "think"]));
    steps.extend(block("msg_1", 1, "text", &["I will ", "read."]));
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
    // The same message's usage twice (a replay): counted once.
    steps.extend(message_end("msg_2", usage_json(2, 10, 1500, 0), "tool_use"));
    steps.push(stream(
        "msg_2",
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"},
        "usage": usage_json(2, 10, 1500, 0)}),
    ));
    steps.push(tool_result("toolu_write", json!("File created"), false));
    steps.push(tool_result(
        "toolu_edit",
        json!([{"type": "text", "text": "updated"}]),
        false,
    ));
    steps.push(message_start("msg_3"));
    steps.extend(block("msg_3", 0, "text", &["done"]));
    steps.extend(message_end("msg_3", usage_json(2, 5, 1700, 0), "end_turn"));
    // Nothing before `result` ends the turn.
    steps.push(json!({"sleep_ms": 300}));
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
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

/// Frames of a real 2.1.322 turn (thinking summary, Write, Bash, Read, Edit, final text) as
/// recorded, with paths and signatures scrubbed.
#[tokio::test]
async fn a_recorded_turn_maps_like_the_synthetic_ones() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("go"));
    let recorded = include_str!("fixtures/claude-tools-turn.ndjson");
    steps.extend(
        recorded
            .lines()
            .map(|line| serde_json::from_str(line).unwrap()),
    );
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    let events = client.until(is_completed).await;
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
    let used = usages(&events);
    assert_eq!(used.len(), 4, "{used:?}");
    assert!(
        used.iter()
            .all(|usage| usage.output > 0 && usage.cache_read > 0)
    );
}

#[tokio::test]
async fn subagent_frames_and_task_frames_do_not_break_the_main_turn() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.push(message_start("msg_1"));
    steps.extend(tool_use(
        "msg_1",
        0,
        "toolu_agent",
        "Agent",
        &json!({"description": "sub", "prompt": "do"}),
    ));
    steps.extend(message_end("msg_1", usage_json(1, 1, 0, 0), "tool_use"));
    steps.push(system(
        "task_started",
        json!({"task_id": "t1", "tool_use_id": "toolu_agent", "description": "sub"}),
    ));
    let mut child = frame(
        "assistant",
        json!({"message": {"id": "msg_child", "content": [{"type": "text", "text": "child words"}]}}),
    );
    child["emit"]["parent_tool_use_id"] = json!("toolu_agent");
    steps.push(child);
    let mut child_stream = stream(
        "msg_child",
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "child delta"}}),
    );
    child_stream["emit"]["parent_tool_use_id"] = json!("toolu_agent");
    steps.push(child_stream);
    let mut child_result = tool_result("toolu_inner", json!("inner"), false);
    child_result["emit"]["parent_tool_use_id"] = json!("toolu_agent");
    steps.push(child_result);
    steps.push(system(
        "task_notification",
        json!({"task_id": "t1", "status": "completed"}),
    ));
    steps.push(tool_result("toolu_agent", json!("sub done"), false));
    steps.push(message_start("msg_2"));
    steps.extend(block("msg_2", 0, "text", &["parent words"]));
    steps.extend(message_end("msg_2", usage_json(1, 2, 0, 0), "end_turn"));
    steps.push(result_ok("parent words"));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    let text = serde_json::to_string(&known(&events)).unwrap();
    assert!(
        text.contains("parent words") && !text.contains("child"),
        "{text}"
    );
    assert!(!text.contains("inner"), "{text}");
    assert_eq!(usages(&events).len(), 2);
}

#[tokio::test]
async fn control_requests_are_answered_so_claude_never_waits() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("go"));
    steps.extend([
        emit(
            json!({"type": "control_request", "request_id": "perm-1", "request": {
            "subtype": "can_use_tool", "tool_name": "Write", "input": {"file_path": "/x"},
            "tool_use_id": "toolu_w"}}),
        ),
        expect(json!({"type": "control_response", "response": {
            "subtype": "success", "request_id": "perm-1",
            "response": {"behavior": "deny"}}})),
        emit(
            json!({"type": "control_request", "request_id": "hook-1", "request": {
            "subtype": "hook_callback", "callback_id": "x"}}),
        ),
        expect(json!({"type": "control_response", "response": {
            "subtype": "error", "request_id": "hook-1"}})),
    ]);
    steps.extend(simple_turn("msg_1", "ok", usage_json(1, 1, 0, 0)));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
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
    assert_eq!(saved_thread(&opened.path), id);
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
    for (level, flags) in [
        ("off", ("--thinking", "disabled")),
        ("minimal", ("--effort", "low")),
        ("low", ("--effort", "low")),
        ("medium", ("--effort", "medium")),
        ("xhigh", ("--effort", "xhigh")),
        ("max", ("--effort", "max")),
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
        assert_eq!(arg_after(&launches[0], flags.0), Some(flags.1), "{level}");
    }
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
        let id = saved_thread(&opened.path);
        (opened.path, id)
    };
    // The resumed process repeats msg_1's usage before it gets input (2.1.322 does not, a
    // different Claude might) and answers anew.
    let mut steps = handshake();
    steps.push(message_start("msg_1"));
    steps.extend(message_end("msg_1", usage_json(2, 7, 100, 50), "end_turn"));
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
    assert_eq!(saved_thread(&path), id, "resume keeps the id");
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
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
        let old = saved_thread(&opened.path);
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
    assert_eq!(saved_thread(&path), fresh);
    submit(&mut client, &reopened.thread_id.0, "again", "follow_up").await;
    client.until(is_completed).await;
}

#[tokio::test]
async fn a_message_written_during_a_turn_joins_it_and_does_not_start_another() {
    let fixture = Fixture::new();
    let mut steps = handshake();
    steps.push(user_message("first"));
    steps.extend([
        system("init", json!({})),
        message_start("msg_1"),
        // The steered text arrives while the turn is running.
        user_message("also this"),
    ]);
    steps.extend(block("msg_1", 0, "text", &["both"]));
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
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

async fn running_turn(fixture: &Fixture) -> (Client, String, TurnId) {
    let mut client = fixture.client();
    let opened = open(&mut client, fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let started = client
        .until(|kind| matches!(kind, EventKind::TurnStarted { .. }))
        .await;
    let EventKind::TurnStarted { turn } = &started.last().unwrap().event.event else {
        unreachable!()
    };
    let turn_id = turn.id.clone();
    (client, thread, turn_id)
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
    let (mut client, thread, turn) = running_turn(&fixture).await;
    client.ok("session/submit", interrupt(&thread, &turn)).await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Interrupted);
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));
    // The process stays up for the next turn.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(alive(pid_in(&pid)));
    submit(&mut client, &thread, "after", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert_eq!(fixture.launches().len(), 1);
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
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
    // The session survives: the next turn starts a new process that resumes the session.
    submit(&mut client, &thread, "retry", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    let launches = fixture.launches();
    assert_eq!(launches.len(), 2, "{launches:?}");
    let id = saved_thread(&opened.path);
    assert_eq!(arg_after(&launches[1], "--resume"), Some(id.as_str()));
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
    // The fake records the environment it received.
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
