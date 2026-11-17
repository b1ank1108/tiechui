#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tiechui_protocol::app::{DaemonStatus, LiveState, SessionSummary, Snapshot};
use tiechui_protocol::{
    ApprovalId, ApprovalRequest, BTW_EVENT, BtwEvent, BtwView, EventKind, ThinkingLevel, ThreadId,
    TurnId, TurnStatus,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::Server;
use super::tests::{TestClient, isolated_home_child, sse_chat};

struct GateRoute {
    needle: String,
    reply: String,
    arrived: Arc<Notify>,
    release: Arc<Notify>,
}

struct LiveSseServer {
    uri: String,
    routes: Arc<Mutex<Vec<GateRoute>>>,
    requests: Arc<Mutex<Vec<String>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl LiveSseServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let routes: Arc<Mutex<Vec<GateRoute>>> = Arc::new(Mutex::new(Vec::new()));
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let routes_bg = routes.clone();
        let requests_bg = requests.clone();
        let handle = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let routes = routes_bg.clone();
                let requests = requests_bg.clone();
                tokio::spawn(async move {
                    serve_sse_conn(stream, routes, requests).await;
                });
            }
        });
        Self {
            uri: format!("http://{addr}"),
            routes,
            requests,
            handle,
        }
    }

    fn add_gated(&self, needle: &str, reply: &str) -> (Arc<Notify>, Arc<Notify>) {
        let arrived = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        self.routes.lock().unwrap().push(GateRoute {
            needle: needle.into(),
            reply: reply.into(),
            arrived: arrived.clone(),
            release: release.clone(),
        });
        (arrived, release)
    }

    fn take_requests(&self) -> Vec<String> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }
}

impl Drop for LiveSseServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

async fn read_http_body(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            return String::new();
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let content_len = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")?
                .trim()
                .parse::<usize>()
                .ok()
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_len {
        let n = stream.read(&mut chunk).await.unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf[header_end..]).into_owned()
}

async fn serve_sse_conn(
    mut stream: TcpStream,
    routes: Arc<Mutex<Vec<GateRoute>>>,
    requests: Arc<Mutex<Vec<String>>>,
) {
    let body = read_http_body(&mut stream).await;
    requests.lock().unwrap().push(body.clone());
    let matched = {
        let mut guard = routes.lock().unwrap();
        guard
            .iter()
            .rposition(|r| body.contains(&r.needle))
            .map(|idx| {
                let r = guard.remove(idx);
                (r.reply, r.arrived, r.release)
            })
    };
    let _ = stream
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
        )
        .await;
    let _ = stream.flush().await;
    if let Some((reply, arrived, release)) = matched {
        arrived.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(15), release.notified()).await;
        let _ = stream.write_all(reply.as_bytes()).await;
    } else {
        let _ = stream.write_all(sse_chat("ok").as_bytes()).await;
    }
    let _ = stream.flush().await;
}

struct SequenceResponder {
    steps: Mutex<Vec<ResponseTemplate>>,
    calls: AtomicUsize,
}

impl SequenceResponder {
    fn new(steps: Vec<ResponseTemplate>) -> Self {
        Self {
            steps: Mutex::new(steps),
            calls: AtomicUsize::new(0),
        }
    }
}

impl Respond for SequenceResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let idx = self.calls.fetch_add(1, Ordering::SeqCst);
        let steps = self.steps.lock().unwrap();
        steps
            .get(idx)
            .cloned()
            .unwrap_or_else(|| ResponseTemplate::new(500).set_body_string("unexpected extra call"))
    }
}

fn sse_tool_call(call_id: &str, name: &str, args: &Value) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": args.to_string()
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
    )
}

async fn wait_signal(signal: &Notify, label: &str) {
    tokio::time::timeout(Duration::from_secs(10), signal.notified())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
}

