//! The Codex driver against a fake `codex app-server` (`fake_codex.rs`) that replays scripted
//! frames, driven through a real `chui app-server`.
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
    ApprovalRequest, EventKind, Item, KnownItem, TokenUsage, TurnId, TurnStatus,
};

const THREAD: &str = "thr-root";
const TURN: &str = "turn-1";

// ----- script helpers -----

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
}

fn thread_response() -> Value {
    json!({"thread": {"id": THREAD, "turns": []}, "model": "gpt-fake",
        "modelProvider": "openai", "approvalPolicy": "untrusted", "sandbox": {"type": "readOnly"},
        "cwd": "/x"})
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

// ----- fixture -----

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

    /// The frames the fake received, in order.
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

// ----- tests -----

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
        // A sub-agent's thread runs and finishes its own turn inside the root turn.
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
    fixture.script(vec![
        expect(json!({"method": "initialize"})),
        answer(json!({"userAgent": "fake/0.171.0 (test)"})),
        expect(json!({"method": "initialized"})),
        expect(json!({"method": "thread/resume", "params": {"threadId": THREAD}})),
        answer(thread_response()),
    ]);
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
        // The echo of the user's message is skipped.
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
        // Two model requests, then the same totals again.
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
    // Text streams as started, merged deltas, completed.
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
