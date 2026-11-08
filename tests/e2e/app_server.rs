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
    /// Other notifications.
    notices: Vec<Value>,
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
            self.buffer(message);
        }
    }

    fn buffer(&mut self, message: Value) {
        if message["method"] == "session/event" {
            self.events
                .push(serde_json::from_value(message["params"].clone()).unwrap());
        } else if message.get("method").is_some() {
            self.notices.push(message);
        }
    }

    /// Takes the params of the first buffered or incoming notification that matches.
    async fn notice(&mut self, matches: impl Fn(&str, &Value) -> bool) -> Value {
        loop {
            let found = self
                .notices
                .iter()
                .position(|n| matches(n["method"].as_str().unwrap_or_default(), &n["params"]));
            if let Some(index) = found {
                return self.notices.remove(index)["params"].take();
            }
            let message = self.read().await;
            self.buffer(message);
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
            self.buffer(message);
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
            notices: vec![],
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

#[tokio::test]
async fn workspace_services_through_app_server() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client().await;
    let init = client
        .ok("initialize", json!({"protocol_version": 1}))
        .await;
    assert_eq!(init["capabilities"], json!(["workspace"]));
    let root = fixture.cwd.display().to_string();
    let tree = client.ok("fs/tree", json!({"root": root})).await;
    assert_eq!(
        tree["entries"],
        json!([{"path": "hello.txt", "dir": false, "ignored": false}])
    );
    let file = json!({"root": root, "path": "hello.txt"});
    assert_eq!(client.ok("fs/read", file.clone()).await["text"], "before\n");
    let stale = json!({"root": root, "path": "hello.txt", "text": "x", "expected": "0"});
    assert!(client.call("fs/write", stale).await.is_err());
    assert!(
        client
            .call("fs/read", json!({"root": "relative", "path": "a"}))
            .await
            .is_err()
    );
    assert_eq!(
        client.ok("git/status", json!({"root": root})).await,
        Value::Null
    );

    client.ok("fs/watch", json!({"root": root})).await;
    let new = json!({"root": root, "path": "new.txt", "text": "x", "expected": ""});
    client.ok("fs/write", new).await;
    client
        .notice(|method, params| {
            method == "fs/changed"
                && params["paths"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("new.txt"))
        })
        .await;

    let terminal = json!({"id": "t", "cwd": root, "cols": 80, "rows": 24});
    client.ok("terminal/create", terminal).await;
    let echo = json!({"id": "t", "data": "echo tiechui-$((40 + 2))\n"});
    client.ok("terminal/write", echo).await;
    let mut output = String::new();
    while !output.contains("tiechui-42") {
        let notice = client.notice(|method, _| method == "terminal/output").await;
        output.push_str(notice["data"].as_str().unwrap());
    }
    let attached = client.ok("terminal/attach", json!({"id": "t"})).await;
    assert!(attached["buffer"].as_str().unwrap().contains("tiechui-42"));
    client.ok("terminal/kill", json!({"id": "t"})).await;
    assert!(
        client
            .call("terminal/attach", json!({"id": "t"}))
            .await
            .is_err()
    );
}
