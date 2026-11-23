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

const THREAD: &str = "thr-root";
const TURN: &str = "turn-1";

fn expect(frame: Value) -> Value {
    json!({ "expect": frame })
}

fn emit(frame: Value) -> Value {
    json!({ "emit": frame })
}

fn notify(method: &str, params: Value) -> Value {
    emit(json!({ "method": method, "params": params }))
}

fn answer(result: Value) -> Value {
    emit(json!({ "id": "$id", "result": result }))
}

fn handshake(mode: (&str, &str)) -> Vec<Value> {
    vec![
        expect(json!({"method": "initialize", "params": {"clientInfo": {"name": "tiechui"}}})),
        answer(
            json!({"userAgent": "fake/0.171.0 (test)", "codexHome": "/x",
            "platformFamily": "unix", "platformOs": "test"}),
        ),
        expect(json!({"method": "initialized"})),
        expect(json!({"method": "thread/start", "params": {
            "approvalPolicy": mode.0, "sandbox": mode.1}})),
        answer(thread_response()),
    ]
    .into_iter()
    .chain(listings())
    .collect()
}

fn resume_handshake(params: Value, response: Value) -> Vec<Value> {
    let mut steps = vec![
        expect(json!({"method": "initialize"})),
        answer(json!({"userAgent": "fake/0.171.0 (test)"})),
        expect(json!({"method": "initialized"})),
        expect(json!({"method": "thread/resume", "params": params})),
        answer(response),
    ];
    steps.extend(listings());
    steps
}

fn resumed() -> Vec<Value> {
    resume_handshake(json!({"threadId": THREAD}), thread_response())
}

fn thread_response() -> Value {
    json!({"thread": {"id": THREAD, "turns": []}, "model": "gpt-fake",
        "modelProvider": "openai", "approvalPolicy": "untrusted", "sandbox": {"type": "readOnly"},
        "cwd": "/x"})
}

fn model_entry(id: &str, hidden: bool, efforts: &[&str], default: &str) -> Value {
    let efforts: Vec<Value> = efforts
        .iter()
        .map(|effort| json!({"reasoningEffort": effort, "description": effort}))
        .collect();
    json!({"id": id, "model": id, "displayName": id.to_uppercase(), "description": "",
        "hidden": hidden, "supportedReasoningEfforts": efforts,
        "defaultReasoningEffort": default, "isDefault": id == "gpt-fake"})
}

fn skill(name: &str, description: &str, enabled: bool) -> Value {
    json!({"name": name, "description": description, "path": format!("/s/{name}/SKILL.md"),
        "scope": "user", "enabled": enabled, "pluginId": null})
}

/// What Codex lists once the thread is open: a model catalogue over two pages (with a hidden
/// model and the unmapped `ultra` effort) and the skills of two roots (a disabled one, a
/// duplicate name, one described only by its interface).
fn listings() -> Vec<Value> {
    let mut review = skill("review", "", true);
    review["interface"] = json!({"displayName": "Review", "shortDescription": "look it over"});
    vec![
        expect(json!({"method": "model/list"})),
        answer(json!({"data": [
            model_entry("gpt-fake", false, &["low", "medium", "high", "ultra"], "medium"),
            model_entry("gpt-secret", true, &["low"], "low"),
        ], "nextCursor": "page-2"})),
        expect(json!({"method": "model/list", "params": {"cursor": "page-2"}})),
        answer(
            json!({"data": [model_entry("gpt-lite", false, &["low", "medium"], "low")],
            "nextCursor": null}),
        ),
        expect(json!({"method": "skills/list"})),
        answer(json!({"data": [
            {"cwd": "/x", "errors": [], "skills": [
                skill("fix", "fix things", true), skill("parked", "not now", false)]},
            {"cwd": "/y", "errors": [], "skills": [review, skill("fix", "again", true)]},
        ]})),
    ]
}

fn turn_start(text: &str, turn: &str) -> Vec<Value> {
    vec![
        expect(json!({"method": "turn/start", "params": {
            "threadId": THREAD, "input": [{"type": "text", "text": text}]}})),
        answer(json!({"turn": {"id": turn, "items": [], "status": "inProgress"}})),
        notify(
            "turn/started",
            json!({"threadId": THREAD, "turn": {"id": turn, "items": [], "status": "inProgress"}}),
        ),
    ]
}

fn turn_done(turn: &str, status: &str) -> Value {
    notify(
        "turn/completed",
        json!({"threadId": THREAD, "turn": {"id": turn, "items": [], "status": status}}),
    )
}

