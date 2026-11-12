#![allow(clippy::unwrap_used)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tiechui_protocol::app::{SessionEvent, SessionSummary, Snapshot};
use tiechui_protocol::{ApprovalRequest, EventKind, FilePreview, Item, KnownItem, TurnStatus};
use tiechui_providers::testing::ScriptedServer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
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
        let id = self.send(method, params).await;
        self.reply(id).await
    }

    async fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let request =
            json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params});
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        self.next
    }

    async fn reply(&mut self, id: u64) -> Result<Value, Value> {
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
        let mut child = support::command()
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
        request: ApprovalRequest::Permission { tool, preview, .. },
        ..
    } = &last.event.event
    else {
        panic!("expected a permission request");
    };
    assert_eq!(tool, "edit");
    assert_eq!(
        preview,
        &Some(Box::new(FilePreview {
            before: "before\n".into(),
            after: "after\n".into()
        }))
    );

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
    let command = if cfg!(windows) {
        "set /a tc=40+2\r\necho tiechui-%tc%\r\n"
    } else {
        "echo tiechui-$((40 + 2))\n"
    };
    let echo = json!({"id": "t", "data": command});
    client.ok("terminal/write", echo).await;
    let mut output = String::new();
    #[cfg(windows)]
    let mut cursor_reported = false;
    while !output.contains("tiechui-42") {
        let notice = client.notice(|method, _| method == "terminal/output").await;
        output.push_str(notice["data"].as_str().unwrap());
        #[cfg(windows)]
        if !cursor_reported && output.contains("\x1b[6n") {
            client
                .ok("terminal/write", json!({"id": "t", "data": "\x1b[1;1R"}))
                .await;
            cursor_reported = true;
        }
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

#[tokio::test]
async fn git_unstage_and_discard_keep_stdout_valid_json_rpc() {
    let fixture = Fixture::new().await;
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(&fixture.cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    };
    git(&["init", "-q"]);
    git(&["config", "core.autocrlf", "false"]);
    let mut client = fixture.client().await;
    let root = fixture.cwd.display().to_string();
    let op = |kind| json!({"root":root, "op":{"kind":kind,"paths":["hello.txt"]}});
    // An unborn repository uses `rm --cached` and has no HEAD output either.
    client.ok("git/run", op("stage")).await;
    client.ok("git/run", op("unstage")).await;
    client.ok("git/run", op("stage")).await;
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.test",
        "commit",
        "-qm",
        "initial",
    ]);
    std::fs::write(fixture.cwd.join("hello.txt"), "after\n").unwrap();
    client.ok("git/run", op("stage")).await;
    client.ok("git/run", op("unstage")).await;
    client.ok("git/run", op("discard")).await;
    // Client::read rejects any non-JSON line, including a leaked commit hash.
    assert_eq!(
        client
            .ok("fs/read", json!({"root":root,"path":"hello.txt"}))
            .await["text"],
        "before\n"
    );
}

#[tokio::test]
async fn turn_undo_and_rewind_through_app_server() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client().await;
    let cwd = fixture.cwd.display().to_string();
    let hello = fixture.cwd.join("hello.txt");
    let opened = snapshot(
        client
            .ok("session/open", json!({"cwd": cwd, "mode": "yolo"}))
            .await,
    );
    let thread = json!(opened.thread_id);
    client
        .ok(
            "session/submit",
            json!({"thread_id": thread, "op": {"kind": "user_input", "delivery": "follow_up",
                "content": [{"kind": "text", "text": "Change hello.txt to after"}]}}),
        )
        .await;
    let finished = client
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    let EventKind::TurnCompleted { turn } = &finished.last().unwrap().event.event else {
        unreachable!()
    };
    let message = turn
        .items
        .iter()
        .find_map(|item| match item {
            Item::Known(KnownItem::UserMessage { id, .. }) => Some(id.0.clone()),
            _ => None,
        })
        .unwrap();
    let params = json!({"thread_id": thread, "item_id": message});
    let changes = |reverted: bool| {
        json!({"changes": [{"path": "hello.txt", "before": "before\n", "after": "after\n"}],
            "reverted": reverted})
    };
    assert_eq!(
        client.ok("turn/changes", params.clone()).await,
        changes(false)
    );
    assert_eq!(
        client.ok("turn/check", params.clone()).await,
        json!({"safe": ["hello.txt"], "unsafe": [], "ignored": []})
    );

    // Undo and redo, then refuse both while the file differs from what the turn left.
    assert_eq!(client.ok("turn/undo", params.clone()).await, changes(true));
    assert_eq!(std::fs::read_to_string(&hello).unwrap(), "before\n");
    assert_eq!(client.ok("turn/undo", params.clone()).await, changes(false));
    assert_eq!(std::fs::read_to_string(&hello).unwrap(), "after\n");
    std::fs::write(&hello, "edited\n").unwrap();
    assert_eq!(
        client.ok("turn/check", params.clone()).await["unsafe"],
        json!(["hello.txt"])
    );
    assert!(client.call("turn/undo", params.clone()).await.is_err());
    assert_eq!(
        client.ok("turn/rewind", params.clone()).await,
        json!({"conflicts": ["hello.txt"]})
    );
    assert_eq!(std::fs::read_to_string(&hello).unwrap(), "edited\n");

    std::fs::write(&hello, "after\n").unwrap();
    assert_eq!(
        client.ok("turn/rewind", params.clone()).await,
        json!({"conflicts": []})
    );
    assert_eq!(std::fs::read_to_string(&hello).unwrap(), "before\n");

    // Its file versions follow the session into the trash.
    let trashed = client
        .ok("session/trash", json!({"path": opened.path}))
        .await;
    let trashed = Path::new(trashed["path"].as_str().unwrap()).with_extension("rewind");
    assert!(trashed.join("manifest.json").exists());
    fixture.server.verify().unwrap();
}

