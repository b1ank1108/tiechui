#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tiechui_protocol::app::{SessionEvent, SessionSummary, Snapshot};
use tiechui_protocol::{ApprovalRequest, EventKind, Item, KnownItem, TurnStatus};
use tiechui_providers::testing::ScriptedServer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio_util::sync::CancellationToken;

struct Client {
    _child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next: u64,
    events: Vec<SessionEvent>,
}

impl Client {
    async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(15), self.lines.next_line())
            .await
            .expect("app-server stalled")
            .unwrap()
            .expect("app-server exited");
        serde_json::from_str(&line).unwrap()
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        self.next += 1;
        let id = self.next;
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let message = self.read().await;
            if message["id"] == id {
                return match message.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(message["result"].clone()),
                };
            }
            if message["method"] == "session/event" {
                self.events
                    .push(serde_json::from_value(message["params"].clone()).unwrap());
            }
        }
    }

    async fn ok(&mut self, method: &str, params: Value) -> Value {
        self.call(method, params).await.unwrap()
    }

    /// Takes the buffered and incoming events up to the first one that matches.
    async fn until(&mut self, matches: impl Fn(&EventKind) -> bool) -> Vec<SessionEvent> {
        loop {
            if let Some(index) = self.events.iter().position(|e| matches(&e.event.event)) {
                return self.events.drain(..=index).collect();
            }
            let message = self.read().await;
            if message["method"] == "session/event" {
                self.events
                    .push(serde_json::from_value(message["params"].clone()).unwrap());
            }
        }
    }
}

struct Fixture {
    root: TempDir,
    home: PathBuf,
    cwd: PathBuf,
    server: ScriptedServer,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cwd = root.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("hello.txt"), "before\n").unwrap();
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../tiechui-providers/tests/fixtures/e2e");
        let step = |file: &str, contains: &[&str]| {
            json!({"path": "/v1/chat/completions", "status": 200,
                "body_file": fixtures.join(file), "body_contains": contains})
        };
        let script = root.path().join("script.json");
        std::fs::write(
            &script,
            json!([
                step("completions-read.sse", &["Change hello.txt"]),
                step("completions-edit.sse", &["call_read"]),
                step("completions-answer.sse", &["call_edit"]),
            ])
            .to_string(),
        )
        .unwrap();
        let server = ScriptedServer::from_file(&script, CancellationToken::new())
            .await
            .unwrap();
        std::fs::write(home.join("config.toml"), "model = \"fixture/model\"\n").unwrap();
        std::fs::write(
            home.join("providers.toml"),
            format!(
                "[fixture]\nwire = \"openai-completions\"\nbase_url = \"{}\"\napi_key_env = \"TC_E2E_FAKE_KEY\"\nmodels = [{{ id = \"model\", context_window = 200000, max_output = 4096 }}]\n",
                server.uri()
            ),
        )
        .unwrap();
        Self {
            root,
            home,
            cwd,
            server,
        }
    }

    async fn client(&self) -> Client {
        let mut child = Command::new(env!("CARGO_BIN_EXE_chui"))
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("TIECHUI_HOME", &self.home)
            .env("HOME", self.root.path())
            .env("TC_E2E_FAKE_KEY", "not-a-real-key")
            .current_dir(&self.cwd)
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Client {
            _child: child,
            stdin,
            lines,
            next: 0,
            events: vec![],
        }
    }
}

fn snapshot(value: Value) -> Snapshot {
    serde_json::from_value(value).unwrap()
}

fn turns(snapshot: &Snapshot) -> usize {
    match &snapshot.events[0].event {
        EventKind::ThreadStarted { thread, .. } => thread.turns.len(),
        other => panic!("snapshot starts with {other:?}"),
    }
}