fn agent_text(id: &str, turn: &str, deltas: &[&str]) -> Vec<Value> {
    let item = |text: &str| json!({"type": "agentMessage", "id": id, "text": text});
    let mut steps = vec![notify(
        "item/started",
        json!({"threadId": THREAD, "turnId": turn, "item": item("")}),
    )];
    for delta in deltas {
        steps.push(notify(
            "item/agentMessage/delta",
            json!({"threadId": THREAD, "turnId": turn, "itemId": id, "delta": delta}),
        ));
    }
    steps.push(notify(
        "item/completed",
        json!({"threadId": THREAD, "turnId": turn, "item": item(&deltas.concat())}),
    ));
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
        let name = if cfg!(windows) { "codex.exe" } else { "codex" };
        let fake = root.path().join("bin").join(name);
        std::fs::copy(env!("CARGO_BIN_EXE_fake-codex"), &fake).unwrap();
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

    fn script(&self, steps: Vec<Value>) {
        let text: Vec<String> = steps.iter().map(Value::to_string).collect();
        std::fs::write(self.path("script.ndjson"), text.join("\n")).unwrap();
    }

    fn received(&self) -> Vec<Value> {
        std::fs::read_to_string(self.path("fake.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
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
            .env("FAKE_CODEX_SCRIPT", self.path("script.ndjson"))
            .env("FAKE_CODEX_LOG", self.path("fake.log"))
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
                json!({"cwd": fixture.cwd, "runtime": "codex", "mode": mode}),
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

#[cfg(unix)]
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(unix)]
async fn gone(pid: i32) -> bool {
    for _ in 0..60 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
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

#[tokio::test]
async fn codex_older_than_the_minimum_is_unavailable_and_the_reason_names_both_versions() {
    let fixture = Fixture::new();
    fixture.script(vec![]);
    let mut client = fixture.client_with(&[("FAKE_CODEX_VERSION", "0.100.2")]);
    let list = client.ok("runtimes/list", json!({})).await;
    let codex = list
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == "codex")
        .unwrap();
    assert_eq!(codex["available"], false, "{codex}");
    assert_eq!(codex["version"], "0.100.2");
    let reason = codex["reason"].as_str().unwrap();
    assert!(
        reason.contains("0.100.2") && reason.contains("0.156.1"),
        "{reason}"
    );
}

#[tokio::test]
async fn the_turn_ends_only_on_the_root_threads_turn_completed() {
    let fixture = Fixture::new();
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("go", TURN));
    steps.extend([
        notify(
            "turn/started",
            json!({"threadId": "thr-child", "turn": {"id": "turn-c", "items": [], "status": "inProgress"}}),
        ),
    ]);
    steps.extend({
        let mut child = agent_text("msg-child", "turn-c", &["child words"]);
        for step in &mut child {
            let params = step["emit"]["params"].as_object_mut().unwrap();
            params.insert("threadId".into(), json!("thr-child"));
        }
        child
    });
    steps.push(notify(
        "turn/completed",
        json!({"threadId": "thr-child", "turn": {"id": "turn-c", "items": [], "status": "completed"}}),
    ));
    steps.push(json!({"sleep_ms": 300}));
    steps.extend(agent_text("msg-root", TURN, &["root ", "words"]));
    steps.push(turn_done(TURN, "completed"));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    let events = client.until(is_completed).await;
    let items = known(&events);
    let texts: Vec<_> = items
        .iter()
        .filter_map(|item| match item {
            KnownItem::AssistantMessage { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        ["root words"],
        "the child's text stays out: {items:?}"
    );
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

fn command_approval(id: u64) -> Value {
    emit(
        json!({"id": id, "method": "item/commandExecution/requestApproval", "params": {
        "threadId": THREAD, "turnId": TURN, "itemId": "exec-1", "startedAtMs": 1,
        "command": "/bin/bash -lc \"touch approved.txt\"",
        "commandActions": [{"type": "unknown", "command": "touch approved.txt"}],
        "cwd": "/x"}}),
    )
}

fn approval_turn() -> Vec<Value> {
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("go", TURN));
    steps.push(notify(
        "item/started",
        json!({"threadId": THREAD, "turnId": TURN, "item": {"type": "commandExecution",
            "id": "exec-1", "command": "/bin/bash -lc \"touch approved.txt\"",
            "commandActions": [{"type": "unknown", "command": "touch approved.txt"}],
            "cwd": "/x", "status": "inProgress"}}),
    ));
    steps.push(command_approval(7));
    steps
}

async fn wait_for_approval(client: &mut Client) -> (String, ApprovalRequest) {
    let seen = client
        .until(|kind| matches!(kind, EventKind::ApprovalRequested { .. }))
        .await;
    match seen.last().unwrap().event.event.clone() {
        EventKind::ApprovalRequested {
            approval_id,
            request,
            ..
        } => (approval_id.0, request),
        _ => unreachable!(),
    }
}

fn approval_response(thread: &str, id: &str, decision: Value) -> Value {
    json!({"thread_id": thread, "op": {"kind": "approval_response",
        "approval_id": id, "decision": decision}})
}

fn responses(fixture: &Fixture, id: u64) -> Vec<Value> {
    fixture
        .received()
        .into_iter()
        .filter(|frame| frame["id"] == id && frame.get("result").is_some())
        .collect()
}

#[tokio::test]
async fn an_approval_answered_twice_at_once_reaches_codex_once() {
    let fixture = Fixture::new();
    let mut steps = approval_turn();
    steps.extend([
        expect(json!({"id": 7, "result": {"decision": "accept"}})),
        notify(
            "serverRequest/resolved",
            json!({"threadId": THREAD, "requestId": 7}),
        ),
        turn_done(TURN, "completed"),
    ]);
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let (approval, request) = wait_for_approval(&mut client).await;
    let ApprovalRequest::Permission {
        tool,
        call_id,
        arguments,
        values,
        ..
    } = request
    else {
        panic!("{request:?}");
    };
    assert_eq!(tool, "bash");
    assert_eq!(call_id, "exec-1", "the card attaches to the command step");
    assert_eq!(arguments["command"], "touch approved.txt");
    assert_eq!(values, ["touch approved.txt"]);

    let first = client
        .send(
            "session/submit",
            approval_response(&thread, &approval, json!({"kind": "allow_once"})),
        )
        .await;
    let second = client
        .send(
            "session/submit",
            approval_response(&thread, &approval, json!({"kind": "deny", "reason": null})),
        )
        .await;
    client.reply(first).await.unwrap();
    client.reply(second).await.unwrap();
    client.until(is_completed).await;
    let sent = responses(&fixture, 7);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0]["result"]["decision"], "accept");
}

#[tokio::test]
async fn a_denied_approval_is_written_as_decline() {
    let fixture = Fixture::new();
    let mut steps = approval_turn();
    steps.extend([
        expect(json!({"id": 7, "result": {"decision": "decline"}})),
        turn_done(TURN, "completed"),
    ]);
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let (approval, _) = wait_for_approval(&mut client).await;
    client
        .ok(
            "session/submit",
            approval_response(&thread, &approval, json!({"kind": "deny", "reason": null})),
        )
        .await;
    client.until(is_completed).await;
    assert_eq!(responses(&fixture, 7).len(), 1);
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

#[tokio::test]
async fn an_approval_codex_resolves_itself_is_withdrawn() {
    let fixture = Fixture::new();
    let mut steps = approval_turn();
    steps.extend([
        notify(
            "serverRequest/resolved",
            json!({"threadId": THREAD, "requestId": 7}),
        ),
        turn_done(TURN, "completed"),
    ]);
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    let events = client.until(is_completed).await;
    let kinds: Vec<_> = events
        .iter()
        .filter(|e| {
            matches!(
                e.event.event,
                EventKind::ApprovalRequested { .. } | EventKind::ApprovalResolved { .. }
            )
        })
        .collect();
    assert_eq!(kinds.len(), 2, "{kinds:?}");
    assert!(responses(&fixture, 7).is_empty(), "nothing is written back");
}

#[tokio::test]
async fn requests_without_a_mapping_are_declined_explicitly() {
    let fixture = Fixture::new();
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("go", TURN));
    steps.extend([
        emit(
            json!({"id": 11, "method": "item/tool/requestUserInput", "params": {
            "threadId": THREAD, "turnId": TURN, "itemId": "q", "isBlocking": true,
            "questions": []}}),
        ),
        expect(json!({"id": 11, "result": {"answers": {}}})),
        emit(json!({"id": 12, "method": "item/tool/call", "params": {
            "threadId": THREAD, "turnId": TURN, "callId": "c", "tool": "t", "arguments": {}}})),
        expect(json!({"id": 12, "result": {"success": false}})),
        emit(json!({"id": 13, "method": "somethingNew/request", "params": {}})),
        expect(json!({"id": 13, "error": {"code": -32601}})),
        turn_done(TURN, "completed"),
    ]);
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    client.until(is_completed).await;
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

#[tokio::test]
async fn a_steer_codex_rejects_runs_as_the_next_turn() {
    let fixture = Fixture::new();
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("first", TURN));
    steps.extend([
        expect(json!({"method": "turn/steer", "params": {
            "threadId": THREAD, "expectedTurnId": TURN,
            "input": [{"type": "text", "text": "also this"}]}})),
        emit(json!({"id": "$id", "error": {"code": -32600, "message": "turn is not steerable"}})),
        turn_done(TURN, "completed"),
    ]);
    steps.extend(turn_start("also this", "turn-2"));
    steps.extend(agent_text("msg-2", "turn-2", &["done"]));
    steps.push(turn_done("turn-2", "completed"));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "first", "follow_up").await;
    client
        .until(|kind| matches!(kind, EventKind::TurnStarted { .. }))
        .await;
    submit(&mut client, &thread, "also this", "steer").await;
    let first = client.until(is_completed).await;
    assert_eq!(completed_status(&first), TurnStatus::Completed);
    let second = client.until(is_completed).await;
    assert!(
        known(&second).iter().any(|item| matches!(item,
            KnownItem::AssistantMessage { text, .. } if text == "done")),
        "the queued input ran as its own turn"
    );
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

#[tokio::test]
async fn a_steer_codex_accepts_joins_the_running_turn() {
    let fixture = Fixture::new();
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("first", TURN));
    steps.extend([
        expect(json!({"method": "turn/steer", "params": {"expectedTurnId": TURN}})),
        answer(json!({"turnId": TURN})),
        turn_done(TURN, "completed"),
    ]);
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "first", "follow_up").await;
    client
        .until(|kind| matches!(kind, EventKind::TurnStarted { .. }))
        .await;
    submit(&mut client, &thread, "more", "steer").await;
    let events = client.until(is_completed).await;
    assert!(
        known(&events).iter().any(|item| matches!(item,
            KnownItem::UserMessage { content, .. } if format!("{content:?}").contains("more"))),
        "the steered message joined the turn"
    );
    let turn_starts = fixture
        .received()
        .iter()
        .filter(|frame| frame["method"] == "turn/start")
        .count();
    assert_eq!(turn_starts, 1);
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

#[tokio::test]
async fn an_interrupt_is_sent_and_the_turn_ends_when_codex_confirms_it() {
    let fixture = Fixture::new();
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("go", TURN));
    steps.extend([
        expect(json!({"method": "turn/interrupt", "params": {
            "threadId": THREAD, "turnId": TURN}})),
        answer(json!({})),
        turn_done(TURN, "interrupted"),
    ]);
    fixture.script(steps);
    let (mut client, thread, turn) = running_turn(&fixture).await;
    client.ok("session/submit", interrupt(&thread, &turn)).await;
    let started = std::time::Instant::now();
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Interrupted);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        !events
            .iter()
            .any(|e| matches!(&e.event.event, EventKind::Error { .. }))
    );
}

#[tokio::test]
async fn an_interrupt_codex_never_confirms_ends_as_interrupted_after_the_window() {
    let fixture = Fixture::new();
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("go", TURN));
    let fake_pid = fixture.path("fake.pid");
    steps.insert(0, json!({"self_pid": fake_pid}));
    steps.extend([
        expect(json!({"method": "turn/interrupt"})),
        answer(json!({})),
        json!({"hang": true}),
    ]);
    fixture.script(steps);
    let (mut client, thread, turn) = running_turn(&fixture).await;
    client.ok("session/submit", interrupt(&thread, &turn)).await;
    let started = std::time::Instant::now();
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Interrupted);
    assert!(
        started.elapsed() >= Duration::from_secs(9),
        "waited the window"
    );
    #[cfg(unix)]
    {
        let fake = pid_in(&fake_pid);
        client
            .ok("session/close", json!({"thread_id": thread}))
            .await;
        assert!(gone(fake).await, "the hung codex outlived the session");
    }
    client.finish().await;
}

#[cfg(unix)]
#[tokio::test]
async fn closing_the_session_and_exiting_leaves_no_codex_or_grandchild_behind() {
    let fixture = Fixture::new();
    let (fake_pid, child_pid) = (fixture.path("fake.pid"), fixture.path("child.pid"));
    let mut steps = vec![
        json!({"self_pid": fake_pid}),
        json!({"grandchild": child_pid}),
    ];
    steps.extend(handshake(("untrusted", "read-only")));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let (fake, child) = (pid_in(&fake_pid), pid_in(&child_pid));
    assert!(alive(fake) && alive(child));
    client
        .ok("session/close", json!({"thread_id": opened.thread_id}))
        .await;
    assert!(gone(fake).await && gone(child).await, "closed session");
    let status = client.finish().await;
    assert!(status.success(), "{status:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn exiting_the_app_server_without_closing_reaps_the_process_group() {
    let fixture = Fixture::new();
    let (fake_pid, child_pid) = (fixture.path("fake.pid"), fixture.path("child.pid"));
    let mut steps = vec![
        json!({"self_pid": fake_pid}),
        json!({"grandchild": child_pid}),
    ];
    steps.extend(handshake(("untrusted", "read-only")));
    fixture.script(steps);
    let mut client = fixture.client();
    open(&mut client, &fixture, None).await;
    let (fake, child) = (pid_in(&fake_pid), pid_in(&child_pid));
    client.finish().await;
    assert!(gone(fake).await && gone(child).await, "app-server exit");
}

#[cfg(unix)]
#[tokio::test]
async fn a_codex_that_never_answers_initialize_times_out_and_is_killed() {
    let fixture = Fixture::new();
    let pid = fixture.path("fake.pid");
    fixture.script(vec![
        json!({"self_pid": pid}),
        expect(json!({"method": "initialize"})),
        json!({"hang": true}),
    ]);
    let mut client = fixture.client_with(&[("CHUI_CODEX_HANDSHAKE_TIMEOUT_MS", "2000")]);
    let error = client
        .call(
            "session/open",
            json!({"cwd": fixture.cwd, "runtime": "codex"}),
        )
        .await
        .unwrap_err();
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("timed out"), "{message}");
    assert!(gone(pid_in(&pid)).await, "the process group was killed");
}