fn write_providers(home: &Path, main_uri: &str, side_uri: &str) {
    std::fs::write(
        home.join("providers.toml"),
        format!(
            "[alpha]\nwire = \"openai-completions\"\nbase_url = \"{main_uri}\"\napi_key = \"sk-alpha\"\n\
             models = [{{ id = \"main\", context_window = 128000, max_output = 4096, images = true }}]\n\n\
             [beta]\nwire = \"openai-completions\"\nbase_url = \"{side_uri}\"\napi_key = \"sk-beta\"\n\
             [[beta.models]]\nid = \"fast\"\ncontext_window = 128000\nmax_output = 4096\nreasoning = true\n\
             [beta.models.thinking_levels]\noff = \"none\"\nlow = \"low\"\nhigh = \"high\"\n\
             [[beta.models]]\nid = \"router/v1\"\ncontext_window = 64000\nmax_output = 4096\n"
        ),
    )
    .unwrap();
    std::fs::write(home.join("config.toml"), "model = \"alpha/main\"\n").unwrap();
}

fn parse_btw_event(msg: &Value, thread_id: &ThreadId) -> Option<BtwEvent> {
    if msg.get("method") != Some(&json!("session/event")) {
        return None;
    }
    let event: tiechui_protocol::app::SessionEvent =
        serde_json::from_value(msg["params"].clone()).ok()?;
    if &event.event.thread_id != thread_id {
        return None;
    }
    match event.event.event {
        EventKind::Extension { name, data } if name == BTW_EVENT => {
            serde_json::from_value(data).ok()
        }
        _ => None,
    }
}

async fn wait_btw_idle(client: &mut TestClient, thread_id: &ThreadId) -> BtwView {
    client
        .wait_matching("btw idle update", |msg| {
            match parse_btw_event(msg, thread_id)? {
                BtwEvent::Update { view, .. } if !view.busy && !view.messages.is_empty() => {
                    Some(view)
                }
                _ => None,
            }
        })
        .await
}

async fn wait_btw_closed(client: &mut TestClient, thread_id: &ThreadId) {
    client
        .wait_matching("btw closed event", |msg| {
            matches!(parse_btw_event(msg, thread_id), Some(BtwEvent::Closed)).then_some(())
        })
        .await;
}

#[tokio::test]
async fn btw_rpcs_concurrent_turns_share_stop_close_model_and_approvals() {
    let Some(home) = isolated_home_child(
        "TIECHUI_TEST_BTW_RPC_HOME",
        "app_server::btw_tests::btw_rpcs_concurrent_turns_share_stop_close_model_and_approvals",
    ) else {
        return;
    };
    run_btw_rpc_suite(home).await;
}

async fn run_btw_rpc_suite(home: PathBuf) {
    let work = home.parent().unwrap().join("work");
    std::fs::create_dir_all(&work).unwrap();

    let main_mock = MockServer::start().await;
    let side_mock = MockServer::start().await;
    write_providers(&home, &main_mock.uri(), &side_mock.uri());

    let mut client = TestClient::new(Server::new(home.clone()));

    verify_open_concurrent_ask_isolation_and_share(&mut client, &home, &work, &main_mock).await;
    verify_stop_during_active_main_turn(&mut client, &home, &work).await;
    write_providers(&home, &main_mock.uri(), &side_mock.uri());
    let _: Value = client.call_ok("providers/reload", json!({})).await;
    verify_close_snapshot_and_fresh_reopen(&mut client, &work, &main_mock).await;
    verify_model_switching(&mut client, &work, &main_mock, &side_mock).await;
    verify_side_tool_approvals(&mut client, &work, &main_mock).await;
    verify_image_input_deferred_reload_and_disabled_module(&mut client, &home, &work).await;

    client.close().await;
}