#[tokio::test]
async fn user_settings_round_trip_and_inactive_sessions_are_archived() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client().await;
    let cwd = fixture.cwd.display().to_string();
    // Two sessions whose only message is from 1970 and from now.
    let cancel = CancellationToken::new();
    let session = |timestamp: u64| {
        let path = tiechui_session::SessionStore::create(&fixture.home, &fixture.cwd, &cancel)
            .unwrap()
            .path()
            .to_owned();
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str(&format!(
            "{}\n",
            json!({"id": format!("m{timestamp}"), "parent_id": null, "timestamp": timestamp,
                "kind": "message", "model": null,
                "message": {"role": "user", "content": [{"kind": "text", "text": "hi"}]}})
        ));
        std::fs::write(&path, text).unwrap();
        path.display().to_string()
    };
    let old = session(1000);
    session(tiechui_session::now_ms());

    let get = json!({"model": "fixture/model", "thinking": null, "proxy": null, "no_proxy": null,
        "ca_cert": null, "auto_continue_after_sec": null, "archive_after_days": null});
    assert_eq!(client.ok("config/get", json!({})).await, get);
    for (key, value) in [
        ("network.proxy", json!("http://127.0.0.1:9")),
        ("network.no_proxy", json!("localhost")),
        ("ask.auto_continue_after_sec", json!(300)),
    ] {
        client
            .ok("config/update", json!({"key": key, "value": value}))
            .await;
    }
    let mut expected = get.clone();
    expected["proxy"] = json!("http://127.0.0.1:9");
    expected["no_proxy"] = json!("localhost");
    expected["auto_continue_after_sec"] = json!(300);
    assert_eq!(client.ok("config/get", json!({})).await, expected);

    // Rejected values leave the file as it was.
    let text = std::fs::read_to_string(fixture.home.join("config.toml")).unwrap();
    for (key, value) in [
        ("network.proxy", json!("not a url")),
        ("network.ca_cert", json!("/missing/ca.pem")),
        ("ask.auto_continue_after_sec", json!(0)),
        ("sessions.archive_after_days", json!("soon")),
    ] {
        assert!(
            client
                .call("config/update", json!({"key": key, "value": value}))
                .await
                .is_err(),
            "{key}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(fixture.home.join("config.toml")).unwrap(),
        text
    );
    client
        .ok(
            "config/update",
            json!({"key": "network.proxy", "value": null}),
        )
        .await;
    assert_eq!(
        client.ok("config/get", json!({})).await["proxy"],
        json!(null)
    );

    // Setting the period archives the inactive session at once.
    client
        .ok(
            "config/update",
            json!({"key": "sessions.archive_after_days", "value": 30}),
        )
        .await;
    let archived = client
        .notice(|method, _| method == "session/archived")
        .await;
    assert_eq!(archived, json!({"path": old, "archived": true}));
    let listed: Vec<SessionSummary> = serde_json::from_value(
        client
            .ok("session/list", json!({"cwd": cwd, "archived": true}))
            .await,
    )
    .unwrap();
    let archived: Vec<_> = listed
        .into_iter()
        .filter(|s| s.archived_at.is_some())
        .map(|s| s.path)
        .collect();
    assert_eq!(archived, [old]);
}

#[tokio::test]
async fn runtime_settings_reload_open_sessions_but_defaults_do_not() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client().await;
    let opened = snapshot(client.ok("session/open", json!({"cwd":fixture.cwd})).await);
    let thread = json!({"thread_id":opened.thread_id});
    let mut epoch = opened.events[0].epoch.0;
    client.events.clear();
    for (key, value) in [
        ("network.proxy", json!("http://127.0.0.1:9")),
        ("network.no_proxy", json!("localhost")),
        ("ask.auto_continue_after_sec", json!(1)),
        ("network.proxy", Value::Null),
        ("network.no_proxy", Value::Null),
        ("ask.auto_continue_after_sec", Value::Null),
        ("network.ca_cert", Value::Null),
    ] {
        client
            .ok("config/update", json!({"key":key,"value":value}))
            .await;
        let replay = client
            .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
            .await;
        epoch += 1;
        assert_eq!(replay.last().unwrap().event.epoch.0, epoch, "{key}");
        let current = snapshot(client.ok("session/snapshot", thread.clone()).await);
        assert_eq!(current.events[0].epoch.0, epoch);
    }
    for (key, value) in [
        ("model", json!("fixture/model")),
        ("thinking", json!("off")),
    ] {
        client
            .ok("config/update", json!({"key":key,"value":value}))
            .await;
    }
    assert!(
        client
            .call(
                "config/update",
                json!({"key":"ask.auto_continue_after_sec","value":0})
            )
            .await
            .is_err()
    );
    let current = snapshot(client.ok("session/snapshot", thread).await);
    assert_eq!(current.events[0].epoch.0, epoch);
}