#[tokio::test]
async fn a_codex_that_talks_but_never_answers_initialize_times_out_too() {
    let fixture = Fixture::new();
    fixture.script(vec![
        expect(json!({"method": "initialize"})),
        notify("configWarning", json!({"summary": "hi"})),
        json!({"hang": true}),
    ]);
    let mut client = fixture.client_with(&[("CHUI_CODEX_HANDSHAKE_TIMEOUT_MS", "2000")]);
    let error = client
        .call(
            "session/open",
            json!({"cwd": fixture.cwd, "runtime": "codex"}),
        )
        .await
        .unwrap_err();
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("timed out") && message.contains("initialize"),
        "{message}"
    );
}

#[tokio::test]
async fn reopening_by_path_resumes_the_saved_thread() {
    let fixture = Fixture::new();
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("remember 4217", TURN));
    steps.extend(agent_text("msg-1", TURN, &["noted"]));
    steps.push(turn_done(TURN, "completed"));
    fixture.script(steps);
    let path = {
        let mut client = fixture.client();
        let opened = open(&mut client, &fixture, None).await;
        submit(
            &mut client,
            &opened.thread_id.0,
            "remember 4217",
            "follow_up",
        )
        .await;
        client.until(is_completed).await;
        client
            .ok("session/close", json!({"thread_id": opened.thread_id}))
            .await;
        opened.path
    };
    fixture.script(resumed());
    let mut client = fixture.client();
    let reopened = snapshot(client.ok("session/open", json!({"path": path})).await);
    let text = serde_json::to_string(&reopened.events[0].event).unwrap();
    assert!(
        text.contains("remember 4217") && text.contains("noted"),
        "{text}"
    );
    assert!(
        !reopened
            .events
            .iter()
            .any(|e| matches!(&e.event, EventKind::Error { .. })),
        "a resumed thread reports no error"
    );
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

#[tokio::test]
async fn a_failed_resume_starts_a_new_thread_and_says_so() {
    let fixture = Fixture::new();
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("hello", TURN));
    steps.push(turn_done(TURN, "completed"));
    fixture.script(steps);
    let path = {
        let mut client = fixture.client();
        let opened = open(&mut client, &fixture, None).await;
        submit(&mut client, &opened.thread_id.0, "hello", "follow_up").await;
        client.until(is_completed).await;
        client
            .ok("session/close", json!({"thread_id": opened.thread_id}))
            .await;
        opened.path
    };
    let mut steps = vec![
        expect(json!({"method": "initialize"})),
        answer(json!({"userAgent": "fake/0.171.0 (test)"})),
        expect(json!({"method": "initialized"})),
        expect(json!({"method": "thread/resume", "params": {"threadId": THREAD}})),
        emit(json!({"id": "$id", "error": {"code": -32600, "message": "no rollout found"}})),
        expect(json!({"method": "thread/start"})),
    ];
    steps.push(answer(
        json!({"thread": {"id": "thr-new", "turns": []}, "model": "gpt-fake"}),
    ));
    steps.extend(listings());
    steps.extend([
        expect(json!({"method": "turn/start", "params": {"threadId": "thr-new"}})),
        answer(json!({"turn": {"id": "turn-9", "items": [], "status": "inProgress"}})),
        notify(
            "turn/completed",
            json!({"threadId": "thr-new", "turn": {"id": "turn-9", "items": [], "status": "completed"}}),
        ),
    ]);
    fixture.script(steps);
    let mut client = fixture.client();
    let reopened = snapshot(client.ok("session/open", json!({"path": path})).await);
    assert!(
        reopened.events.iter().any(|e| matches!(&e.event,
            EventKind::Error { message } if message.contains("could not resume"))),
        "{:?}",
        reopened.events
    );
    submit(&mut client, &reopened.thread_id.0, "again", "follow_up").await;
    client.until(is_completed).await;
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

#[tokio::test]
async fn permission_modes_map_to_codex_approval_policy_and_sandbox() {
    let cases = [
        (None, "untrusted", "read-only", "readOnly"),
        (Some("rules"), "untrusted", "read-only", "readOnly"),
        (
            Some("auto"),
            "on-request",
            "workspace-write",
            "workspaceWrite",
        ),
        (
            Some("yolo"),
            "never",
            "danger-full-access",
            "dangerFullAccess",
        ),
    ];
    for (mode, policy, sandbox, policy_type) in cases {
        let fixture = Fixture::new();
        let mut steps = handshake((policy, sandbox));
        steps.push(expect(json!({"method": "turn/start", "params": {
            "approvalPolicy": policy, "sandboxPolicy": {"type": policy_type}}})));
        steps.push(answer(
            json!({"turn": {"id": TURN, "items": [], "status": "inProgress"}}),
        ));
        steps.push(turn_done(TURN, "completed"));
        fixture.script(steps);
        let mut client = fixture.client();
        let opened = open(&mut client, &fixture, mode).await;
        submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
        client.until(is_completed).await;
        assert!(
            fixture.mismatches().is_empty(),
            "{mode:?}: {:?}",
            fixture.mismatches()
        );
        if mode == Some("auto") {
            let start = fixture
                .received()
                .into_iter()
                .find(|frame| frame["method"] == "turn/start")
                .unwrap();
            assert_eq!(start["params"]["approvalsReviewer"], "auto_review");
        }
    }
}

