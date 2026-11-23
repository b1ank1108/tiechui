#![allow(clippy::unwrap_used)]

mod client;
mod support;

use std::path::PathBuf;

use client::Client;
use serde_json::{Value, json};
use tempfile::TempDir;
use tiechui_protocol::app::{SessionSummary, Snapshot};
use tiechui_protocol::{
    ApprovalRequest, CommandKind, DerivedReason, DerivedTurnMarker, EventKind, Item, KnownItem,
    PERMISSION_MODE_EVENT, RUNTIME_UNSUPPORTED_CODE, SteeringMode, SubagentItem, SubagentState,
    SubagentStatus, ThinkingLevel, TurnStatus,
};

struct Fixture {
    root: TempDir,
    home: PathBuf,
    cwd: PathBuf,
    log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cwd = root.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(home.join("config.toml"), "model = \"fixture/model\"\n").unwrap();
        std::fs::write(
            home.join("providers.toml"),
            "[fixture]\nwire = \"openai-completions\"\nbase_url = \"http://127.0.0.1:9\"\napi_key_env = \"TC_E2E_FAKE_KEY\"\nmodels = [{ id = \"model\", context_window = 200000, max_output = 4096 }]\n",
        )
        .unwrap();
        let log = root.path().join("fake.log");
        Self {
            root,
            home,
            cwd,
            log,
        }
    }

    fn script(&self, script: Value) {
        std::fs::write(self.root.path().join("script.json"), script.to_string()).unwrap();
    }

    fn log_lines(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    async fn client(&self) -> Client {
        self.client_on_path("/usr/bin:/bin").await
    }

    async fn client_on_path(&self, path: &str) -> Client {
        let mut command = support::command();
        command
            .env("PATH", path)
            .env("TIECHUI_HOME", &self.home)
            .env("HOME", self.root.path())
            .env("TC_E2E_FAKE_KEY", "not-a-real-key")
            .env(
                "CHUI_FAKE_RUNTIME_SCRIPT",
                self.root.path().join("script.json"),
            )
            .env("CHUI_FAKE_RUNTIME_LOG", &self.log)
            .current_dir(&self.cwd)
            .arg("app-server");
        Client::spawn(command)
    }
}

fn snapshot(value: Value) -> Snapshot {
    serde_json::from_value(value).unwrap()
}

fn user_input(text: &str) -> Value {
    json!({"kind": "user_input", "delivery": "follow_up", "content": [{"kind": "text", "text": text}]})
}

async fn open_fake(client: &mut Client, fixture: &Fixture) -> Snapshot {
    snapshot(
        client
            .ok(
                "session/open",
                json!({"cwd": fixture.cwd, "runtime": "fake"}),
            )
            .await,
    )
}

fn is_completed(kind: &EventKind) -> bool {
    matches!(kind, EventKind::TurnCompleted { .. })
}

#[cfg(unix)]
fn stub(dir: &std::path::Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn runtime<'a>(list: &'a Value, id: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == id)
        .unwrap_or_else(|| panic!("runtimes/list has no {id}: {list}"))
}