async fn verify_open_concurrent_ask_isolation_and_share(
    client: &mut TestClient,
    home: &Path,
    work: &Path,
    main_mock: &MockServer,
) {
    let live_sse = LiveSseServer::start().await;
    write_providers(home, &live_sse.uri, &main_mock.uri());
    let _: Value = client.call_ok("providers/reload", json!({})).await;
    let (prior_arrived, prior_release) =
        live_sse.add_gated("prior-main-question", &sse_chat("prior-main-answer"));
    prior_release.notify_one();

    let opened: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
    let thread_id = opened.thread_id.clone();
    client.drain_notices();

    let none_before: Option<BtwView> = client
        .call_ok(
            "btw/open",
            json!({ "thread_id": thread_id, "create": false }),
        )
        .await;
    assert_eq!(none_before, None);

    let share_empty = client
        .call("btw/share", json!({ "thread_id": thread_id }))
        .await
        .unwrap_err();
    assert_eq!(share_empty["code"], -32602, "{share_empty}");

    let _: Value = client
        .call_ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "user_input",
                    "delivery": "steer",
                    "content": [{ "kind": "text", "text": "prior-main-question" }]
                }
            }),
        )
        .await;
    wait_signal(&prior_arrived, "prior main request").await;
    client.wait_turn_completed(&thread_id).await;
    live_sse.take_requests();

    let created: Option<BtwView> = client
        .call_ok("btw/open", json!({ "thread_id": thread_id }))
        .await;
    let side_id = created.as_ref().unwrap().id.clone();
    assert!(side_id.starts_with("side-"));
    assert!(!created.unwrap().busy);

    let reattached: Option<BtwView> = client
        .call_ok(
            "btw/open",
            json!({ "thread_id": thread_id, "create": false }),
        )
        .await;
    assert_eq!(reattached.unwrap().id, side_id);

    for empty in [json!([]), json!([{ "kind": "text", "text": "   \n " }])] {
        assert!(
            client
                .call(
                    "btw/ask",
                    json!({ "thread_id": thread_id, "content": empty })
                )
                .await
                .is_err()
        );
    }
    assert!(
        client
            .call("btw/share", json!({ "thread_id": thread_id }))
            .await
            .is_err()
    );

    let (main_arrived, main_release) =
        live_sse.add_gated("active-main-task", &sse_chat("concurrent-main-reply"));
    let (side_arrived, side_release) = live_sse.add_gated(
        "side-question-while-busy",
        &sse_chat("concurrent-side-answer"),
    );

    client.drain_notices();
    let _: Value = client
        .call_ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "user_input",
                    "delivery": "steer",
                    "content": [{ "kind": "text", "text": "active-main-task" }]
                }
            }),
        )
        .await;
    wait_signal(&main_arrived, "main request arrival").await;

    let asked: BtwView = client
        .call_ok(
            "btw/ask",
            json!({
                "thread_id": thread_id,
                "content": [{ "kind": "text", "text": "side-question-while-busy" }]
            }),
        )
        .await;
    assert!(asked.busy);
    assert_eq!(asked.id, side_id);
    wait_signal(&side_arrived, "side request arrival").await;

    let status: DaemonStatus = client.call_ok("daemon/status", json!({})).await;
    assert_eq!(status.busy_sessions, 1);

    let busy_err = client
        .call(
            "btw/ask",
            json!({
                "thread_id": thread_id,
                "content": [{ "kind": "text", "text": "second-question-must-fail" }]
            }),
        )
        .await
        .unwrap_err();
    assert!(
        busy_err["message"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("running")
            || busy_err["message"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("busy"),
        "{busy_err}"
    );
    assert!(
        client
            .call(
                "btw/model",
                json!({ "thread_id": thread_id, "model": "beta/fast" })
            )
            .await
            .is_err()
    );

    side_release.notify_one();
    let finished_side = wait_btw_idle(client, &thread_id).await;
    assert_eq!(
        finished_side.messages.last().unwrap().text,
        "concurrent-side-answer"
    );
    assert!(finished_side.messages.last().unwrap().complete);

    main_release.notify_one();
    let main_events = client.wait_turn_completed(&thread_id).await;
    assert!(
        !main_events
            .iter()
            .any(|e| matches!(e, EventKind::Error { .. }))
    );

    let reqs = live_sse.take_requests();
    assert_eq!(reqs.len(), 2);
    let side_body = reqs
        .iter()
        .find(|b| b.contains("side-question-while-busy"))
        .unwrap();
    assert!(side_body.contains("<main_conversation>"));
    assert!(side_body.contains("prior-main-question"));
    assert!(side_body.contains("prior-main-answer"));
    assert!(!side_body.contains("active-main-task"));

    let (iso_arrived, iso_release) =
        live_sse.add_gated("check-main-isolation", &sse_chat("isolated-main-reply"));
    iso_release.notify_one();

    let _: Value = client
        .call_ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "user_input",
                    "delivery": "steer",
                    "content": [{ "kind": "text", "text": "check-main-isolation" }]
                }
            }),
        )
        .await;
    wait_signal(&iso_arrived, "isolated main arrival").await;
    client.wait_turn_completed(&thread_id).await;
    let isolated_reqs = live_sse.take_requests();
    assert_eq!(isolated_reqs.len(), 1);
    let isolated_body = &isolated_reqs[0];
    assert!(!isolated_body.contains("side-question-while-busy"));
    assert!(!isolated_body.contains("concurrent-side-answer"));

    let session_file = std::fs::read_to_string(&opened.path).unwrap();
    assert!(!session_file.contains("side-question-while-busy"));
    assert!(!session_file.contains("concurrent-side-answer"));

    let shared_view: BtwView = client
        .call_ok("btw/share", json!({ "thread_id": thread_id }))
        .await;
    assert!(
        shared_view
            .notice
            .as_deref()
            .unwrap_or_default()
            .contains("Shared")
    );
    assert!(live_sse.take_requests().is_empty());

    let (share_arrived, share_release) =
        live_sse.add_gated("continue-after-share", &sse_chat("used-shared-answer"));
    share_release.notify_one();
    let _: Value = client
        .call_ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "user_input",
                    "delivery": "steer",
                    "content": [{ "kind": "text", "text": "continue-after-share" }]
                }
            }),
        )
        .await;
    wait_signal(&share_arrived, "after share arrival").await;
    client.wait_turn_completed(&thread_id).await;
    let after_share_reqs = live_sse.take_requests();
    let after_share_body = &after_share_reqs[0];
    assert!(after_share_body.contains("Side conversation result (shared by the user):"));
    assert!(after_share_body.contains("concurrent-side-answer"));

    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": thread_id }))
        .await;
}