async fn submit_op(client: &mut Client, thread: &str, op: Value) {
    client
        .ok("session/submit", json!({"thread_id": thread, "op": op}))
        .await;
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

async fn until_model(client: &mut Client, id: &str) {
    client
        .until(|kind| matches!(kind, EventKind::SessionUpdated { model, .. } if model.id == id))
        .await;
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

fn steered(turn: &str, text: &str) -> Vec<Value> {
    vec![
        expect(
            json!({"method": "turn/steer", "params": {"threadId": THREAD,
            "expectedTurnId": turn, "input": [{"type": "text", "text": text}]}}),
        ),
        answer(json!({"turnId": turn})),
    ]
}

fn turn_starts(fixture: &Fixture) -> Vec<Value> {
    fixture
        .received()
        .into_iter()
        .filter(|frame| frame["method"] == "turn/start")
        .map(|frame| frame["params"].clone())
        .collect()
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

/// The catalogue and the commands the driver listed, from the snapshot when they arrived before
/// it and from the events after it otherwise.
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

fn codex_model(id: &str, levels: &[ThinkingLevel]) -> ModelInfo {
    ModelInfo {
        provider: "codex".into(),
        id: id.into(),
        context_window: 0,
        reasoning: true,
        thinking_levels: levels.to_vec(),
    }
}

#[tokio::test]
async fn the_model_catalogue_and_the_enabled_skills_are_listed_once_the_thread_opens() {
    use ThinkingLevel::{High, Low, Medium};
    let fixture = Fixture::new();
    fixture.script(handshake(("untrusted", "read-only")));
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let (models, commands) = listed(&mut client, &opened).await;
    assert_eq!(
        models,
        [
            codex_model("gpt-fake", &[Low, Medium, High]),
            codex_model("gpt-lite", &[Low, Medium])
        ]
    );
    let skill = |name: &str, description: &str| CommandInfo {
        name: name.into(),
        description: description.into(),
        argument_hint: None,
        kind: CommandKind::Skill,
    };
    assert_eq!(
        commands,
        [skill("fix", "fix things"), skill("review", "look it over")]
    );
    assert_eq!(
        session_state(&opened.events),
        (codex_model("gpt-fake", &[Low, Medium, High]), Medium),
        "the thread runs on the listed model at its default effort"
    );
    let frames = fixture.received();
    let method = |name: &str| {
        frames
            .iter()
            .find(|frame| frame["method"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(
        method("skills/list")["params"],
        json!({"cwds": [method("thread/start")["params"]["cwd"]]})
    );
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

#[tokio::test]
async fn model_and_effort_switches_go_out_with_the_next_turn_and_are_recorded() {
    let fixture = Fixture::new();
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("one", TURN));
    steps.extend(steered(TURN, "and then"));
    steps.push(turn_done(TURN, "completed"));
    steps.extend(turn_start("two", "turn-2"));
    steps.push(turn_done("turn-2", "completed"));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    let thinking = |level: &str| json!({"kind": "set_thinking", "level": level});
    let model = |id: &str| json!({"kind": "set_model", "provider": "codex", "model": id});

    submit_op(&mut client, &thread, thinking("off")).await;
    let refused = until_error(&mut client, "off").await;
    assert!(refused.contains("low, medium, high"), "{refused}");
    submit_op(&mut client, &thread, thinking("xhigh")).await;
    until_error(&mut client, "xhigh").await;
    submit_op(&mut client, &thread, model("gpt-none")).await;
    until_error(&mut client, "gpt-none").await;
    submit_op(&mut client, &thread, thinking("high")).await;
    until_thinking(&mut client, ThinkingLevel::High).await;
    // gpt-lite has no `high`, so the effort falls back to its default, `low`.
    submit_op(&mut client, &thread, model("gpt-lite")).await;
    until_model(&mut client, "gpt-lite").await;

    submit(&mut client, &thread, "one", "follow_up").await;
    until_turn_started(&mut client).await;
    submit_op(&mut client, &thread, model("gpt-fake")).await;
    until_model(&mut client, "gpt-fake").await;
    submit_op(&mut client, &thread, thinking("medium")).await;
    until_thinking(&mut client, ThinkingLevel::Medium).await;
    submit(&mut client, &thread, "and then", "steer").await;
    client.until(is_completed).await;
    submit(&mut client, &thread, "two", "follow_up").await;
    client.until(is_completed).await;

    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
    let sent: Vec<_> = turn_starts(&fixture)
        .iter()
        .map(|params| (params["model"].clone(), params["effort"].clone()))
        .collect();
    assert_eq!(
        sent,
        [
            (json!("gpt-lite"), json!("low")),
            (json!("gpt-fake"), json!("medium"))
        ]
    );
    let models: Vec<_> = entries(&opened.path, "model_change")
        .iter()
        .map(|entry| entry["model"].clone())
        .collect();
    assert_eq!(
        models,
        [
            json!({"provider": "codex", "model": "gpt-lite"}),
            json!({"provider": "codex", "model": "gpt-fake"})
        ]
    );
    let levels: Vec<_> = entries(&opened.path, "thinking_change")
        .iter()
        .map(|entry| entry["level"].clone())
        .collect();
    assert_eq!(levels, [json!("high"), json!("medium")]);
}

#[tokio::test]
async fn a_permission_switch_goes_out_with_the_next_turn_even_when_made_during_one() {
    let fixture = Fixture::new();
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("one", TURN));
    steps.extend(steered(TURN, "go on"));
    steps.push(turn_done(TURN, "completed"));
    for (text, turn) in [("two", "turn-2"), ("three", "turn-3")] {
        steps.extend(turn_start(text, turn));
        steps.push(turn_done(turn, "completed"));
    }
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    let switch =
        |mode: &str| json!({"kind": "run_command", "name": "permissions", "arguments": mode});
    let switched = |mode: &'static str| {
        move |kind: &EventKind| {
            matches!(kind, EventKind::Extension { name, data }
                if name == PERMISSION_MODE_EVENT && *data == mode)
        }
    };

    submit(&mut client, &thread, "one", "follow_up").await;
    until_turn_started(&mut client).await;
    submit_op(&mut client, &thread, switch("auto")).await;
    client.until(switched("auto")).await;
    submit(&mut client, &thread, "go on", "steer").await;
    client.until(is_completed).await;
    submit(&mut client, &thread, "two", "follow_up").await;
    client.until(is_completed).await;
    submit_op(&mut client, &thread, switch("yolo")).await;
    client.until(switched("yolo")).await;
    submit(&mut client, &thread, "three", "follow_up").await;
    client.until(is_completed).await;

    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
    let policies: Vec<_> = turn_starts(&fixture)
        .iter()
        .map(|params| {
            (
                params["approvalPolicy"].clone(),
                params["approvalsReviewer"].clone(),
                params["sandboxPolicy"].clone(),
            )
        })
        .collect();
    assert_eq!(
        policies,
        [
            (
                json!("untrusted"),
                json!("user"),
                json!({"type": "readOnly"})
            ),
            (
                json!("on-request"),
                json!("auto_review"),
                json!({"type": "workspaceWrite"})
            ),
            (
                json!("never"),
                json!("user"),
                json!({"type": "dangerFullAccess"})
            ),
        ]
    );
}

#[tokio::test]
async fn skill_mentions_reach_codex_as_dollar_names_and_a_skill_command_starts_a_turn() {
    let typed = "/fix the bug, then $review it; not /foo, src/fix or /fixer";
    let sent = "$fix the bug, then $review it; not /foo, src/fix or /fixer";
    let fixture = Fixture::new();
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start(sent, TURN));
    steps.extend(steered(TURN, "and $review"));
    steps.push(turn_done(TURN, "completed"));
    for (text, turn) in [("$fix the tests", "turn-2"), ("$review", "turn-3")] {
        steps.extend(turn_start(text, turn));
        steps.push(turn_done(turn, "completed"));
    }
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    let command = |name: &str, arguments: &str| json!({"kind": "run_command", "name": name, "arguments": arguments});

    submit(&mut client, &thread, typed, "follow_up").await;
    until_turn_started(&mut client).await;
    submit(&mut client, &thread, "and /review", "steer").await;
    client.until(is_completed).await;
    submit_op(&mut client, &thread, command("nope", "")).await;
    until_error(&mut client, "nope").await;
    submit_op(
        &mut client,
        &thread,
        json!({"kind": "command_input", "name": "fix", "data": {}}),
    )
    .await;
    until_error(&mut client, "command input").await;
    submit_op(&mut client, &thread, command("fix", " the tests ")).await;
    client.until(is_completed).await;
    submit_op(&mut client, &thread, command("review", "")).await;
    client.until(is_completed).await;

    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

#[tokio::test]
async fn reopening_restores_the_recorded_model_on_resume_and_the_effort_on_the_next_turn() {
    let fixture = Fixture::new();
    fixture.script(handshake(("untrusted", "read-only")));
    let path = {
        let mut client = fixture.client();
        let opened = open(&mut client, &fixture, None).await;
        let thread = opened.thread_id.0.clone();
        submit_op(
            &mut client,
            &thread,
            json!({"kind": "set_model", "provider": "codex", "model": "gpt-lite"}),
        )
        .await;
        until_model(&mut client, "gpt-lite").await;
        submit_op(
            &mut client,
            &thread,
            json!({"kind": "set_thinking", "level": "medium"}),
        )
        .await;
        until_thinking(&mut client, ThinkingLevel::Medium).await;
        client
            .ok("session/close", json!({"thread_id": thread}))
            .await;
        opened.path
    };
    let mut response = thread_response();
    response["model"] = json!("gpt-lite");
    let mut steps = resume_handshake(json!({"threadId": THREAD, "model": "gpt-lite"}), response);
    steps.extend(turn_start("again", TURN));
    steps.push(turn_done(TURN, "completed"));
    fixture.script(steps);

    let mut client = fixture.client();
    let reopened = snapshot(client.ok("session/open", json!({"path": path})).await);
    let (model, thinking) = session_state(&reopened.events);
    assert_eq!(
        (model.id.as_str(), thinking),
        ("gpt-lite", ThinkingLevel::Medium),
        "gpt-lite defaults to low; medium is the recorded effort"
    );
    submit(&mut client, &reopened.thread_id.0, "again", "follow_up").await;
    client.until(is_completed).await;
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
    let sent = &turn_starts(&fixture)[0];
    assert_eq!(
        (&sent["model"], &sent["effort"]),
        (&json!("gpt-lite"), &json!("medium"))
    );
}

#[tokio::test]
async fn one_turn_maps_text_reasoning_commands_file_changes_and_usage() {
    let fixture = Fixture::new();
    let ids = |turn: &str| json!({"threadId": THREAD, "turnId": turn});
    let with = |extra: Value| {
        let mut base = ids(TURN);
        base.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        base
    };
    let usage = |last: Value, total: Value, turn: &str| {
        notify(
            "thread/tokenUsage/updated",
            json!({"threadId": THREAD, "turnId": turn, "tokenUsage": {
                "last": last, "total": total, "modelContextWindow": 258400}}),
        )
    };
    let counts = |input: u64, cached: u64, output: u64, reasoning: u64| {
        json!({"totalTokens": input + output, "inputTokens": input, "cachedInputTokens": cached,
            "cacheWriteInputTokens": 0, "outputTokens": output, "reasoningOutputTokens": reasoning})
    };
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("go", TURN));
    // A replay of an earlier turn's usage after the request, as a resume does.
    steps.push(usage(counts(500, 0, 5, 0), counts(500, 0, 5, 0), "turn-0"));
    steps.extend([
        notify(
            "item/started",
            with(json!({"item": {"type": "userMessage", "id": "u1",
            "content": [{"type": "text", "text": "go"}]}})),
        ),
        notify(
            "item/completed",
            with(json!({"item": {"type": "userMessage", "id": "u1",
            "content": [{"type": "text", "text": "go"}]}})),
        ),
        notify(
            "item/started",
            with(json!({"item": {"type": "reasoning", "id": "rs1",
            "summary": [], "content": []}})),
        ),
        notify(
            "item/reasoning/summaryPartAdded",
            with(json!({"itemId": "rs1", "summaryIndex": 0})),
        ),
        notify(
            "item/reasoning/summaryTextDelta",
            with(json!({"itemId": "rs1", "summaryIndex": 0,
            "delta": "Checking "})),
        ),
        notify(
            "item/reasoning/summaryTextDelta",
            with(json!({"itemId": "rs1", "summaryIndex": 0,
            "delta": "files"})),
        ),
        notify(
            "item/completed",
            with(json!({"item": {"type": "reasoning", "id": "rs1",
            "summary": ["Checking files"], "content": []}})),
        ),
    ]);
    steps.extend(agent_text("msg1", TURN, &["Let ", "me ", "run it."]));
    steps.extend([
        notify(
            "item/started",
            with(json!({"item": {"type": "commandExecution", "id": "exec1",
            "command": "/bin/bash -lc \"ls missing\"",
            "commandActions": [{"type": "unknown", "command": "ls missing"}],
            "cwd": "/x", "status": "inProgress"}})),
        ),
        notify(
            "item/commandExecution/outputDelta",
            with(json!({"itemId": "exec1",
            "delta": "ls: missing\n"})),
        ),
        notify(
            "item/completed",
            with(json!({"item": {"type": "commandExecution", "id": "exec1",
            "command": "/bin/bash -lc \"ls missing\"",
            "commandActions": [{"type": "unknown", "command": "ls missing"}],
            "cwd": "/x", "status": "failed", "aggregatedOutput": "ls: missing\n", "exitCode": 1,
            "durationMs": 120}})),
        ),
        notify(
            "item/started",
            with(json!({"item": {"type": "fileChange", "id": "fc1",
            "status": "inProgress", "changes": [
                {"path": "/x/new.txt", "kind": {"type": "add"}, "diff": "hello\n"},
                {"path": "/x/old.txt", "kind": {"type": "update", "move_path": null},
                 "diff": "@@ -1,3 +1,3 @@\n keep\n-before\n+after\n tail\n"},
                {"path": "/x/gone.txt", "kind": {"type": "delete"}, "diff": "bye\n"}]}})),
        ),
        notify(
            "item/completed",
            with(json!({"item": {"type": "fileChange", "id": "fc1",
            "status": "completed", "changes": [
                {"path": "/x/new.txt", "kind": {"type": "add"}, "diff": "hello\n"},
                {"path": "/x/old.txt", "kind": {"type": "update", "move_path": null},
                 "diff": "@@ -1,3 +1,3 @@\n keep\n-before\n+after\n tail\n"},
                {"path": "/x/gone.txt", "kind": {"type": "delete"}, "diff": "bye\n"}]}})),
        ),
        notify(
            "item/started",
            with(json!({"item": {"type": "mcpToolCall", "id": "mcp1",
            "server": "docs", "tool": "search", "arguments": {"q": "x"}, "status": "inProgress"}})),
        ),
        notify(
            "item/completed",
            with(json!({"item": {"type": "mcpToolCall", "id": "mcp1",
            "server": "docs", "tool": "search", "arguments": {"q": "x"}, "status": "completed",
            "result": {"content": [{"type": "text", "text": "found it"}]}}})),
        ),
        usage(counts(1000, 400, 50, 20), counts(1000, 400, 50, 20), TURN),
        usage(counts(1200, 1000, 30, 0), counts(2200, 1400, 80, 20), TURN),
        usage(counts(1200, 1000, 30, 0), counts(2200, 1400, 80, 20), TURN),
        turn_done(TURN, "completed"),
    ]);
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    let events = client.until(is_completed).await;

    let items = known(&events);
    let call = |id: &str| {
        items
            .iter()
            .find_map(|item| match item {
                KnownItem::ToolCall {
                    id: found,
                    name,
                    arguments,
                } if found.0 == id => Some((name.clone(), arguments.clone())),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no call {id}: {items:?}"))
    };
    let result = |call_id: &str| {
        items
            .iter()
            .find_map(|item| match item {
                KnownItem::ToolResult {
                    call_id: found,
                    output,
                    is_error,
                    ..
                } if found.0 == call_id => Some((output.clone(), *is_error)),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no result for {call_id}: {items:?}"))
    };
    assert_eq!(
        items
            .iter()
            .filter(|item| matches!(item, KnownItem::UserMessage { .. }))
            .count(),
        1,
        "only the session's own user message: Codex's echo is skipped"
    );
    assert!(items.iter().any(|item| matches!(item,
        KnownItem::Thinking { text, .. } if text == "Checking files")));
    let text_events: Vec<_> = events
        .iter()
        .filter(|e| match &e.event.event {
            EventKind::ItemStarted { item, .. } => {
                matches!(item, Item::Known(KnownItem::AssistantMessage { .. }))
            }
            EventKind::ItemDelta { item_id, .. } => item_id.0 == "msg1",
            _ => false,
        })
        .collect();
    assert!(matches!(
        text_events[0].event.event,
        EventKind::ItemStarted { .. }
    ));
    let streamed: String = text_events
        .iter()
        .filter_map(|e| match &e.event.event {
            EventKind::ItemDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(streamed, "Let me run it.");
    assert!(
        text_events.len() <= 4,
        "deltas are merged: {}",
        text_events.len()
    );

    assert_eq!(
        call("exec1"),
        ("bash".into(), json!({"command": "ls missing"}))
    );
    let (output, is_error) = result("exec1");
    assert!(is_error);
    let text = output["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("ls: missing") && text.ends_with("Command exited with code 1"),
        "{text}"
    );

    assert_eq!(
        call("fc1"),
        (
            "write".into(),
            json!({"path": "/x/new.txt", "content": "hello\n"})
        )
    );
    assert_eq!(
        call("fc1:1"),
        (
            "edit".into(),
            json!({"path": "/x/old.txt", "edits": [{"oldText": "before\n", "newText": "after\n"}]})
        )
    );
    assert_eq!(call("fc1:2").0, "delete");
    assert!(!result("fc1").1 && !result("fc1:1").1);

    assert_eq!(
        call("mcp1"),
        ("mcp__docs__search".into(), json!({"q": "x"}))
    );
    let (output, is_error) = result("mcp1");
    assert!(!is_error && output["content"][0]["text"] == "found it");

    let usages: Vec<TokenUsage> = events
        .iter()
        .filter_map(|e| match &e.event.event {
            EventKind::TokenUsageUpdated { last, .. } => Some(*last),
            _ => None,
        })
        .collect();
    assert_eq!(
        usages.len(),
        2,
        "a repeat and a replay are not new requests: {usages:?}"
    );
    assert_eq!(
        (
            usages[0].input,
            usages[0].cache_read,
            usages[0].output,
            usages[0].reasoning
        ),
        (600, 400, 50, 20)
    );
    assert_eq!(
        (usages[1].input, usages[1].cache_read, usages[1].output),
        (200, 1000, 30)
    );
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

// Sub-agent frames shaped like t3code's codex_transcript fixtures (Codex 0.156.1).

const CHILD: &str = "thr-child";
const CHILD_TURN: &str = "turn-c";
const SPAWN: &str = "call-spawn";

fn on(thread: &str, turn: &str, extra: Value) -> Value {
    let mut base = json!({"threadId": thread, "turnId": turn});
    base.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    base
}

fn v2_spawn() -> Vec<Value> {
    let item = json!({"type": "subAgentActivity", "id": SPAWN, "kind": "started",
        "agentThreadId": CHILD, "agentPath": "/root/fixture_writer"});
    vec![
        notify("item/started", on(THREAD, TURN, json!({"item": item}))),
        notify("item/completed", on(THREAD, TURN, json!({"item": item}))),
    ]
}

fn v2_finished() -> Vec<Value> {
    let item = json!({"type": "subAgentActivity", "id": "subagent-completed-turn-c",
        "kind": "completed", "agentThreadId": CHILD, "agentPath": "/root/fixture_writer"});
    vec![
        notify("item/started", on(THREAD, TURN, json!({"item": item}))),
        notify("item/completed", on(THREAD, TURN, json!({"item": item}))),
    ]
}

fn child_turn_started() -> Value {
    notify(
        "turn/started",
        json!({"threadId": CHILD, "turn": {"id": CHILD_TURN, "items": [], "status": "inProgress"}}),
    )
}

const CHILD_COMMAND: &str = "/bin/bash -lc \"touch sub.txt\"";

fn child_command(status: &str) -> Value {
    json!({"type": "commandExecution", "id": "exec-sub", "command": CHILD_COMMAND,
        "commandActions": [{"type": "unknown", "command": "touch sub.txt"}],
        "cwd": "/x", "status": status, "aggregatedOutput": if status == "completed" { json!("") } else { Value::Null },
        "exitCode": if status == "completed" { json!(0) } else { Value::Null }})
}

fn child_command_started() -> Value {
    notify(
        "item/started",
        on(
            CHILD,
            CHILD_TURN,
            json!({"item": child_command("inProgress")}),
        ),
    )
}

fn child_command_completed() -> Value {
    notify(
        "item/completed",
        on(
            CHILD,
            CHILD_TURN,
            json!({"item": child_command("completed")}),
        ),
    )
}

fn child_approval(id: u64) -> Value {
    emit(
        json!({"id": id, "method": "item/commandExecution/requestApproval", "params": {
        "kind": "command", "threadId": CHILD, "turnId": CHILD_TURN, "itemId": "exec-sub",
        "startedAtMs": 1, "command": CHILD_COMMAND,
        "commandActions": [{"type": "unknown", "command": "touch sub.txt"}], "cwd": "/x"}}),
    )
}

fn child_message(text: &str) -> Vec<Value> {
    let item = json!({"type": "agentMessage", "id": "msg-sub", "text": text});
    vec![
        notify(
            "item/started",
            on(
                CHILD,
                CHILD_TURN,
                json!({"item": {"type": "agentMessage", "id": "msg-sub", "text": ""}}),
            ),
        ),
        notify(
            "item/agentMessage/delta",
            on(
                CHILD,
                CHILD_TURN,
                json!({"itemId": "msg-sub", "delta": text}),
            ),
        ),
        notify(
            "item/completed",
            on(CHILD, CHILD_TURN, json!({"item": item})),
        ),
    ]
}

fn child_turn_done(text: &str) -> Value {
    notify(
        "turn/completed",
        json!({"threadId": CHILD, "turn": {"id": CHILD_TURN, "status": "completed",
            "items": [{"type": "agentMessage", "id": "msg-sub", "text": text}]}}),
    )
}

fn custom_items(events: &[tiechui_protocol::Event]) -> (Vec<SubagentItem>, Vec<SubagentStatus>) {
    let mut children = vec![];
    let mut statuses = vec![];
    for event in events {
        if let EventKind::ItemCompleted { item, .. } = &event.event {
            if let Some(child) = SubagentItem::from_item(item) {
                children.push(child);
            }
            if let Some(status) = SubagentStatus::from_item(item) {
                statuses.push(status);
            }
        }
    }
    (children, statuses)
}

fn structure(events: &[tiechui_protocol::Event]) -> Vec<(String, String, Vec<String>)> {
    let describe = |item: &Item| match item {
        Item::Known(known) => serde_json::to_value(known).unwrap()["id"].to_string(),
        Item::Custom { kind, payload } => format!("{kind}:{}", payload["id"]),
    };
    let mut turns: Vec<(String, String, Vec<String>)> = vec![];
    for event in events {
        match &event.event {
            EventKind::ThreadStarted { thread, .. } => {
                turns = thread
                    .turns
                    .iter()
                    .map(|t| {
                        (
                            t.id.0.clone(),
                            format!("{:?}", t.status),
                            t.items.iter().map(describe).collect(),
                        )
                    })
                    .collect();
            }
            EventKind::TurnStarted { turn } => {
                turns.push((turn.id.0.clone(), "InProgress".into(), vec![]));
            }
            EventKind::ItemCompleted { turn_id, item } => {
                let turn = turns
                    .iter_mut()
                    .find(|t| t.0 == turn_id.0)
                    .expect("item outside a turn");
                let described = describe(item);
                match turn.2.iter().position(|d| *d == described) {
                    Some(index) => turn.2[index] = described,
                    None => turn.2.push(described),
                }
            }
            EventKind::TurnCompleted { turn } => {
                let slot = turns
                    .iter_mut()
                    .find(|t| t.0 == turn.id.0)
                    .expect("unknown turn");
                slot.1 = format!("{:?}", turn.status);
                slot.2 = turn.items.iter().map(describe).collect();
            }
            _ => {}
        }
    }
    turns
}

#[tokio::test]
async fn a_subagent_that_finishes_first_folds_under_its_spawn_and_the_parent_turn_runs_on() {
    let fixture = Fixture::new();
    let mut steps = handshake(("never", "danger-full-access"));
    steps.extend(turn_start("go", TURN));
    steps.extend(v2_spawn());
    steps.extend([
        child_turn_started(),
        child_command_started(),
        child_command_completed(),
    ]);
    steps.extend(child_message("Written."));
    steps.push(child_turn_done("Written."));
    steps.extend(v2_finished());
    steps.push(json!({"sleep_ms": 300}));
    steps.extend(agent_text("msg-root", TURN, &["Written."]));
    steps.push(turn_done(TURN, "completed"));
    fixture.script(steps);

    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("yolo")).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    let events = client.until(is_completed).await;
    let live: Vec<_> = events.iter().map(|e| e.event.clone()).collect();
    assert_eq!(
        events
            .iter()
            .filter(|e| is_completed(&e.event.event))
            .count(),
        1,
        "only the root thread's turn/completed ends a turn"
    );
    let (children, statuses) = custom_items(&live);
    assert!(
        children
            .iter()
            .all(|c| c.parent_call_id.0 == SPAWN && c.agent_id == CHILD),
        "{children:?}"
    );
    let ids: Vec<_> = children.iter().map(|c| c.id.0.as_str()).collect();
    assert_eq!(
        ids,
        [
            "thr-child:exec-sub",
            "thr-child:exec-sub:result",
            "thr-child:msg-sub"
        ],
        "completed items only, call before result"
    );
    let states: Vec<_> = statuses.iter().map(|s| s.state).collect();
    assert_eq!(states, [SubagentState::Running, SubagentState::Completed]);
    assert_eq!(statuses[0].name.as_deref(), Some("fixture_writer"));
    assert_eq!(statuses[1].message.as_deref(), Some("Written."));
    let root = known(&events);
    assert!(root.iter().any(|item| matches!(item,
        KnownItem::ToolCall { id, name, .. } if id.0 == SPAWN && name == "spawn_agent")));
    assert!(root.iter().any(|item| matches!(item,
        KnownItem::AssistantMessage { text, .. } if text == "Written.")));
    let turns = structure(&live);
    assert_eq!(turns.len(), 1, "{turns:?}");
    assert_eq!(
        turns[0]
            .2
            .iter()
            .filter(|d| d.contains("subagent-status"))
            .count(),
        1,
        "{turns:?}"
    );
    assert_eq!(completed_status(&events), TurnStatus::Completed);
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

fn late_subagent_script(fixture: &Fixture) {
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("go", TURN));
    steps.extend(v2_spawn());
    steps.push(child_turn_started());
    steps.extend(agent_text("msg-root", TURN, &["spawned"]));
    steps.push(turn_done(TURN, "completed"));
    steps.push(json!({"sleep_ms": 200}));
    steps.push(child_command_started());
    steps.push(child_approval(9));
    steps.push(expect(json!({"id": 9, "result": {"decision": "accept"}})));
    steps.push(child_command_completed());
    steps.extend(child_message("Written."));
    steps.push(child_turn_done("Written."));
    steps.extend(v2_finished());
    fixture.script(steps);
}

#[tokio::test]
async fn output_and_an_approval_of_a_subagent_after_the_root_turn_open_one_derived_turn() {
    let fixture = Fixture::new();
    late_subagent_script(&fixture);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    let thread = opened.thread_id.0.clone();
    let mut live = opened.events.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let first = client.until(is_completed).await;
    let root_turn = match &first.last().unwrap().event.event {
        EventKind::TurnCompleted { turn } => turn.id.clone(),
        _ => unreachable!(),
    };
    live.extend(first.iter().map(|e| e.event.clone()));
    let seen = client
        .until(|kind| matches!(kind, EventKind::ApprovalRequested { .. }))
        .await;
    live.extend(seen.iter().map(|e| e.event.clone()));
    let EventKind::ApprovalRequested {
        turn_id: Some(approval_turn),
        approval_id,
        request:
            ApprovalRequest::Permission {
                actor,
                call_id,
                values,
                ..
            },
    } = seen.last().unwrap().event.event.clone()
    else {
        panic!("not a permission approval");
    };
    assert_eq!(actor.as_deref(), Some(CHILD));
    assert_eq!(call_id, "thr-child:exec-sub");
    assert_eq!(values, ["touch sub.txt"]);
    let (children, _) = custom_items(&live);
    assert!(
        children.iter().any(|c| matches!(&c.item,
            KnownItem::ToolCall { id, arguments, .. }
                if id.0 == call_id && arguments["command"] == "touch sub.txt")),
        "the approval's call_id is the id of the child ToolCall with the same command: {children:?}"
    );
    let derived_start = live
        .iter()
        .find_map(|e| match &e.event {
            EventKind::TurnStarted { turn }
                if turn.id != root_turn && turn.id.0 == approval_turn.0 =>
            {
                Some(turn.clone())
            }
            _ => None,
        })
        .expect("a derived turn opened");
    assert_ne!(derived_start.id, root_turn);
    let mid = snapshot(
        client
            .ok("session/snapshot", json!({"thread_id": thread}))
            .await,
    );
    assert_eq!(structure(&mid.events), structure(&live));
    assert_eq!(structure(&live)[1].1, "InProgress");

    client
        .ok(
            "session/submit",
            approval_response(&thread, &approval_id.0, json!({"kind": "allow_once"})),
        )
        .await;
    let done = client.until(is_completed).await;
    live.extend(done.iter().map(|e| e.event.clone()));
    assert_eq!(completed_status(&done), TurnStatus::Completed);
    assert_eq!(
        responses(&fixture, 9).len(),
        1,
        "one answer, on the child's request id"
    );

    let turns = structure(&live);
    assert_eq!(turns.len(), 2, "{turns:?}");
    assert_eq!(turns[1].1, "Completed");
    let after = snapshot(
        client
            .ok("session/snapshot", json!({"thread_id": thread}))
            .await,
    );
    assert_eq!(structure(&after.events), turns, "snapshot vs live");
    let EventKind::ThreadStarted {
        thread: started, ..
    } = &after.events[0].event
    else {
        panic!()
    };
    let marker = DerivedTurnMarker::from_item(&started.turns[1].items[0]).expect("marker");
    assert_eq!(marker.derived_from, root_turn);
    assert_eq!(marker.reason, DerivedReason::Subagent);
    let (_, statuses) = custom_items(&live);
    let last = statuses.last().unwrap();
    assert_eq!(last.state, SubagentState::Completed);
    assert!(
        statuses.iter().any(|s| s.state == SubagentState::Waiting),
        "the agent waits while its approval is open: {statuses:?}"
    );
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );

    client
        .ok("session/close", json!({"thread_id": thread}))
        .await;
    fixture.script(resumed());
    let mut again = fixture.client();
    let reopened = snapshot(again.ok("session/open", json!({"path": opened.path})).await);
    let keep = |turns: Vec<(String, String, Vec<String>)>| -> Vec<(String, String, Vec<String>)> {
        turns
            .into_iter()
            .map(|(id, status, items)| {
                let items = items
                    .into_iter()
                    .map(|item| {
                        if item.starts_with("runtime.") || item == "\"call-spawn\"" {
                            item
                        } else {
                            "known".into()
                        }
                    })
                    .collect();
                (id, status, items)
            })
            .collect()
    };
    assert_eq!(
        keep(structure(&reopened.events)),
        keep(turns),
        "reopened vs live"
    );
}

#[tokio::test]
async fn a_followup_turn_started_right_after_turn_completed_is_not_cut_short_by_a_subagent() {
    let fixture = Fixture::new();
    let mut steps = handshake(("untrusted", "read-only"));
    steps.extend(turn_start("go", TURN));
    steps.extend(v2_spawn());
    steps.push(child_turn_started());
    steps.push(turn_done(TURN, "completed"));
    steps.extend(turn_start("again", "turn-2"));
    steps.extend(child_message("late"));
    steps.push(child_turn_done("late"));
    steps.push(json!({"sleep_ms": 300}));
    steps.extend(agent_text("msg-2", "turn-2", &["second"]));
    steps.push(turn_done("turn-2", "completed"));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, None).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    submit(&mut client, &opened.thread_id.0, "again", "follow_up").await;
    let first = client.until(is_completed).await;
    assert_eq!(completed_status(&first), TurnStatus::Completed);
    let second = client.until(is_completed).await;
    assert_eq!(completed_status(&second), TurnStatus::Completed);
    assert!(
        known(&second).iter().any(|item| matches!(item,
        KnownItem::AssistantMessage { text, .. } if text == "second")),
        "the second turn ran to its own end"
    );
    assert!(
        fixture.mismatches().is_empty(),
        "{:?}",
        fixture.mismatches()
    );
}

#[tokio::test]
async fn collab_tool_calls_spawn_and_wait_map_to_tool_calls_and_agent_states() {
    let fixture = Fixture::new();
    let mut steps = handshake(("never", "danger-full-access"));
    steps.extend(turn_start("go", TURN));
    let spawn = |status: &str, receivers: Value, states: Value| {
        json!({"type": "collabAgentToolCall", "id": "exec-spawn", "tool": "spawnAgent",
            "status": status, "senderThreadId": THREAD, "receiverThreadIds": receivers,
            "prompt": "Read package.json", "model": "gpt-x", "reasoningEffort": "medium",
            "agentsStates": states})
    };
    steps.push(notify(
        "item/started",
        on(
            THREAD,
            TURN,
            json!({"item": spawn("inProgress", json!([]), json!({}))}),
        ),
    ));
    steps.push(notify(
        "item/completed",
        on(
            THREAD,
            TURN,
            json!({"item": spawn("completed", json!([CHILD]),
            json!({CHILD: {"status": "pendingInit", "message": null}}))}),
        ),
    ));
    steps.push(child_turn_started());
    steps.extend(child_message("summary"));
    steps.push(child_turn_done("summary"));
    let wait = |status: &str, states: Value| {
        json!({"type": "collabAgentToolCall", "id": "exec-wait", "tool": "wait",
            "status": status, "senderThreadId": THREAD, "receiverThreadIds": [CHILD],
            "prompt": null, "model": null, "reasoningEffort": null, "agentsStates": states})
    };
    steps.push(notify(
        "item/started",
        on(THREAD, TURN, json!({"item": wait("inProgress", json!({}))})),
    ));
    steps.push(notify(
        "item/completed",
        on(
            THREAD,
            TURN,
            json!({"item": wait("completed",
            json!({CHILD: {"status": "completed", "message": "summary"}}))}),
        ),
    ));
    steps.push(turn_done(TURN, "completed"));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("yolo")).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    let events = client.until(is_completed).await;
    let live: Vec<_> = events.iter().map(|e| e.event.clone()).collect();
    let calls: Vec<_> = known(&events)
        .into_iter()
        .filter_map(|item| match item {
            KnownItem::ToolCall { id, name, .. } => Some((id.0, name)),
            _ => None,
        })
        .collect();
    assert!(
        calls.contains(&("exec-spawn".into(), "spawn_agent".into())),
        "{calls:?}"
    );
    assert!(
        calls.contains(&("exec-wait".into(), "wait".into())),
        "{calls:?}"
    );
    let (children, statuses) = custom_items(&live);
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].parent_call_id.0, "exec-spawn");
    assert_eq!(
        statuses.iter().map(|s| s.state).collect::<Vec<_>>(),
        [SubagentState::Running, SubagentState::Completed]
    );
    assert_eq!(structure(&live).len(), 1);
}

#[tokio::test]
async fn a_subagent_spawned_by_a_subagent_folds_under_the_nested_spawn_call() {
    let fixture = Fixture::new();
    let mut steps = handshake(("never", "danger-full-access"));
    steps.extend(turn_start("go", TURN));
    steps.extend(v2_spawn());
    steps.push(child_turn_started());
    let nested = json!({"type": "subAgentActivity", "id": "call-nested", "kind": "started",
        "agentThreadId": "thr-grand", "agentPath": "/root/fixture_writer/grand"});
    steps.push(notify(
        "item/started",
        on(CHILD, CHILD_TURN, json!({"item": nested})),
    ));
    steps.push(notify(
        "turn/started",
        json!({"threadId": "thr-grand", "turn": {"id": "turn-g", "items": [], "status": "inProgress"}}),
    ));
    steps.push(notify(
        "item/completed",
        on(
            "thr-grand",
            "turn-g",
            json!({"item": {"type": "agentMessage", "id": "msg-g", "text": "hi"}}),
        ),
    ));
    steps.push(notify(
        "turn/completed",
        json!({"threadId": "thr-grand", "turn": {"id": "turn-g", "status": "completed", "items": []}}),
    ));
    steps.push(child_turn_done("done"));
    steps.push(turn_done(TURN, "completed"));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("yolo")).await;
    submit(&mut client, &opened.thread_id.0, "go", "follow_up").await;
    let events = client.until(is_completed).await;
    let live: Vec<_> = events.iter().map(|e| e.event.clone()).collect();
    let (children, statuses) = custom_items(&live);
    let nested_call = children
        .iter()
        .find(|c| c.id.0 == "thr-child:call-nested")
        .expect("the nested spawn is a child tool call");
    assert_eq!(nested_call.parent_call_id.0, SPAWN);
    let grand = children
        .iter()
        .find(|c| c.id.0 == "thr-grand:msg-g")
        .expect("the nested agent's text");
    assert_eq!(grand.parent_call_id.0, "thr-child:call-nested");
    assert_eq!(grand.agent_id, "thr-grand");
    assert!(
        statuses
            .iter()
            .any(|s| s.agent_id == "thr-grand" && s.parent_call_id.0 == "thr-child:call-nested")
    );
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test]
async fn a_turn_that_edits_files_can_be_undone_from_its_start_of_turn_snapshot() {
    let fixture = Fixture::new();
    git(&fixture.cwd, &["init", "-q"]);
    std::fs::create_dir_all(fixture.cwd.join("src")).unwrap();
    std::fs::write(fixture.cwd.join("src/hello.ts"), "original\n").unwrap();
    git(&fixture.cwd, &["add", "."]);
    git(&fixture.cwd, &["commit", "-q", "-m", "init"]);
    let mut steps = handshake(("never", "danger-full-access"));
    steps.extend(turn_start("edit", TURN));
    steps.push(json!({"write": {"path": "src/hello.ts", "content": "changed by codex\n"}}));
    steps.push(json!({"write": {"path": "new.txt", "content": "new\n"}}));
    steps.push(turn_done(TURN, "completed"));
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
    end_snapshot_is_ready(&mut client, &thread, &item).await;
    assert_eq!(
        std::fs::read_to_string(fixture.cwd.join("src/hello.ts")).unwrap(),
        "changed by codex\n"
    );

    let rewind = client
        .ok(
            "turn/rewind_check",
            json!({"thread_id": thread, "item_id": item}),
        )
        .await;
    assert_eq!(rewind["mode"], "snapshot", "{rewind}");
    assert_eq!(rewind["files"], json!([]));
    assert!(
        rewind["snapshot_ref"]
            .as_str()
            .unwrap()
            .starts_with("refs/tiechui/checkpoints/"),
        "{rewind}"
    );
    let check = client
        .ok("turn/check", json!({"thread_id": thread, "item_id": item}))
        .await;
    assert_eq!(
        check,
        json!({"safe": ["new.txt", "src/hello.ts"], "unsafe": [], "ignored": []})
    );
    let undone = client
        .ok("turn/undo", json!({"thread_id": thread, "item_id": item}))
        .await;
    assert_eq!(
        undone,
        json!({"changes": [
            {"path": "new.txt", "before": null, "after": "new\n"},
            {"path": "src/hello.ts", "before": "original\n", "after": "changed by codex\n"}],
            "reverted": true})
    );
    assert_eq!(undone["reverted"], true, "{undone}");
    assert_eq!(
        std::fs::read_to_string(fixture.cwd.join("src/hello.ts")).unwrap(),
        "original\n"
    );
    assert!(!fixture.cwd.join("new.txt").exists());
    assert_eq!(
        git(&fixture.cwd, &["status", "--porcelain"]),
        "",
        "no stray changes"
    );
}

#[tokio::test]
async fn a_failed_snapshot_is_recorded_as_missing_and_the_turn_still_runs() {
    let fixture = Fixture::new(); // not a git repository
    let mut steps = handshake(("never", "danger-full-access"));
    steps.extend(turn_start("go", TURN));
    steps.extend(agent_text("msg-1", TURN, &["ok"]));
    steps.push(turn_done(TURN, "completed"));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("yolo")).await;
    let thread = opened.thread_id.0.clone();
    submit(&mut client, &thread, "go", "follow_up").await;
    let events = client.until(is_completed).await;
    assert_eq!(completed_status(&events), TurnStatus::Completed);
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
    let error = client
        .call("turn/undo", json!({"thread_id": thread, "item_id": item}))
        .await
        .unwrap_err();
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("snapshot"), "{message}");
}

fn repo_with_three_files(fixture: &Fixture) {
    git(&fixture.cwd, &["init", "-q"]);
    for name in ["a", "b", "c"] {
        std::fs::write(
            fixture.cwd.join(format!("{name}.txt")),
            format!("{name}0\n"),
        )
        .unwrap();
    }
    git(&fixture.cwd, &["add", "."]);
    git(&fixture.cwd, &["commit", "-q", "-m", "init"]);
}

fn write_step(path: &str, content: &str) -> Value {
    json!({"write": {"path": path, "content": content}})
}

fn read_file(fixture: &Fixture, name: &str) -> String {
    std::fs::read_to_string(fixture.cwd.join(name)).unwrap()
}

fn script_turns(fixture: &Fixture, turns: &[&[(&str, &str)]]) {
    let mut steps = handshake(("never", "danger-full-access"));
    for (index, writes) in turns.iter().enumerate() {
        let id = format!("turn-{}", index + 1);
        steps.extend(turn_start(&format!("turn {}", index + 1), &id));
        steps.extend(
            writes
                .iter()
                .map(|(path, content)| write_step(path, content)),
        );
        steps.push(turn_done(&id, "completed"));
    }
    fixture.script(steps);
}

async fn run_turn(client: &mut Client, thread: &str, text: &str) -> String {
    submit(client, thread, text, "follow_up").await;
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
    end_snapshot_is_ready(client, thread, &item).await;
    item
}

async fn end_snapshot_is_ready(client: &mut Client, thread: &str, item: &str) {
    if let Err(error) = client.call("turn/check", at(thread, item)).await {
        panic!("turn/check right after TurnCompleted: {error}");
    }
}

fn at(thread: &str, item: &str) -> Value {
    json!({"thread_id": thread, "item_id": item})
}

#[tokio::test]
async fn a_turns_changes_are_its_own_and_undoing_it_leaves_later_turns_alone() {
    let fixture = Fixture::new();
    repo_with_three_files(&fixture);
    script_turns(
        &fixture,
        &[
            &[("a.txt", "a1\n")],
            &[("b.txt", "b2\n"), ("new.txt", "n\n")],
        ],
    );
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("yolo")).await;
    let thread = opened.thread_id.0.clone();
    let first = run_turn(&mut client, &thread, "turn 1").await;
    let second = run_turn(&mut client, &thread, "turn 2").await;

    let changes = client.ok("turn/changes", at(&thread, &first)).await;
    assert_eq!(
        changes,
        json!({"changes": [{"path": "a.txt", "before": "a0\n", "after": "a1\n"}],
            "reverted": false})
    );
    let later = client.ok("turn/changes", at(&thread, &second)).await;
    assert_eq!(
        later,
        json!({"changes": [
            {"path": "b.txt", "before": "b0\n", "after": "b2\n"},
            {"path": "new.txt", "before": null, "after": "n\n"}],
            "reverted": false})
    );
    let check = client.ok("turn/check", at(&thread, &first)).await;
    assert_eq!(
        check,
        json!({"safe": ["a.txt"], "unsafe": [], "ignored": []})
    );

    let undone = client.ok("turn/undo", at(&thread, &first)).await;
    assert_eq!(undone["reverted"], true, "{undone}");
    assert_eq!(read_file(&fixture, "a.txt"), "a0\n");
    assert_eq!(read_file(&fixture, "b.txt"), "b2\n", "turn 2 is untouched");
    assert_eq!(read_file(&fixture, "new.txt"), "n\n");
}