#[tokio::test]
async fn runtimes_list_has_chui_and_reports_codex_unavailable_without_it_on_path() {
    let fixture = Fixture::new();
    fixture.script(json!({"turns": []}));
    let mut client = fixture.client().await;
    let list = client.ok("runtimes/list", json!({})).await;
    let chui = runtime(&list, "chui");
    assert_eq!(chui["available"], true);
    assert_eq!(chui["capabilities"]["steering"], "inject");
    assert_eq!(list[0]["id"], "chui", "chui is listed first");
    let codex = runtime(&list, "codex");
    assert_eq!(codex["available"], false);
    assert!(
        codex["reason"].as_str().unwrap().contains("not on PATH"),
        "{codex}"
    );
    assert!(codex.get("version").is_none() && codex.get("path").is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn runtimes_list_probes_codex_version_on_the_daemon_path_without_side_effects() {
    let fixture = Fixture::new();
    fixture.script(json!({"turns": []}));
    let bin = fixture.root.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let marker = fixture.root.path().join("ran-with-other-args");
    stub(
        &bin,
        "codex",
        &format!(
            "if [ \"$1\" = \"--version\" ] && [ $# -eq 1 ]; then echo 'codex-cli 0.171.0'; else touch {}; fi",
            marker.display()
        ),
    );
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let mut client = fixture.client_on_path(&path).await;
    let list = client.ok("runtimes/list", json!({})).await;
    let codex = runtime(&list, "codex");
    assert_eq!(codex["available"], true, "{codex}");
    assert_eq!(codex["version"], "0.171.0");
    assert_eq!(codex["path"], bin.join("codex").display().to_string());
    assert!(codex.get("reason").is_none());
    assert!(
        !marker.exists(),
        "the probe ran codex with more than --version"
    );

    stub(&bin, "codex", "echo broken >&2; exit 3");
    let list = client.ok("runtimes/list", json!({})).await;
    let codex = runtime(&list, "codex");
    assert_eq!(codex["available"], false);
    assert!(
        codex["reason"].as_str().unwrap().contains("exited"),
        "{codex}"
    );
}

#[tokio::test]
async fn opening_an_unknown_or_unimplemented_runtime_is_an_error_that_names_it() {
    let fixture = Fixture::new();
    fixture.script(json!({"turns": []}));
    let mut client = fixture.client().await;
    let error = client
        .call(
            "session/open",
            json!({"cwd": fixture.cwd, "runtime": "codex"}),
        )
        .await
        .unwrap_err();
    assert!(
        error["message"].as_str().unwrap().contains("codex"),
        "{error}"
    );
    let error = client
        .call(
            "session/open",
            json!({"cwd": fixture.cwd, "runtime": "nope"}),
        )
        .await
        .unwrap_err();
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("unknown runtime")
    );
}

#[tokio::test]
async fn gated_entries_fail_with_runtime_unsupported_on_a_fake_session_but_work_natively() {
    let fixture = Fixture::new();
    fixture.script(json!({"turns": []}));
    let mut client = fixture.client().await;
    let fake = open_fake(&mut client, &fixture).await;
    assert_eq!(fake.runtime.as_deref(), Some("fake"));
    let caps = fake.capabilities.unwrap();
    assert_eq!(caps.steering, SteeringMode::Inject);
    assert!(!caps.branch && !caps.shell);
    assert!(caps.commands && caps.permissions && caps.thinking && caps.model);
    let thread = fake.thread_id.0.clone();

    let refused = [
        (
            "session/branch",
            json!({"thread_id": thread, "item_id": "x"}),
        ),
        ("session/fork", json!({"thread_id": thread, "item_id": "x"})),
        (
            "session/rewind",
            json!({"thread_id": thread, "item_id": "x", "mode": "chat_only"}),
        ),
        ("btw/open", json!({"thread_id": thread})),
        ("btw/ask", json!({"thread_id": thread, "content": []})),
    ];
    for (method, params) in refused {
        let error = client.call(method, params).await.unwrap_err();
        assert_eq!(error["code"], RUNTIME_UNSUPPORTED_CODE, "{method}: {error}");
        assert_eq!(error["data"], json!({"runtime": "fake", "feature": method}));
        assert_eq!(
            error["message"],
            format!("runtime fake does not support {method}")
        );
    }
    let ops = [
        (json!({"kind": "retry"}), "retry"),
        (json!({"kind": "compact", "instructions": null}), "compact"),
        (json!({"kind": "run_shell", "command": "ls"}), "run_shell"),
        (json!({"kind": "branch", "entry_id": "x"}), "branch"),
    ];
    for (op, feature) in ops {
        let error = client
            .call("session/submit", json!({"thread_id": thread, "op": op}))
            .await
            .unwrap_err();
        assert_eq!(error["code"], RUNTIME_UNSUPPORTED_CODE, "{op}: {error}");
        assert_eq!(
            error["data"],
            json!({"runtime": "fake", "feature": feature})
        );
    }
    for op in [
        json!({"kind": "input_state", "blocked": false}),
        json!({"kind": "run_command", "name": "x", "arguments": ""}),
        json!({"kind": "command_input", "name": "x", "data": null}),
        json!({"kind": "set_thinking", "level": "high"}),
        json!({"kind": "set_model", "provider": "fake", "model": "fake-model"}),
    ] {
        client
            .ok("session/submit", json!({"thread_id": thread, "op": op}))
            .await;
    }

    let native = snapshot(client.ok("session/open", json!({"cwd": fixture.cwd})).await);
    assert_eq!(native.runtime.as_deref(), Some("chui"));
    let caps = native.capabilities.unwrap();
    assert!(caps.branch && caps.shell);
    client
        .ok(
            "session/submit",
            json!({"thread_id": native.thread_id, "op": {"kind": "set_thinking", "level": "off"}}),
        )
        .await;
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
                let id = described.rsplit(':').next().unwrap().to_owned();
                match turn
                    .2
                    .iter()
                    .position(|d| d.rsplit(':').next() == Some(id.as_str()))
                {
                    Some(index) => turn.2[index] = described,
                    None => turn.2.push(described),
                }
            }
            EventKind::TurnCompleted { turn } => {
                let slot = turns
                    .iter_mut()
                    .find(|t| t.0 == turn.id.0)
                    .expect("unknown turn");
                *slot = (
                    turn.id.0.clone(),
                    format!("{:?}", turn.status),
                    turn.items.iter().map(describe).collect(),
                );
            }
            _ => {}
        }
    }
    turns
}