async fn verify_stop_during_active_main_turn(client: &mut TestClient, home: &Path, work: &Path) {
    let live_sse = LiveSseServer::start().await;
    write_providers(home, &live_sse.uri, &live_sse.uri);
    let _: Value = client.call_ok("providers/reload", json!({})).await;

    let (main_arrived, main_release) = live_sse.add_gated(
        "main-turn-during-stop",
        &sse_chat("main-uninterrupted-by-side-stop"),
    );
    let (side_arrived, _side_never_released) =
        live_sse.add_gated("slow-side-question", &sse_chat("never-arrives"));

    let opened: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
    let thread_id = opened.thread_id;
    client.drain_notices();

    let _: Value = client
        .call_ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "user_input",
                    "delivery": "steer",
                    "content": [{ "kind": "text", "text": "main-turn-during-stop" }]
                }
            }),
        )
        .await;
    wait_signal(&main_arrived, "main turn arrival").await;

    let _: BtwView = client
        .call_ok(
            "btw/ask",
            json!({
                "thread_id": thread_id,
                "content": [{ "kind": "text", "text": "slow-side-question" }]
            }),
        )
        .await;
    wait_signal(&side_arrived, "slow side arrival").await;

    let started = Instant::now();
    let stopped: Option<BtwView> = client
        .call_ok("btw/stop", json!({ "thread_id": thread_id }))
        .await;
    assert!(started.elapsed() < Duration::from_secs(5));
    let stopped = stopped.unwrap();
    assert!(!stopped.busy);
    assert!(stopped.messages.last().unwrap().interrupted);
    assert!(!stopped.messages.last().unwrap().complete);

    main_release.notify_one();
    let events = client.wait_turn_completed(&thread_id).await;
    assert!(events.iter().any(
        |e| matches!(e, EventKind::TurnCompleted { turn } if turn.status == TurnStatus::Completed)
    ));

    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": thread_id }))
        .await;
}