#[tokio::test]
async fn undo_is_refused_for_a_file_edited_after_the_turn_and_names_it() {
    let fixture = Fixture::new();
    repo_with_three_files(&fixture);
    script_turns(&fixture, &[&[("a.txt", "a1\n"), ("b.txt", "b1\n")]]);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("yolo")).await;
    let thread = opened.thread_id.0.clone();
    let item = run_turn(&mut client, &thread, "turn 1").await;
    std::fs::write(fixture.cwd.join("a.txt"), "edited by hand\n").unwrap();

    let check = client.ok("turn/check", at(&thread, &item)).await;
    assert_eq!(
        check,
        json!({"safe": ["b.txt"], "unsafe": ["a.txt"], "ignored": []})
    );
    let error = client
        .call("turn/undo", at(&thread, &item))
        .await
        .unwrap_err();
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("changed since the turn") && message.contains("a.txt"),
        "{message}"
    );
    assert_eq!(read_file(&fixture, "a.txt"), "edited by hand\n");
    assert_eq!(read_file(&fixture, "b.txt"), "b1\n", "nothing is restored");
}

#[tokio::test]
async fn undo_never_touches_a_file_the_turn_did_not_change() {
    let fixture = Fixture::new();
    repo_with_three_files(&fixture);
    script_turns(&fixture, &[&[("a.txt", "a1\n")]]);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("yolo")).await;
    let thread = opened.thread_id.0.clone();
    let item = run_turn(&mut client, &thread, "turn 1").await;
    std::fs::write(fixture.cwd.join("c.txt"), "c edited later\n").unwrap();
    std::fs::write(fixture.cwd.join("stray.txt"), "stray\n").unwrap();

    let check = client.ok("turn/check", at(&thread, &item)).await;
    assert_eq!(check["unsafe"], json!([]), "{check}");
    client.ok("turn/undo", at(&thread, &item)).await;
    assert_eq!(read_file(&fixture, "a.txt"), "a0\n");
    assert_eq!(read_file(&fixture, "c.txt"), "c edited later\n");
    assert_eq!(read_file(&fixture, "stray.txt"), "stray\n");
}