#[tokio::test]
async fn runtime_settings_wait_for_an_active_approval_before_reloading() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client().await;
    let opened = snapshot(
        client
            .ok("session/open", json!({"cwd":fixture.cwd,"mode":"rules"}))
            .await,
    );
    let thread = opened.thread_id;
    client
        .ok(
            "session/submit",
            json!({"thread_id":thread,"op":{"kind":"user_input","delivery":"steer",
        "content":[{"kind":"text","text":"Change hello.txt to after"}]}}),
        )
        .await;
    let events = client
        .until(|event| matches!(event, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested { approval_id, .. } = &events.last().unwrap().event.event
    else {
        unreachable!()
    };
    client
        .ok(
            "config/update",
            json!({"key":"network.no_proxy","value":"localhost"}),
        )
        .await;
    client
        .ok(
            "config/update",
            json!({"key":"ask.auto_continue_after_sec","value":1}),
        )
        .await;
    let waiting = snapshot(
        client
            .ok("session/snapshot", json!({"thread_id":thread}))
            .await,
    );
    assert_eq!(waiting.events[0].epoch.0, 0);
    assert!(
        waiting
            .events
            .iter()
            .any(|event| matches!(event.event, EventKind::ApprovalRequested { .. }))
    );
    client
        .ok(
            "session/submit",
            json!({"thread_id":thread,"op":{"kind":"approval_response",
        "approval_id":approval_id,"decision":{"kind":"allow_once"}}}),
        )
        .await;
    client
        .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    let reloaded = snapshot(
        client
            .ok("session/snapshot", json!({"thread_id":thread}))
            .await,
    );
    assert_eq!(reloaded.events[0].epoch.0, 1);
    assert_eq!(
        std::fs::read_to_string(fixture.cwd.join("hello.txt")).unwrap(),
        "after\n"
    );
    fixture.server.verify().unwrap();
}

#[tokio::test]
async fn changed_ask_timeout_is_used_by_an_already_open_session() {
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let fixture = Fixture::new().await;
    let provider = MockServer::start().await;
    let args = json!({"questions":[{"id":"choice","header":"Choice","question":"Which option?"}]});
    let ask = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,
        "delta":{"tool_calls":[{"index":0,"id":"call_ask","type":"function","function":{
            "name":"request_user_input","arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]})
    );
    let answer = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,
        "delta":{"content":"Finished"},"finish_reason":"stop"}]})
    );
    Mock::given(method("POST"))
        .and(body_string_contains(
            "The user did not answer within 1 seconds.",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_raw(answer, "text/event-stream"))
        .with_priority(1)
        .expect(1)
        .mount(&provider)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(ask, "text/event-stream"))
        .with_priority(2)
        .expect(1)
        .mount(&provider)
        .await;
    let providers = fixture.home.join("providers.toml");
    let config = std::fs::read_to_string(&providers).unwrap();
    std::fs::write(
        &providers,
        config.replace(&fixture.server.uri(), &provider.uri()),
    )
    .unwrap();
    let mut client = fixture.client().await;
    let opened = snapshot(
        client
            .ok("session/open", json!({"cwd":fixture.cwd,"mode":"yolo"}))
            .await,
    );
    client.events.clear();
    client
        .ok(
            "config/update",
            json!({"key":"ask.auto_continue_after_sec","value":1}),
        )
        .await;
    client
        .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    client
        .ok(
            "session/submit",
            json!({"thread_id":opened.thread_id,"op":{"kind":"user_input",
        "delivery":"steer","content":[{"kind":"text","text":"Ask me a question"}]}}),
        )
        .await;
    client
        .until(|event| {
            matches!(
                event,
                EventKind::ApprovalRequested {
                    request: ApprovalRequest::Questions { .. },
                    ..
                }
            )
        })
        .await;
    let finished = client
        .until(|event| matches!(event, EventKind::TurnCompleted { .. }))
        .await;
    assert!(
        finished
            .iter()
            .any(|event| matches!(event.event.event, EventKind::ApprovalResolved { .. }))
    );
    let EventKind::TurnCompleted { turn } = &finished.last().unwrap().event.event else {
        unreachable!()
    };
    assert_eq!(turn.status, TurnStatus::Completed);
    provider.verify().await;
}