async fn verify_close_snapshot_and_fresh_reopen(
    client: &mut TestClient,
    work: &Path,
    main_mock: &MockServer,
) {
    main_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(SequenceResponder::new(vec![
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("first-side-answer"), "text/event-stream"),
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("main-between-sides"), "text/event-stream"),
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("fresh-side-answer"), "text/event-stream"),
        ]))
        .mount(main_mock)
        .await;

    let opened: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
    let thread_id = opened.thread_id;
    client.drain_notices();

    let first_view: BtwView = client
        .call_ok(
            "btw/ask",
            json!({
                "thread_id": thread_id,
                "content": [{ "kind": "text", "text": "old-side-question" }]
            }),
        )
        .await;
    let first_id = first_view.id;
    wait_btw_idle(client, &thread_id).await;

    let _: Value = client
        .call_ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": { "kind": "run_command", "name": "btw", "arguments": "model" }
            }),
        )
        .await;
    client
        .wait_matching("btw models event", |msg| {
            let ev: tiechui_protocol::app::SessionEvent =
                serde_json::from_value(msg.get("params")?.clone()).ok()?;
            matches!(
                ev.event.event,
                EventKind::Extension { ref name, ref data }
                    if name == BTW_EVENT
                        && matches!(serde_json::from_value::<BtwEvent>(data.clone()), Ok(BtwEvent::Models { .. }))
            )
            .then_some(())
        })
        .await;

    let snap_open: Snapshot = client
        .call_ok("session/snapshot", json!({ "thread_id": thread_id }))
        .await;
    let snap_btw = snap_open.events.iter().find_map(|ev| match &ev.event {
        EventKind::Extension { name, data } if name == BTW_EVENT => {
            serde_json::from_value::<BtwEvent>(data.clone()).ok()
        }
        _ => None,
    });
    assert!(matches!(snap_btw, Some(BtwEvent::Update { ref view, .. }) if view.id == first_id));

    let closed_res: Value = client
        .call_ok("btw/close", json!({ "thread_id": thread_id }))
        .await;
    assert_eq!(closed_res, Value::Null);
    wait_btw_closed(client, &thread_id).await;

    let after_close: Option<BtwView> = client
        .call_ok(
            "btw/open",
            json!({ "thread_id": thread_id, "create": false }),
        )
        .await;
    assert_eq!(after_close, None);

    let snap_closed: Snapshot = client
        .call_ok("session/snapshot", json!({ "thread_id": thread_id }))
        .await;
    assert!(
        !snap_closed
            .events
            .iter()
            .any(|ev| matches!(&ev.event, EventKind::Extension { name, .. } if name == BTW_EVENT))
    );

    let _: Value = client
        .call_ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "user_input",
                    "delivery": "steer",
                    "content": [{ "kind": "text", "text": "main-question-after-close" }]
                }
            }),
        )
        .await;
    client.wait_turn_completed(&thread_id).await;

    let reopened: BtwView = client
        .call_ok(
            "btw/ask",
            json!({
                "thread_id": thread_id,
                "content": [{ "kind": "text", "text": "fresh-side-question" }]
            }),
        )
        .await;
    assert_ne!(reopened.id, first_id);
    let reopened_done = wait_btw_idle(client, &thread_id).await;
    assert_eq!(reopened_done.messages.len(), 2);

    let reqs = main_mock.received_requests().await.unwrap();
    let fresh_body = String::from_utf8_lossy(&reqs[2].body);
    assert!(!fresh_body.contains("old-side-question"));
    assert!(!fresh_body.contains("first-side-answer"));
    assert!(fresh_body.contains("main-question-after-close"));

    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": thread_id }))
        .await;
}

async fn verify_model_switching(
    client: &mut TestClient,
    work: &Path,
    main_mock: &MockServer,
    side_mock: &MockServer,
) {
    main_mock.reset().await;
    side_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("main-stays-alpha"), "text/event-stream"),
        )
        .expect(1)
        .mount(main_mock)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("side-uses-beta"), "text/event-stream"),
        )
        .expect(1)
        .mount(side_mock)
        .await;

    let opened: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
    let thread_id = opened.thread_id;
    client.drain_notices();

    assert!(
        client
            .call(
                "btw/model",
                json!({ "thread_id": thread_id, "model": "unknown/model" })
            )
            .await
            .is_err()
    );

    let switched_slash: BtwView = client
        .call_ok(
            "btw/set_model",
            json!({
                "thread_id": thread_id,
                "model": "beta/router/v1",
                "thinking": "max"
            }),
        )
        .await;
    assert_eq!(switched_slash.model.provider, "beta");
    assert_eq!(switched_slash.model.id, "router/v1");
    assert_eq!(switched_slash.thinking, ThinkingLevel::Off);

    let switched_fast: BtwView = client
        .call_ok(
            "btw/model",
            json!({
                "thread_id": thread_id,
                "model": "beta/fast"
            }),
        )
        .await;
    assert_eq!(switched_fast.model.provider, "beta");
    assert_eq!(switched_fast.model.id, "fast");
    assert_eq!(switched_fast.thinking, ThinkingLevel::High);

    client.drain_notices();
    let _: BtwView = client
        .call_ok(
            "btw/ask",
            json!({
                "thread_id": thread_id,
                "content": [{ "kind": "text", "text": "side-on-beta" }]
            }),
        )
        .await;
    let done = wait_btw_idle(client, &thread_id).await;
    assert_eq!(done.messages.last().unwrap().text, "side-uses-beta");

    let _: Value = client
        .call_ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "user_input",
                    "delivery": "steer",
                    "content": [{ "kind": "text", "text": "main-still-on-alpha" }]
                }
            }),
        )
        .await;
    client.wait_turn_completed(&thread_id).await;

    let side_reqs = side_mock.received_requests().await.unwrap();
    assert_eq!(side_reqs.len(), 1);
    let side_json: Value = serde_json::from_slice(&side_reqs[0].body).unwrap();
    assert_eq!(side_json["model"], "fast");
    assert_eq!(side_json["reasoning_effort"], "high");

    let main_reqs = main_mock.received_requests().await.unwrap();
    assert_eq!(main_reqs.len(), 1);
    let main_json: Value = serde_json::from_slice(&main_reqs[0].body).unwrap();
    assert_eq!(main_json["model"], "main");

    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": thread_id }))
        .await;
}