#[tokio::test]
async fn sessions_converse_ask_approvals_and_fork_through_app_server() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client().await;
    let cwd = fixture.cwd.display().to_string();
    let init = client
        .ok("initialize", json!({"protocol_version": 1}))
        .await;
    assert_eq!(init["home"], json!(fixture.home.display().to_string()));
    assert_eq!(
        client.call("nothing", json!({})).await.unwrap_err()["code"],
        -32601
    );

    let first = snapshot(
        client
            .ok("session/open", json!({"cwd": cwd, "mode": "rules"}))
            .await,
    );
    assert_eq!(turns(&first), 0);
    let thread = json!(first.thread_id);
    client
        .ok(
            "session/submit",
            json!({"thread_id": thread, "op": {"kind": "user_input", "delivery": "follow_up",
                "content": [{"kind": "text", "text": "Change hello.txt to after"}]}}),
        )
        .await;
    let asked = client
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let last = asked.last().unwrap();
    assert!(last.seq > first.seq);
    let EventKind::ApprovalRequested {
        approval_id,
        request: ApprovalRequest::Permission { tool, .. },
        ..
    } = &last.event.event
    else {
        panic!("expected a permission request");
    };
    assert_eq!(tool, "edit");

    // A late snapshot shows the open turn and the pending approval; later events follow it.
    let waiting = snapshot(
        client
            .ok("session/snapshot", json!({"thread_id": thread}))
            .await,
    );
    assert_eq!(waiting.seq, last.seq);
    assert!(matches!(
        &waiting.events.last().unwrap().event,
        EventKind::ApprovalRequested { .. }
    ));
    let listed: Vec<SessionSummary> =
        serde_json::from_value(client.ok("session/list", json!({"cwd": cwd})).await).unwrap();
    assert_eq!(
        listed[0].live,
        Some(tiechui_protocol::app::LiveState::Waiting)
    );

    client
        .ok(
            "session/submit",
            json!({"thread_id": thread, "op": {"kind": "approval_response",
                "approval_id": approval_id, "decision": {"kind": "allow_once"}}}),
        )
        .await;
    let finished = client
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    assert_eq!(finished[0].seq, waiting.seq + 1);
    let EventKind::TurnCompleted { turn } = &finished.last().unwrap().event.event else {
        unreachable!()
    };
    assert_eq!(turn.status, TurnStatus::Completed);
    assert!(turn.started_at > 0 && turn.ended_at >= Some(turn.started_at));
    assert_eq!(
        std::fs::read_to_string(fixture.cwd.join("hello.txt")).unwrap(),
        "after\n"
    );
    let message = turn
        .items
        .iter()
        .find_map(|item| match item {
            Item::Known(KnownItem::UserMessage { id, .. }) => Some(id.0.clone()),
            _ => None,
        })
        .unwrap();

    // Metadata, a second concurrent session, and a fork through the finished turn.
    client
        .ok(
            "session/rename",
            json!({"path": first.path, "title": "Renamed"}),
        )
        .await;
    let second = snapshot(client.ok("session/open", json!({"cwd": cwd})).await);
    assert_ne!(second.thread_id, first.thread_id);
    let forked = snapshot(
        client
            .ok(
                "session/fork",
                json!({"thread_id": thread, "item_id": message}),
            )
            .await,
    );
    assert_ne!(forked.path, first.path);
    assert_eq!(turns(&forked), 1);
    let listed: Vec<SessionSummary> =
        serde_json::from_value(client.ok("session/list", json!({"cwd": cwd})).await).unwrap();
    assert_eq!(listed.len(), 2);
    let renamed = listed.iter().find(|s| s.path == first.path).unwrap();
    assert_eq!(renamed.title.as_deref(), Some("Renamed"));
    assert_eq!(renamed.live, Some(tiechui_protocol::app::LiveState::Idle));

    // Branching before the only turn leaves an empty thread after the new epoch.
    client
        .ok(
            "session/branch",
            json!({"thread_id": thread, "item_id": message}),
        )
        .await;
    let replayed = client
        .until(|e| matches!(e, EventKind::ThreadStarted { replay: true, .. }))
        .await;
    assert!(matches!(&replayed.last().unwrap().event.event,
        EventKind::ThreadStarted { thread, .. } if thread.turns.is_empty()));

    client
        .ok(
            "session/archive",
            json!({"path": forked.path, "archived": true}),
        )
        .await;
    client
        .ok("session/trash", json!({"path": first.path}))
        .await;
    let listed: Vec<SessionSummary> = serde_json::from_value(
        client
            .ok("session/list", json!({"cwd": cwd, "archived": true}))
            .await,
    )
    .unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].archived_at.is_some());
    assert!(
        client
            .call("session/snapshot", json!({"thread_id": thread}))
            .await
            .is_err()
    );
    fixture.server.verify().unwrap();
}