#[tokio::test]
async fn providers_and_defaults_are_edited_and_reload_open_sessions() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client().await;
    let cwd = fixture.cwd.display().to_string();
    let providers = fixture.home.join("providers.toml");
    let config = fixture.home.join("config.toml");
    client.ok("session/open", json!({"cwd": cwd})).await;
    let listed = client.ok("providers/list", json!({})).await;
    assert_eq!(
        listed,
        json!([{"name": "fixture", "wire": "openai-completions", "base_url": fixture.server.uri(),
            "api_key": false, "api_key_env": "TC_E2E_FAKE_KEY", "models": [{"id": "model",
            "context_window": 200000, "max_output": 4096, "levels": ["off"], "mapping": {},
            "images": false}]}])
    );

    let second = json!({"id": "second", "context_window": 1000, "max_output": 100,
        "levels": ["off", "low", "high"], "mapping": {"high": "max-effort"}, "images": true});
    client
        .ok(
            "providers/save_model",
            json!({"provider": "fixture", "previous": null, "model": second}),
        )
        .await;
    let text = std::fs::read_to_string(&providers).unwrap();
    assert!(
        text.contains(
            "thinking_levels = { minimal = false, medium = false, high = \"max-effort\" }"
        )
    );
    assert_eq!(
        client.ok("providers/list", json!({})).await[0]["models"][1],
        second
    );
    // The open session reloads and lists the new model.
    client
        .until(|e| {
            matches!(e, EventKind::ModelsUpdated { models } if models.iter().any(|m| m.id == "second"))
        })
        .await;
    assert!(
        client
            .call(
                "providers/save_model",
                json!({"provider": "fixture", "previous": null, "model": second}),
            )
            .await
            .is_err()
    );

    client
        .ok(
            "config/update",
            json!({"key": "model", "value": "fixture/second"}),
        )
        .await;
    client
        .ok("config/update", json!({"key": "thinking", "value": "high"}))
        .await;
    for (key, value) in [
        ("model", json!("fixture/none")),
        ("tui.theme", json!("dark")),
    ] {
        assert!(
            client
                .call("config/update", json!({"key": key, "value": value}))
                .await
                .is_err()
        );
    }
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "model = \"fixture/second\"\nthinking = \"high\"\n"
    );

    client
        .ok(
            "providers/save",
            json!({"name": "other", "wire": "anthropic-messages", "base_url": null, "api_key": "k"}),
        )
        .await;
    let listed = client.ok("providers/list", json!({})).await;
    assert_eq!(listed[1]["name"], "other");
    assert_eq!(listed[1]["api_key"], true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&providers).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // The scripted server answers only the conversation, so the check reports its refusal.
    let tested = client
        .ok(
            "providers/test",
            json!({"provider": "fixture", "id": "model"}),
        )
        .await;
    assert!(tested["error"].is_string());
    client
        .ok("providers/delete", json!({"name": "other"}))
        .await;
    client
        .ok(
            "providers/delete_model",
            json!({"provider": "fixture", "id": "second"}),
        )
        .await;
    let listed = client.ok("providers/list", json!({})).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["models"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn mcp_servers_are_edited_started_and_signed_in_to() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let fixture = Fixture::new().await;
    let mut client = fixture.client().await;
    let cwd = fixture.cwd.display().to_string();
    let oauth = MockServer::start().await;
    let base = oauth.uri();
    for (route, body) in [
        (
            "/.well-known/oauth-protected-resource/mcp",
            json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]}),
        ),
        (
            "/.well-known/oauth-authorization-server",
            json!({"issuer": base, "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
                "registration_endpoint": format!("{base}/register"),
                "code_challenge_methods_supported": ["S256"]}),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&oauth)
            .await;
    }
    for (route, body) in [
        ("/register", json!({"client_id": "fixture"})),
        (
            "/token",
            json!({"access_token": "fake", "token_type": "Bearer", "expires_in": 3600}),
        ),
    ] {
        Mock::given(method("POST"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&oauth)
            .await;
    }
    client.ok("session/open", json!({"cwd": cwd})).await;

    let server = |name: &str, command: Value, url: Value, enabled: bool| {
        json!({"name": name, "command": command, "args": ["--flag"], "env": {}, "url": url,
            "http_headers": {}, "tool_timeout_sec": 30.0, "enabled": enabled})
    };
    let local = server("local", json!("/nonexistent/mcp-server"), json!(null), true);
    let remote = server("remote", json!(null), json!(format!("{base}/mcp")), false);
    for server in [&local, &remote] {
        client
            .ok("mcp/save", json!({"previous": null, "server": server}))
            .await;
    }
    // The open session reloads.
    client
        .until(|e| matches!(e, EventKind::ThreadStarted { .. }))
        .await;
    for (previous, server) in [
        (
            json!(null),
            server("both", json!("x"), json!("http://a"), true),
        ),
        (json!(null), server("none", json!(null), json!(""), true)),
        (json!(null), server("local", json!("x"), json!(null), true)),
        (json!("missing"), server("x", json!("x"), json!(null), true)),
    ] {
        assert!(
            client
                .call("mcp/save", json!({"previous": previous, "server": server}))
                .await
                .is_err()
        );
    }
    assert_eq!(
        client.ok("mcp/list", json!({})).await,
        json!([local, remote])
    );
    let status = client.ok("mcp/status", json!({})).await;
    assert_eq!(
        (
            &status[0]["state"],
            &status[1]["state"],
            &status[1]["oauth"]
        ),
        (&json!("failed"), &json!("disabled"), &json!("logged_out"))
    );
    assert!(status[0]["error"].is_string());

    let login = client.send("mcp/login", json!({"name": "remote"})).await;
    let page = client.notice(|method, _| method == "mcp/authorize").await;
    assert_eq!(page["name"], "remote");
    let page = reqwest::Url::parse(page["url"].as_str().unwrap()).unwrap();
    let fields: std::collections::BTreeMap<_, _> = page.query_pairs().into_owned().collect();
    let mut callback = reqwest::Url::parse(&fields["redirect_uri"]).unwrap();
    callback
        .query_pairs_mut()
        .extend_pairs([("code", "fixture"), ("state", fields["state"].as_str())]);
    assert!(reqwest::get(callback).await.unwrap().status().is_success());
    client.reply(login).await.unwrap();
    assert_eq!(
        client.ok("mcp/status", json!({})).await[1]["oauth"],
        "authenticated"
    );
    client.ok("mcp/logout", json!({"name": "remote"})).await;
    assert_eq!(
        client.ok("mcp/status", json!({})).await[1]["oauth"],
        "logged_out"
    );
    assert!(
        client
            .call("mcp/login", json!({"name": "local"}))
            .await
            .is_err()
    );

    let renamed = server(
        "renamed",
        json!("/nonexistent/mcp-server"),
        json!(null),
        false,
    );
    client
        .ok("mcp/save", json!({"previous": "local", "server": renamed}))
        .await;
    client.ok("mcp/delete", json!({"name": "remote"})).await;
    assert_eq!(client.ok("mcp/list", json!({})).await, json!([renamed]));
}

#[tokio::test]
async fn skills_are_listed_and_switched_off_for_open_sessions() {
    let fixture = Fixture::new().await;
    for (dir, name) in [
        (fixture.home.join("skills/review"), "review"),
        (fixture.cwd.join(".agents/skills/deploy"), "deploy"),
    ] {
        std::fs::create_dir_all(&dir).unwrap();
        let text = format!("---\nname: {name}\ndescription: Fixture\n---\nBody\n");
        std::fs::write(dir.join("SKILL.md"), text).unwrap();
    }
    let mut client = fixture.client().await;
    let cwd = fixture.cwd.display().to_string();
    client.ok("trust", json!({"cwd": cwd, "allow": true})).await;
    client.ok("session/open", json!({"cwd": cwd})).await;
    let states = |list: &Value| -> Vec<(String, String, bool)> {
        list["skills"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                let text = |key: &str| s[key].as_str().unwrap().to_owned();
                (text("name"), text("source"), s["enabled"] == true)
            })
            .collect()
    };
    let skill = |name: &str, source: &str, enabled| (name.to_owned(), source.to_owned(), enabled);
    let list = client.ok("skills/list", json!({"cwd": cwd})).await;
    assert_eq!(
        list["dir"],
        fixture.home.join("skills").display().to_string()
    );
    assert_eq!(
        states(&list),
        [
            skill("review", "user", true),
            skill("deploy", "project", true)
        ]
    );
    let user = client.ok("skills/list", json!({"cwd": null})).await;
    assert_eq!(states(&user), [skill("review", "user", true)]);

    client
        .ok("skills/enable", json!({"name": "review", "enabled": false}))
        .await;
    client
        .until(|e| {
            matches!(e, EventKind::ResourcesUpdated { resources }
                if resources.iter().any(|r| r.name == "deploy")
                    && resources.iter().all(|r| r.name != "review"))
        })
        .await;
    let config = std::fs::read_to_string(fixture.home.join("config.toml")).unwrap();
    assert!(config.contains("disabled = [\"review\"]"), "{config}");
    let list = client.ok("skills/list", json!({"cwd": cwd})).await;
    assert_eq!(
        states(&list),
        [
            skill("review", "user", false),
            skill("deploy", "project", true)
        ]
    );
    client
        .ok("skills/enable", json!({"name": "review", "enabled": true}))
        .await;
    let config = std::fs::read_to_string(fixture.home.join("config.toml")).unwrap();
    assert!(!config.contains("review"), "{config}");
}