#[cfg(unix)]
#[tokio::test]
async fn undo_restores_a_removed_executable_with_its_mode_and_a_removed_symlink_as_a_link() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    git(&fixture.cwd, &["init", "-q"]);
    let script = fixture.cwd.join("run.sh");
    std::fs::write(&script, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("run.sh", fixture.cwd.join("link")).unwrap();
    git(&fixture.cwd, &["add", "."]);
    git(&fixture.cwd, &["commit", "-q", "-m", "init"]);
    let mut steps = handshake(("never", "danger-full-access"));
    steps.extend(turn_start("turn 1", "turn-1"));
    steps.extend([json!({"remove": "run.sh"}), json!({"remove": "link"})]);
    steps.push(turn_done("turn-1", "completed"));
    fixture.script(steps);
    let mut client = fixture.client();
    let opened = open(&mut client, &fixture, Some("yolo")).await;
    let thread = opened.thread_id.0.clone();
    let item = run_turn(&mut client, &thread, "turn 1").await;

    client.ok("turn/undo", at(&thread, &item)).await;
    let mode = std::fs::metadata(&script).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    let target = std::fs::read_link(fixture.cwd.join("link")).unwrap();
    assert_eq!(target, std::path::Path::new("run.sh"));
    let check = client.ok("turn/check", at(&thread, &item)).await;
    assert_eq!(check["unsafe"], json!([]), "{check}");
    client.ok("turn/undo", at(&thread, &item)).await;
    assert!(!script.exists());
    assert!(std::fs::symlink_metadata(fixture.cwd.join("link")).is_err());
}