async fn verify_side_tool_approvals(client: &mut TestClient, work: &Path, main_mock: &MockServer) {
    main_mock.reset().await;
    let allowed_path = work.join("side-allowed.txt");
    let denied_path = work.join("side-denied.txt");
    Mock::given(method("POST"))
        .respond_with(SequenceResponder::new(vec![
            ResponseTemplate::new(200).set_body_raw(sse_chat("main-ready"), "text/event-stream"),
            ResponseTemplate::new(200).set_body_raw(
                sse_tool_call(
                    "call_w1",
                    "write",
                    &json!({
                        "path": allowed_path.display().to_string(),
                        "content": "approved-from-side\n"
                    }),
                ),
                "text/event-stream",
            ),
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("side-write-approved"), "text/event-stream"),
            ResponseTemplate::new(200).set_body_raw(
                sse_tool_call(
                    "call_w2",
                    "write",
                    &json!({
                        "path": denied_path.display().to_string(),
                        "content": "must-not-exist\n"
                    }),
                ),
                "text/event-stream",
            ),
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("side-write-denied"), "text/event-stream"),
        ]))
        .mount(main_mock)
        .await;

    let opened: Snapshot = client
        .call_ok("session/open", json!({ "cwd": work, "mode": "rules" }))
        .await;
    let thread_id = opened.thread_id;
    let _: Value = client
        .call_ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "user_input",
                    "delivery": "steer",
                    "content": [{ "kind": "text", "text": "seed main session" }]
                }
            }),
        )
        .await;
    client.wait_turn_completed(&thread_id).await;
    client.drain_notices();

    let view: BtwView = client
        .call_ok(
            "btw/ask",
            json!({
                "thread_id": thread_id,
                "content": [{ "kind": "text", "text": "please write side-allowed.txt" }]
            }),
        )
        .await;
    let side_turn = TurnId(view.id.clone());

    let (approval_1, turn_1) = wait_side_approval(client, &thread_id).await;
    assert_eq!(turn_1, Some(side_turn.clone()));

    let listed: Vec<SessionSummary> = client.call_ok("session/list", json!({ "cwd": work })).await;
    let summary = listed.iter().find(|s| s.thread_id == thread_id).unwrap();
    assert_eq!(summary.live, Some(LiveState::Idle));

    let daemon: DaemonStatus = client.call_ok("daemon/status", json!({})).await;
    assert_eq!(daemon.busy_sessions, 1);

    let snap: Snapshot = client
        .call_ok("session/snapshot", json!({ "thread_id": thread_id }))
        .await;
    assert!(snap.events.iter().any(|ev| matches!(
        &ev.event,
        EventKind::ApprovalRequested { turn_id, approval_id, .. }
            if turn_id == &Some(side_turn.clone()) && approval_id == &approval_1
    )));

    let _: Value = client
        .call_ok(
            "turn/approve",
            json!({
                "thread_id": thread_id,
                "approval_id": approval_1,
                "decision": { "kind": "allow_once" }
            }),
        )
        .await;

    let done_1 = wait_btw_idle(client, &thread_id).await;
    assert!(
        done_1
            .messages
            .last()
            .unwrap()
            .text
            .contains("side-write-approved")
    );
    assert_eq!(
        std::fs::read_to_string(&allowed_path).unwrap(),
        "approved-from-side\n"
    );

    client.drain_notices();
    let _: BtwView = client
        .call_ok(
            "btw/ask",
            json!({
                "thread_id": thread_id,
                "content": [{ "kind": "text", "text": "please write side-denied.txt" }]
            }),
        )
        .await;
    let (approval_2, turn_2) = wait_side_approval(client, &thread_id).await;
    assert_eq!(turn_2, Some(side_turn));

    let _: Value = client
        .call_ok(
            "turn/approve",
            json!({
                "thread_id": thread_id,
                "approval_id": approval_2,
                "decision": { "kind": "deny", "reason": "not allowed in side chat" }
            }),
        )
        .await;

    let done_2 = wait_btw_idle(client, &thread_id).await;
    assert!(
        done_2
            .messages
            .last()
            .unwrap()
            .text
            .contains("side-write-denied")
    );
    assert!(!denied_path.exists());

    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": thread_id }))
        .await;
}