#[tokio::test]
async fn prompts_are_saved_as_commands_that_send_their_expanded_text() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client().await;
    let cwd = fixture.cwd.display().to_string();
    let opened = snapshot(client.ok("session/open", json!({"cwd": cwd})).await);
    let names = |list: &Value| -> Vec<(String, String)> {
        list["prompts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["name"].as_str().unwrap().into(),
                    p["source"].as_str().unwrap().into(),
                )
            })
            .collect()
    };
    let save = |scope: &str, name: &str, previous: Value| {
        json!({"cwd": cwd, "scope": scope, "name": name, "description": "Edit a file",
            "argument_hint": "<change>", "body": "Change $ARGUMENTS", "previous": previous})
    };
    client
        .ok("prompts/save", save("user", "edit", Value::Null))
        .await;
    let error = client
        .call("prompts/save", save("user", "edit", Value::Null))
        .await
        .unwrap_err();
    assert_eq!(error["message"], "/edit already exists");
    client
        .until(|e| {
            matches!(e, EventKind::CommandsUpdated { commands }
                if commands.iter().any(|c| c.name == "edit" && c.argument_hint.as_deref() == Some("<change>")))
        })
        .await;
    let list = client.ok("prompts/list", json!({"cwd": cwd})).await;
    assert_eq!(
        list["dir"],
        fixture.home.join("prompts").display().to_string()
    );
    assert_eq!(names(&list), [("edit".into(), "user".into())]);
    let user = list["prompts"][0]["path"].clone();
    let file = std::fs::read_to_string(user.as_str().unwrap()).unwrap();
    assert!(
        file.starts_with("---\ndescription: \"Edit a file\"\n"),
        "{file}"
    );

    client
        .ok(
            "session/submit",
            json!({"thread_id": opened.thread_id, "op": {"kind": "run_command",
                "name": "edit", "arguments": "hello.txt to after"}}),
        )
        .await;
    client
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;

    // Untrusted project prompts are reported, not listed.
    client
        .ok("prompts/save", save("project", "change", user.clone()))
        .await;
    assert!(!Path::new(user.as_str().unwrap()).exists());
    let list = client.ok("prompts/list", json!({"cwd": cwd})).await;
    assert_eq!(names(&list), []);
    assert!(
        list["warnings"][0]
            .as_str()
            .unwrap()
            .ends_with("skipping untrusted project prompts")
    );
    client.ok("trust", json!({"cwd": cwd, "allow": true})).await;
    let list = client.ok("prompts/list", json!({"cwd": cwd})).await;
    assert_eq!(names(&list), [("change".into(), "project".into())]);
    let path = list["prompts"][0]["path"].clone();
    let outside = fixture.cwd.join("hello.txt").display().to_string();
    assert!(
        client
            .call("prompts/delete", json!({"cwd": cwd, "path": outside}))
            .await
            .is_err()
    );
    client
        .ok("prompts/delete", json!({"cwd": cwd, "path": path}))
        .await;
    let list = client.ok("prompts/list", json!({"cwd": cwd})).await;
    assert_eq!(names(&list), []);
}