fn late_script() -> Value {
    json!({"turns": [{"steps": [
        {"op": "text", "text": "starting"},
        {"op": "subagent", "id": "a1", "name": "worker", "phase": "spawn"},
        {"op": "turn_end"},
        {"op": "sleep", "ms": 100},
        {"op": "subagent", "id": "a1", "phase": "output", "final": "completed", "steps": [
            {"op": "command", "command": "ls", "output": "a\n"},
            {"op": "text", "text": "child done"},
        ]},
        {"op": "approval", "tool": "bash", "values": ["touch x"], "agent": "a1"},
        {"op": "idle"},
    ]}]})
}

#[tokio::test]
async fn subagent_items_and_a_turn_after_turn_completed_have_one_structure_live_and_in_snapshots() {
    let fixture = Fixture::new();
    fixture.script(late_script());
    let mut client = fixture.client().await;
    let opened = open_fake(&mut client, &fixture).await;
    let thread = opened.thread_id.0.clone();
    let mut live: Vec<tiechui_protocol::Event> = opened.events.clone();
    client
        .ok(
            "session/submit",
            json!({"thread_id": thread, "op": user_input("go")}),
        )
        .await;
    live.extend(
        client
            .until(is_completed)
            .await
            .into_iter()
            .map(|e| e.event),
    );
    let seen = client
        .until(|kind| matches!(kind, EventKind::ApprovalRequested { .. }))
        .await;
    let approval = seen.last().unwrap().event.event.clone();
    live.extend(seen.into_iter().map(|e| e.event));
    let EventKind::ApprovalRequested {
        turn_id: Some(approval_turn),
        approval_id,
        request: ApprovalRequest::Permission { actor, .. },
    } = approval
    else {
        panic!("unexpected {approval:?}");
    };
    assert_eq!(actor.as_deref(), Some("a1"));

    let mid = snapshot(
        client
            .ok("session/snapshot", json!({"thread_id": thread}))
            .await,
    );
    let live_mid = structure(&live);
    assert_eq!(
        structure(&mid.events),
        live_mid,
        "mid-flight snapshot differs from live"
    );
    assert_eq!(live_mid.len(), 2, "{live_mid:?}");
    assert_eq!(live_mid[0].1, "Completed");
    assert_eq!(live_mid[1].0, approval_turn.0);
    assert_eq!(
        live_mid[1].1, "InProgress",
        "the derived turn stays open for its approval"
    );
    assert!(live_mid[1].2[0].starts_with("runtime.derived_turn:"));

    client
        .ok(
            "session/submit",
            json!({"thread_id": thread, "op": {"kind": "approval_response",
                "approval_id": approval_id, "decision": {"kind": "allow_once"}}}),
        )
        .await;
    live.extend(
        client
            .until(is_completed)
            .await
            .into_iter()
            .map(|e| e.event),
    );
    let done = snapshot(
        client
            .ok("session/snapshot", json!({"thread_id": thread}))
            .await,
    );
    let live_done = structure(&live);
    assert_eq!(
        structure(&done.events),
        live_done,
        "final snapshot differs from live"
    );
    assert_eq!(live_done[1].1, "Completed");

    let EventKind::ThreadStarted {
        thread: started, ..
    } = &done.events[0].event
    else {
        panic!("snapshot starts with {:?}", done.events[0].event);
    };
    let first = &started.turns[0];
    let derived = &started.turns[1];
    let marker =
        DerivedTurnMarker::from_item(&derived.items[0]).expect("derived turn marker first");
    assert_eq!(marker.derived_from, first.id);
    assert_eq!(marker.reason, DerivedReason::Subagent);
    let statuses: Vec<SubagentStatus> = first
        .items
        .iter()
        .chain(&derived.items)
        .filter_map(SubagentStatus::from_item)
        .collect();
    assert_eq!(
        statuses.len(),
        2,
        "one status item per update, per turn it arrived in"
    );
    assert_eq!(statuses[0].state, SubagentState::Running);
    assert_eq!(statuses[1].state, SubagentState::Completed);
    assert!(
        statuses
            .iter()
            .all(|s| s.agent_id == "a1" && s.parent_call_id.0 == "spawn-a1")
    );
    let children: Vec<SubagentItem> = derived
        .items
        .iter()
        .filter_map(SubagentItem::from_item)
        .collect();
    assert_eq!(children.len(), 3, "command call, its result and a text");
    assert!(
        children
            .iter()
            .all(|c| c.parent_call_id.0 == "spawn-a1" && c.agent_id == "a1")
    );
    assert!(first.items.iter().any(|item| matches!(item,
        Item::Known(KnownItem::ToolCall { id, name, .. }) if id.0 == "spawn-a1" && name == "spawn_agent")));
    assert_eq!(first.status, TurnStatus::Completed);
}