#[tokio::test]
async fn undo_twice_reapplies_the_turn_and_the_reverted_state_survives_reopening() {
    let fixture = Fixture::new();
    repo_with_three_files(&fixture);
    script_turns(&fixture, &[&[("a.txt", "a1\n")]]);
    let (path, thread, item) = {
        let mut client = fixture.client();
        let opened = open(&mut client, &fixture, Some("yolo")).await;
        let thread = opened.thread_id.0.clone();
        let item = run_turn(&mut client, &thread, "turn 1").await;
        let undone = client.ok("turn/undo", at(&thread, &item)).await;
        assert_eq!(undone["reverted"], true, "{undone}");
        assert_eq!(read_file(&fixture, "a.txt"), "a0\n");
        let check = client.ok("turn/check", at(&thread, &item)).await;
        assert_eq!(
            check,
            json!({"safe": ["a.txt"], "unsafe": [], "ignored": []})
        );
        client
            .ok("session/close", json!({"thread_id": thread}))
            .await;
        (opened.path, thread, item)
    };
    fixture.script(resumed());
    let mut client = fixture.client();
    let reopened = snapshot(client.ok("session/open", json!({"path": path})).await);
    assert_eq!(reopened.thread_id.0, thread);
    let changes = client.ok("turn/changes", at(&thread, &item)).await;
    assert_eq!(changes["reverted"], true, "{changes}");
    let again = client.ok("turn/undo", at(&thread, &item)).await;
    assert_eq!(again["reverted"], false, "{again}");
    assert_eq!(read_file(&fixture, "a.txt"), "a1\n");
    client.ok("turn/undo", at(&thread, &item)).await;
    std::fs::write(fixture.cwd.join("a.txt"), "mine\n").unwrap();
    let error = client
        .call("turn/undo", at(&thread, &item))
        .await
        .unwrap_err();
    assert!(error["message"].as_str().unwrap().contains("a.txt"));
    assert_eq!(read_file(&fixture, "a.txt"), "mine\n");
}