#[tokio::test]
async fn agent_definitions_are_listed_saved_moved_and_deleted() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client().await;
    let cwd = fixture.cwd.display().to_string();
    let entries = |list: &Value| -> Vec<(String, String)> {
        list["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["name"].as_str().unwrap().into(),
                    a["source"].as_str().unwrap().into(),
                )
            })
            .collect()
    };
    let list = client.ok("agents/list", json!({"cwd": cwd})).await;
    assert_eq!(
        list["dir"],
        fixture.home.join("agents").display().to_string()
    );
    assert_eq!(entries(&list), []);

    let user = fixture.home.join("agents").join("custom.md");
    std::fs::create_dir_all(user.parent().unwrap()).unwrap();
    std::fs::write(
        &user,
        "---\nname: custom\ndescription: old\npermission:\n  mode: rules\n# keep comment\n---\nOld",
    )
    .unwrap();
    std::fs::write(
        fixture.home.join("agents").join("broken.md"),
        "---\nname: broken\ndescription: bad\nthinking: bogus\n---\nBroken",
    )
    .unwrap();
    let save = |scope: &str, thinking: Value, previous: Value| {
        json!({"cwd": cwd, "scope": scope, "name": "custom", "description": "Checks: things",
            "tools": ["read", "grep"], "model": "fixture/model", "thinking": thinking,
            "prompt": "Check it", "previous": previous})
    };
    for bad in [json!("bogus"), json!(""), json!("HIGH")] {
        let error = client
            .call("agents/save", save("user", bad.clone(), json!(user)))
            .await
            .unwrap_err();
        assert!(
            error["message"]
                .as_str()
                .unwrap()
                .contains("Invalid thinking level"),
            "{bad}: {error}"
        );
    }
    assert!(
        std::fs::read_to_string(&user)
            .unwrap()
            .contains("description: old")
    );

    client
        .ok("agents/save", save("user", json!("medium"), json!(user)))
        .await;
    let text = std::fs::read_to_string(&user).unwrap();
    assert!(
        text.contains(
            "tools: [\"read\",\"grep\"]\nmodel: \"fixture/model\"\nthinking: \"medium\"\npermission:\n  mode: rules\n# keep comment\n"
        ),
        "{text}"
    );
    let list = client.ok("agents/list", json!({"cwd": null})).await;
    assert!(
        list["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["name"] != "broken")
    );
    let custom = list["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "custom")
        .unwrap()
        .clone();
    assert_eq!(
        custom,
        json!({"name": "custom", "description": "Checks: things", "tools": ["read", "grep"],
            "model": "fixture/model", "thinking": "medium", "prompt": "Check it",
            "path": user, "source": "user"})
    );
    client
        .ok("agents/save", save("user", json!("inherit"), json!(user)))
        .await;
    let list = client.ok("agents/list", json!({"cwd": null})).await;
    let custom = list["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "custom")
        .unwrap();
    assert_eq!(custom["thinking"], json!("inherit"));
    let error = client
        .call("agents/save", save("user", Value::Null, Value::Null))
        .await
        .unwrap_err();
    assert_eq!(error["message"], "custom already exists");

    client
        .ok("agents/save", save("project", Value::Null, json!(user)))
        .await;
    assert!(!user.exists());
    let project = fixture
        .cwd
        .canonicalize()
        .unwrap()
        .join(".agents/agents/custom.md");
    let moved = std::fs::read_to_string(&project).unwrap();
    assert!(!moved.contains("thinking:"), "{moved}");
    assert!(
        moved.contains("permission:\n  mode: rules\n# keep comment\n"),
        "{moved}"
    );
    let list = client.ok("agents/list", json!({"cwd": cwd})).await;
    let moved_entry = list["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "custom")
        .unwrap();
    assert_eq!(moved_entry["source"], json!("project"));
    assert_eq!(moved_entry["thinking"], Value::Null);
    let outside = fixture.cwd.join("hello.txt");
    assert!(
        client
            .call("agents/delete", json!({"cwd": cwd, "path": outside}))
            .await
            .is_err()
    );
    client
        .ok("agents/delete", json!({"cwd": cwd, "path": project}))
        .await;
    assert!(!project.exists());
}

#[tokio::test]
async fn hooks_are_saved_approved_and_deleted() {
    let fixture = Fixture::new().await;
    let mut client = fixture.client().await;
    let cwd = fixture.cwd.display().to_string();
    let list = || json!({"cwd": cwd});
    let summary = |list: &Value| -> Vec<(String, String, String)> {
        list["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h["command"].as_str().unwrap().into(),
                    h["source"].as_str().unwrap().into(),
                    h["trust"].as_str().unwrap().into(),
                )
            })
            .collect()
    };
    let save = |scope: &str, command: &str, previous: Value| {
        json!({"cwd": cwd, "scope": scope, "event": "PreToolUse", "matcher": "Bash",
            "command": command, "timeout": 30, "previous": previous})
    };
    let hooks = client.ok("hooks/list", list()).await;
    assert_eq!(
        hooks["path"],
        fixture.home.join("hooks.json").display().to_string()
    );
    assert_eq!(hooks["hooks"], json!([]));

    client
        .ok("hooks/save", save("project", "true", Value::Null))
        .await;
    let hooks = client.ok("hooks/list", list()).await;
    assert_eq!(
        summary(&hooks),
        [("true".into(), "project".into(), "untrusted_project".into())]
    );
    let entry = &hooks["hooks"][0];
    assert_eq!(
        (
            &entry["event"],
            &entry["matcher"],
            &entry["timeout"],
            &entry["editable"]
        ),
        (
            &json!("PreToolUse"),
            &json!("Bash"),
            &json!(30),
            &json!(true)
        )
    );
    client.ok("trust", json!({"cwd": cwd, "allow": true})).await;
    let hooks = client.ok("hooks/list", list()).await;
    assert_eq!(hooks["hooks"][0]["trust"], "trusted");

    let file = fixture
        .cwd
        .canonicalize()
        .unwrap()
        .join(".agents/hooks.json");
    let mut document: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    document["hooks"]["Stop"] = json!([{"hooks": [{"type": "command", "command": "false"}]}]);
    std::fs::write(&file, document.to_string()).unwrap();
    let hooks = client.ok("hooks/list", list()).await;
    let stop = hooks["hooks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["command"] == "false")
        .unwrap();
    assert_eq!(stop["trust"], "unapproved");
    client
        .ok("hooks/approve", json!({"cwd": cwd, "keys": [stop["key"]]}))
        .await;
    let hooks = client.ok("hooks/list", list()).await;
    assert!(
        summary(&hooks)
            .iter()
            .all(|(_, _, trust)| trust == "trusted")
    );

    let key = hooks["hooks"][0]["key"].clone();
    client.ok("hooks/save", save("user", "echo", key)).await;
    let hooks = client.ok("hooks/list", list()).await;
    assert_eq!(
        summary(&hooks),
        [
            ("echo".into(), "user".into(), "user".into()),
            ("false".into(), "project".into(), "trusted".into())
        ]
    );
    let error = client
        .call(
            "hooks/save",
            json!({"cwd": cwd, "scope": "user", "event": "Nope",
            "matcher": null, "command": "true", "timeout": 1, "previous": null}),
        )
        .await
        .unwrap_err();
    assert_eq!(error["message"], "Nope is not a hook event");
    for hook in hooks["hooks"].as_array().unwrap() {
        client
            .ok("hooks/delete", json!({"cwd": cwd, "key": hook["key"]}))
            .await;
    }
    let hooks = client.ok("hooks/list", list()).await;
    assert_eq!(hooks["hooks"], json!([]));
}

#[test]
#[ignore]
fn e2e_tcp_listener_helper() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "LISTEN_PORT={port}").unwrap();
    stdout.flush().unwrap();
    drop(stdout);

    for stream in listener.incoming() {
        let _ = stream;
    }
}

#[cfg(unix)]
impl Fixture {
    fn connect_client(&self, sock: &Path) -> Client {
        let mut child = support::command()
            .env("TIECHUI_HOME", &self.home)
            .env("HOME", self.root.path())
            .env("TC_E2E_FAKE_KEY", "not-a-real-key")
            .current_dir(&self.cwd)
            .args(["app-server", "--connect"])
            .arg(sock)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
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

    fn spawn_daemon(&self, sock: &Path, idle_ms: Option<u64>) -> Child {
        let mut cmd = support::command();
        cmd.env("TIECHUI_HOME", &self.home)
            .env("HOME", self.root.path())
            .env("TC_E2E_FAKE_KEY", "not-a-real-key")
            .current_dir(&self.cwd)
            .args(["app-server", "--socket"])
            .arg(sock)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(ms) = idle_ms {
            cmd.env("CHUI_DAEMON_IDLE_MS", ms.to_string());
        }
        cmd.spawn().unwrap()
    }
}

#[cfg(unix)]
async fn wait_for_socket(sock: &Path) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::UnixStream::connect(sock).await.is_ok() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon socket {} never became ready",
            sock.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(unix)]