#[tokio::test]
async fn approval_answered_twice_at_once_reaches_the_backend_once() {
    let fixture = Fixture::new();
    fixture.script(json!({"turns": [{"steps": [
        {"op": "approval", "tool": "bash", "values": ["touch x"]},
    ]}]}));
    let mut client = fixture.client().await;
    let opened = open_fake(&mut client, &fixture).await;
    let thread = opened.thread_id.0.clone();
    client
        .ok(
            "session/submit",
            json!({"thread_id": thread, "op": user_input("go")}),
        )
        .await;
    let seen = client
        .until(|kind| matches!(kind, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested { approval_id, .. } = &seen.last().unwrap().event.event else {
        unreachable!()
    };
    let answer = |decision: Value| {
        json!({"thread_id": thread, "op": {"kind": "approval_response",
            "approval_id": approval_id, "decision": decision}})
    };
    let first = client
        .send("session/submit", answer(json!({"kind": "allow_once"})))
        .await;
    let second = client
        .send(
            "session/submit",
            answer(json!({"kind": "deny", "reason": null})),
        )
        .await;
    client.reply(first).await.unwrap();
    client.reply(second).await.unwrap();
    client.until(is_completed).await;
    let answers: Vec<_> = fixture
        .log_lines()
        .into_iter()
        .filter(|line| line.starts_with("answer "))
        .collect();
    assert_eq!(answers.len(), 1, "{answers:?}");
}

#[tokio::test]
async fn reopening_by_path_restores_history_runtime_and_the_external_thread() {
    let fixture = Fixture::new();
    fixture.script(json!({"turns": [{"steps": [
        {"op": "text", "text": "remembered"},
        {"op": "usage", "input": 7, "output": 3},
    ]}]}));
    let (path, thread) = {
        let mut client = fixture.client().await;
        let opened = open_fake(&mut client, &fixture).await;
        client
            .ok(
                "session/submit",
                json!({"thread_id": opened.thread_id, "op": user_input("remember this")}),
            )
            .await;
        client.until(is_completed).await;
        let snap = snapshot(
            client
                .ok("session/snapshot", json!({"thread_id": opened.thread_id}))
                .await,
        );
        let usage = snap.events.iter().find_map(|e| match &e.event {
            EventKind::SessionUpdated { total, .. } => Some(*total),
            _ => None,
        });
        assert_eq!(
            usage.map(|u| (u.input, u.output)),
            Some((7, 3)),
            "snapshot keeps usage"
        );
        client
            .ok("session/close", json!({"thread_id": opened.thread_id}))
            .await;
        (opened.path, opened.thread_id)
    };
    let started: Vec<_> = fixture
        .log_lines()
        .into_iter()
        .filter(|line| line.starts_with("start "))
        .collect();
    assert_eq!(started.len(), 1);
    let external = started[0].rsplit("id=").next().unwrap().to_owned();

    let mut client = fixture.client().await;
    let listed: Vec<SessionSummary> =
        serde_json::from_value(client.ok("session/list", json!({"cwd": fixture.cwd})).await)
            .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].runtime.as_deref(), Some("fake"));
    assert_eq!(listed[0].thread_id, thread);
    let reopened = snapshot(
        client
            .ok("session/open", json!({"path": path, "runtime": "chui"}))
            .await,
    );
    assert_eq!(reopened.runtime.as_deref(), Some("fake"));
    assert_eq!(reopened.thread_id, thread);
    let EventKind::ThreadStarted {
        thread: history, ..
    } = &reopened.events[0].event
    else {
        panic!("{:?}", reopened.events[0].event);
    };
    let text = serde_json::to_string(&history.turns).unwrap();
    assert!(
        text.contains("remember this") && text.contains("remembered"),
        "{text}"
    );
    let (total, context) = reopened
        .events
        .iter()
        .find_map(|e| match &e.event {
            EventKind::SessionUpdated { total, context, .. } => Some((*total, *context)),
            _ => None,
        })
        .unwrap();
    assert_eq!((total.input, total.output), (7, 3));
    assert_eq!(context.map(|c| c.input), Some(7));
    let starts: Vec<_> = fixture
        .log_lines()
        .into_iter()
        .filter(|line| line.starts_with("start "))
        .collect();
    assert_eq!(
        starts[1],
        format!("start resume={external} model=none thinking=none id={external}")
    );
}