async fn wait_side_approval(
    client: &mut TestClient,
    thread_id: &ThreadId,
) -> (ApprovalId, Option<TurnId>) {
    client
        .wait_matching("side approval_requested", |msg| {
            if msg.get("method") != Some(&json!("session/event")) {
                return None;
            }
            let ev: tiechui_protocol::app::SessionEvent =
                serde_json::from_value(msg["params"].clone()).ok()?;
            if &ev.event.thread_id != thread_id {
                return None;
            }
            match ev.event.event {
                EventKind::ApprovalRequested {
                    turn_id,
                    approval_id,
                    request: ApprovalRequest::Permission { tool, .. },
                } if tool == "write" => Some((approval_id, turn_id)),
                _ => None,
            }
        })
        .await
}

async fn verify_image_input_deferred_reload_and_disabled_module(
    client: &mut TestClient,
    home: &Path,
    work: &Path,
) {
    let live_sse = LiveSseServer::start().await;
    write_providers(home, &live_sse.uri, &live_sse.uri);
    let _: Value = client.call_ok("providers/reload", json!({})).await;

    let (arrived, release) =
        live_sse.add_gated("image-side-question", &sse_chat("image-side-answer"));
    let opened: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
    let thread_id = opened.thread_id;
    client.drain_notices();

    let view: BtwView = client
        .call_ok(
            "btw/ask",
            json!({
                "thread_id": thread_id,
                "content": [
                    { "kind": "text", "text": "image-side-question" },
                    { "kind": "image", "mime_type": "image/png", "data": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==" }
                ]
            }),
        )
        .await;
    assert_eq!(view.messages[0].images.len(), 1);
    wait_signal(&arrived, "image side arrival").await;

    let _: Value = client
        .call_ok(
            "config/update",
            json!({ "key": "network.no_proxy", "value": "localhost" }),
        )
        .await;
    let mid_snap: Snapshot = client
        .call_ok("session/snapshot", json!({ "thread_id": thread_id }))
        .await;
    assert_eq!(mid_snap.events[0].epoch.0, 0);

    release.notify_one();
    client.wait_reloaded(&thread_id).await;
    let after_snap: Snapshot = client
        .call_ok("session/snapshot", json!({ "thread_id": thread_id }))
        .await;
    assert_eq!(after_snap.events[0].epoch.0, 1);

    let reqs = live_sse.take_requests();
    assert!(reqs[0].contains("image_url"));

    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": thread_id }))
        .await;

    std::fs::write(
        home.join("config.toml"),
        "model = \"alpha/main\"\n[modules]\nbtw = false\n",
    )
    .unwrap();
    let disabled_open: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
    let disabled_err = client
        .call(
            "btw/open",
            json!({ "thread_id": disabled_open.thread_id, "create": true }),
        )
        .await
        .unwrap_err();
    assert_eq!(disabled_err["code"], -32602, "{disabled_err}");
    let _: Value = client
        .call_ok(
            "session/close",
            json!({ "thread_id": disabled_open.thread_id }),
        )
        .await;
}