async fn disconnect_bridge(client: Client) {
    let Client {
        mut _child, stdin, ..
    } = client;
    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), _child.wait())
        .await
        .expect("bridge process hung after stdin EOF")
        .unwrap();
    assert!(status.success(), "bridge exited with {status}");
}

#[cfg(unix)]
#[tokio::test]
async fn socket_daemon_serves_concurrent_clients_ports_and_reconnects_without_dropping_work() {
    use std::os::unix::fs::MetadataExt;

    let fixture = Fixture::new().await;
    let short_dir = tempfile::Builder::new()
        .prefix("tc-")
        .tempdir_in("/tmp")
        .unwrap();
    let sock = short_dir.path().join("run/sub/app.sock");

    let missing = support::command()
        .env("TIECHUI_HOME", &fixture.home)
        .env("HOME", fixture.root.path())
        .args(["app-server", "--connect"])
        .arg(&sock)
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(!missing.status.success());
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        stderr.contains(&sock.display().to_string()),
        "expected socket path in stderr: {stderr}"
    );

    let mut daemon = fixture.spawn_daemon(&sock, None);
    let daemon_pid = daemon.id().unwrap();
    wait_for_socket(&sock).await;

    let parent_mode = std::fs::metadata(sock.parent().unwrap()).unwrap().mode() & 0o777;
    let sock_mode = std::fs::symlink_metadata(&sock).unwrap().mode() & 0o777;
    let sock_ino = std::fs::symlink_metadata(&sock).unwrap().ino();
    assert_eq!(parent_mode, 0o700);
    assert_eq!(sock_mode, 0o600);

    let duplicate = support::command()
        .env("TIECHUI_HOME", &fixture.home)
        .env("HOME", fixture.root.path())
        .args(["app-server", "--socket"])
        .arg(&sock)
        .stdin(Stdio::null())
        .output()
        .await
        .unwrap();
    assert!(
        duplicate.status.success(),
        "duplicate daemon failed: {:?}",
        duplicate
    );
    assert_eq!(std::fs::symlink_metadata(&sock).unwrap().ino(), sock_ino);
    assert!(daemon.try_wait().unwrap().is_none());

    let mut client_a = fixture.connect_client(&sock);
    let mut client_b = fixture.connect_client(&sock);
    let (init_a, init_b) = tokio::join!(
        client_a.ok("initialize", json!({"protocol_version": 1})),
        client_b.ok("initialize", json!({"protocol_version": 1})),
    );
    let expected_home = json!(fixture.home.display().to_string());
    assert_eq!(init_a["home"], expected_home);
    assert_eq!(init_b["home"], expected_home);

    let cwd = fixture.cwd.display().to_string();
    let other_dir = fixture.root.path().join("other-client-repo");
    std::fs::create_dir_all(&other_dir).unwrap();
    let other_cwd = other_dir.display().to_string();
    let initial_ports = client_a.ok("ports/list", json!({"roots": [cwd]})).await;
    assert_eq!(initial_ports, json!([]));
    let other_ports = client_b
        .ok("ports/list", json!({"roots": [other_cwd]}))
        .await;
    assert_eq!(other_ports, json!([]));
    let cleared_b = client_b.ok("ports/list", json!({"roots": []})).await;
    assert_eq!(cleared_b, json!([]));

    let mut listener_proc = tokio::process::Command::new(std::env::current_exe().unwrap())
        .current_dir(&fixture.cwd)
        .args(["--ignored", "--nocapture", "e2e_tcp_listener_helper"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let listener_pid = listener_proc.id().unwrap();
    let mut port_lines = BufReader::new(listener_proc.stdout.take().unwrap()).lines();
    let listener_port: u16 = loop {
        let line = port_lines
            .next_line()
            .await
            .unwrap()
            .expect("helper exited without LISTEN_PORT");
        if let Some(p) = line.trim().strip_prefix("LISTEN_PORT=") {
            break p.parse().unwrap();
        }
    };

    let matches_added = |method: &str, params: &Value| {
        method == "ports/changed"
            && params["ports"].as_array().is_some_and(|ports| {
                ports.iter().any(|p| {
                    p["root"] == cwd && p["pid"] == listener_pid && p["port"] == listener_port
                })
            })
    };
    tokio::join!(
        client_a.notice(matches_added),
        client_b.notice(matches_added),
    );

    let stop_started = std::time::Instant::now();
    client_b
        .ok(
            "ports/stop",
            json!({"root": cwd, "pid": listener_pid, "port": listener_port}),
        )
        .await;
    let _ = listener_proc.wait().await;
    let matches_removed = |method: &str, params: &Value| {
        method == "ports/changed"
            && params["ports"]
                .as_array()
                .is_some_and(|ports| ports.iter().all(|p| p["pid"] != listener_pid))
    };
    tokio::join!(
        client_a.notice(matches_removed),
        client_b.notice(matches_removed),
    );
    assert!(stop_started.elapsed() < Duration::from_secs(1));

    let opened = snapshot(
        client_a
            .ok("session/open", json!({"cwd": cwd, "mode": "rules"}))
            .await,
    );
    let thread = json!(opened.thread_id);
    client_a
        .ok(
            "session/submit",
            json!({
                "thread_id": thread,
                "op": {
                    "kind": "user_input",
                    "delivery": "follow_up",
                    "content": [{"kind": "text", "text": "Change hello.txt to after"}]
                }
            }),
        )
        .await;
    let (asked_a, asked_b) = tokio::join!(
        client_a.until(|e| matches!(e, EventKind::ApprovalRequested { .. })),
        client_b.until(|e| matches!(e, EventKind::ApprovalRequested { .. })),
    );
    let EventKind::ApprovalRequested { approval_id, .. } = &asked_a.last().unwrap().event.event
    else {
        panic!("expected approval request");
    };
    let approval_id = approval_id.clone();
    assert!(matches!(
        asked_b.last().unwrap().event.event,
        EventKind::ApprovalRequested { .. }
    ));

    client_a
        .ok(
            "terminal/create",
            json!({"id": "t-persist", "cwd": cwd, "cols": 80, "rows": 24}),
        )
        .await;
    client_a
        .ok(
            "terminal/write",
            json!({"id": "t-persist", "data": "echo before-drop; sleep 0.35; echo after-drop\n"}),
        )
        .await;
    client_b
        .notice(|m, p| {
            m == "terminal/output"
                && p["id"] == "t-persist"
                && p["data"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("before-drop")
        })
        .await;

    disconnect_bridge(client_a).await;
    disconnect_bridge(client_b).await;
    tokio::time::sleep(Duration::from_millis(550)).await;
    assert_eq!(daemon.id(), Some(daemon_pid));
    assert!(daemon.try_wait().unwrap().is_none());

    let mut client_c = fixture.connect_client(&sock);
    client_c
        .ok("initialize", json!({"protocol_version": 1}))
        .await;
    let attached = client_c
        .ok("terminal/attach", json!({"id": "t-persist"}))
        .await;
    let buf = attached["buffer"].as_str().unwrap();
    assert!(
        buf.contains("before-drop") && buf.contains("after-drop"),
        "buffer={buf:?}"
    );
    assert!(attached["seq"].as_u64().unwrap() > 0);
    assert!(attached["exit"].is_null());

    let resumed = snapshot(
        client_c
            .ok("session/open", json!({"path": opened.path}))
            .await,
    );
    assert!(matches!(
        resumed.events.last().unwrap().event,
        EventKind::ApprovalRequested { .. }
    ));
    client_c
        .ok(
            "session/submit",
            json!({
                "thread_id": thread,
                "op": {
                    "kind": "approval_response",
                    "approval_id": approval_id,
                    "decision": {"kind": "allow_once"}
                }
            }),
        )
        .await;
    let completed = client_c
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    assert!(matches!(
        completed.last().unwrap().event.event,
        EventKind::TurnCompleted {
            turn: tiechui_protocol::Turn {
                status: TurnStatus::Completed,
                ..
            }
        }
    ));
    assert_eq!(
        std::fs::read_to_string(fixture.cwd.join("hello.txt")).unwrap(),
        "after\n"
    );

    client_c
        .ok("terminal/kill", json!({"id": "t-persist"}))
        .await;
    disconnect_bridge(client_c).await;
    let _ = daemon.kill().await;
}

#[cfg(unix)]
#[tokio::test]
async fn socket_daemon_replaces_stale_socket_and_auto_exits_after_full_idle() {
    let fixture = Fixture::new().await;
    let short_dir = tempfile::Builder::new()
        .prefix("tc-idle-")
        .tempdir_in("/tmp")
        .unwrap();
    let sock = short_dir.path().join("app.sock");

    {
        let _stale = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    }
    assert!(std::fs::symlink_metadata(&sock).is_ok());

    let mut daemon = fixture.spawn_daemon(&sock, Some(400));
    wait_for_socket(&sock).await;

    let mut client_1 = fixture.connect_client(&sock);
    client_1
        .ok("initialize", json!({"protocol_version": 1}))
        .await;
    let cwd = fixture.cwd.display().to_string();
    client_1
        .ok(
            "terminal/create",
            json!({"id": "keepalive", "cwd": cwd, "cols": 80, "rows": 24}),
        )
        .await;
    disconnect_bridge(client_1).await;

    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(
        daemon.try_wait().unwrap().is_none(),
        "daemon exited while a PTY terminal was still alive"
    );
    assert!(std::fs::symlink_metadata(&sock).is_ok());

    let mut client_2 = fixture.connect_client(&sock);
    client_2
        .ok("initialize", json!({"protocol_version": 1}))
        .await;
    client_2
        .ok("terminal/kill", json!({"id": "keepalive"}))
        .await;
    let opened = snapshot(
        client_2
            .ok("session/open", json!({"cwd": cwd, "mode": "rules"}))
            .await,
    );
    let thread = json!(opened.thread_id);
    client_2
        .ok(
            "session/submit",
            json!({
                "thread_id": thread,
                "op": {
                    "kind": "user_input",
                    "delivery": "follow_up",
                    "content": [{"kind": "text", "text": "Change hello.txt to after"}]
                }
            }),
        )
        .await;
    let asked = client_2
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested { approval_id, .. } = &asked.last().unwrap().event.event
    else {
        panic!("expected approval request");
    };
    let approval_id = approval_id.clone();
    disconnect_bridge(client_2).await;

    tokio::time::sleep(Duration::from_millis(650)).await;
    assert!(
        daemon.try_wait().unwrap().is_none(),
        "daemon exited while a session turn was still active"
    );

    let mut client_3 = fixture.connect_client(&sock);
    client_3
        .ok("initialize", json!({"protocol_version": 1}))
        .await;
    client_3
        .ok(
            "session/submit",
            json!({
                "thread_id": thread,
                "op": {
                    "kind": "approval_response",
                    "approval_id": approval_id,
                    "decision": {"kind": "allow_once"}
                }
            }),
        )
        .await;
    client_3
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    disconnect_bridge(client_3).await;

    let status = tokio::time::timeout(Duration::from_secs(5), daemon.wait())
        .await
        .expect("daemon did not auto-exit after full idle timeout")
        .unwrap();
    assert!(status.success(), "daemon exited with {status}");
    assert!(
        std::fs::symlink_metadata(&sock).is_err(),
        "daemon did not unlink socket on idle exit"
    );
}