fn thinking_entries(path: &str) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|entry| entry["kind"] == "thinking_change")
        .collect()
}

fn session_thinking(events: &[tiechui_protocol::Event]) -> Option<ThinkingLevel> {
    events.iter().rev().find_map(|e| match &e.event {
        EventKind::SessionUpdated { thinking, .. } => Some(*thinking),
        _ => None,
    })
}

fn start_lines(fixture: &Fixture) -> Vec<String> {
    fixture
        .log_lines()
        .into_iter()
        .filter(|line| line.starts_with("start "))
        .collect()
}

#[tokio::test]
async fn set_thinking_is_persisted_and_restored_when_the_session_is_reopened_by_path() {
    let fixture = Fixture::new();
    fixture.script(json!({"turns": []}));
    let (path, thread) = {
        let mut client = fixture.client().await;
        let opened = open_fake(&mut client, &fixture).await;
        assert_eq!(session_thinking(&opened.events), Some(ThinkingLevel::Off));
        assert!(thinking_entries(&opened.path).is_empty());
        client
            .ok(
                "session/submit",
                json!({"thread_id": opened.thread_id, "op": {"kind": "set_thinking", "level": "high"}}),
            )
            .await;
        client
            .until(|kind| {
                matches!(
                    kind,
                    EventKind::SessionUpdated {
                        thinking: ThinkingLevel::High,
                        ..
                    }
                )
            })
            .await;
        let entries = thinking_entries(&opened.path);
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(entries[0]["level"], "high");
        assert!(
            fixture
                .log_lines()
                .contains(&"set_thinking high".to_owned())
        );
        client
            .ok("session/close", json!({"thread_id": opened.thread_id}))
            .await;
        (opened.path, opened.thread_id)
    };

    let mut client = fixture.client().await;
    let reopened = snapshot(client.ok("session/open", json!({"path": path})).await);
    assert_eq!(reopened.thread_id, thread);
    assert_eq!(
        session_thinking(&reopened.events),
        Some(ThinkingLevel::High)
    );
    let starts = start_lines(&fixture);
    assert_eq!(starts.len(), 2, "{starts:?}");
    assert!(starts[1].contains("thinking=high"), "{}", starts[1]);
    client
        .ok("session/close", json!({"thread_id": thread}))
        .await;

    client
        .ok("session/open", json!({"path": path, "thinking": "low"}))
        .await;
    let starts = start_lines(&fixture);
    assert!(starts[2].contains("thinking=high"), "{}", starts[2]);
    assert!(fixture.log_lines().contains(&"set_thinking low".to_owned()));
    let entries = thinking_entries(&path);
    assert_eq!(entries.last().unwrap()["level"], "low");
}

#[tokio::test]
async fn set_thinking_off_is_recorded_as_an_empty_level_and_restored() {
    let fixture = Fixture::new();
    fixture.script(json!({"turns": []}));
    let mut client = fixture.client().await;
    let opened = snapshot(
        client
            .ok(
                "session/open",
                json!({"cwd": fixture.cwd, "runtime": "fake", "thinking": "medium"}),
            )
            .await,
    );
    assert_eq!(
        session_thinking(&opened.events),
        Some(ThinkingLevel::Medium)
    );
    client
        .ok(
            "session/submit",
            json!({"thread_id": opened.thread_id, "op": {"kind": "set_thinking", "level": "off"}}),
        )
        .await;
    client
        .until(|kind| {
            matches!(
                kind,
                EventKind::SessionUpdated {
                    thinking: ThinkingLevel::Off,
                    ..
                }
            )
        })
        .await;
    let entries = thinking_entries(&opened.path);
    assert_eq!(entries.last().unwrap()["level"], Value::Null, "{entries:?}");
    client
        .ok("session/close", json!({"thread_id": opened.thread_id}))
        .await;
    client
        .ok("session/open", json!({"path": opened.path}))
        .await;
    let starts = start_lines(&fixture);
    assert!(starts[1].contains("thinking=off"), "{}", starts[1]);
}

async fn thinking_on(client: &mut Client, id: &str) -> ThinkingLevel {
    let events = client
        .until(|kind| matches!(kind, EventKind::SessionUpdated { model, .. } if model.id == id))
        .await;
    match &events.last().unwrap().event.event {
        EventKind::SessionUpdated { thinking, .. } => *thinking,
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn a_model_switch_shows_the_requested_level_clamped_to_the_new_model_without_recording_it() {
    let fixture = Fixture::new();
    fixture.script(json!({"turns": []}));
    let mut client = fixture.client().await;
    let opened = open_fake(&mut client, &fixture).await;
    let submit = |op: Value| json!({"thread_id": opened.thread_id, "op": op});
    let model = |id: &str| json!({"kind": "set_model", "provider": "fake", "model": id});

    client
        .ok(
            "session/submit",
            submit(json!({"kind": "set_thinking", "level": "high"})),
        )
        .await;
    client
        .until(|kind| {
            matches!(
                kind,
                EventKind::SessionUpdated {
                    thinking: ThinkingLevel::High,
                    ..
                }
            )
        })
        .await;
    client
        .ok("session/submit", submit(model("fake-small")))
        .await;
    assert_eq!(
        thinking_on(&mut client, "fake-small").await,
        ThinkingLevel::Off,
        "fake-small lists only off"
    );
    let entries = thinking_entries(&opened.path);
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["level"], "high");
    client
        .ok("session/submit", submit(model("fake-model")))
        .await;
    assert_eq!(
        thinking_on(&mut client, "fake-model").await,
        ThinkingLevel::High,
        "the requested level comes back with a model that lists it"
    );
    assert_eq!(thinking_entries(&opened.path).len(), 1);
}

#[tokio::test]
async fn the_permissions_command_switches_the_mode_and_other_commands_reach_the_backend() {
    let fixture = Fixture::new();
    fixture.script(json!({"turns": []}));
    let mut client = fixture.client().await;
    let opened = open_fake(&mut client, &fixture).await;
    let commands = opened
        .events
        .iter()
        .find_map(|e| match &e.event {
            EventKind::CommandsUpdated { commands } => Some(commands.clone()),
            _ => None,
        })
        .unwrap();
    let kinds: Vec<_> = commands.iter().map(|command| command.kind).collect();
    assert!(kinds.contains(&CommandKind::Skill), "{commands:?}");
    assert!(kinds.contains(&CommandKind::Command), "{commands:?}");

    client
        .ok(
            "session/submit",
            json!({"thread_id": opened.thread_id, "op": {"kind": "run_command", "name": "permissions", "arguments": "auto"}}),
        )
        .await;
    let events = client
        .until(|kind| {
            matches!(kind, EventKind::Extension { name, data }
                if name == PERMISSION_MODE_EVENT && *data == "auto")
        })
        .await;
    assert!(!events.is_empty());
    assert!(
        fixture
            .log_lines()
            .contains(&"run_command permissions auto".to_owned())
    );
}

#[tokio::test]
async fn a_scratch_session_may_run_on_an_external_runtime_in_the_shared_scratch_dir() {
    let fixture = Fixture::new();
    fixture.script(json!({"turns": []}));
    let mut client = fixture.client().await;
    let opened = snapshot(
        client
            .ok("session/open", json!({"scratch": {}, "runtime": "fake"}))
            .await,
    );
    assert_eq!(opened.runtime.as_deref(), Some("fake"));
    let scratch = fixture.home.join("scratch").join("default");
    assert_eq!(
        std::fs::canonicalize(&opened.cwd).unwrap(),
        std::fs::canonicalize(&scratch).unwrap()
    );
    let listed: Vec<SessionSummary> =
        serde_json::from_value(client.ok("scratch/list", json!({})).await).unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].thread_id, opened.thread_id);
    assert_eq!(listed[0].runtime.as_deref(), Some("fake"));

    let native = snapshot(client.ok("session/open", json!({"scratch": {}})).await);
    assert_eq!(native.runtime.as_deref(), Some("chui"));
    assert_eq!(native.cwd, opened.cwd);
    let error = client
        .call("session/open", json!({"scratch": {}, "runtime": "nope"}))
        .await
        .unwrap_err();
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("unknown runtime nope"),
        "{error}"
    );
}
