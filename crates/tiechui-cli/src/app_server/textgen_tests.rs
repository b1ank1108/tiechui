#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tiechui_model::{Content, Message, Role};
use tiechui_protocol::app::{SessionSummary, Snapshot};
use tiechui_protocol::workspace::CommitMessageResult;
use tiechui_protocol::{
    Event, EventKind, InputContent, Item, ItemId, KnownItem, SessionEpoch, Thread, ThreadId, Turn,
    TurnId, TurnStatus,
};
use tiechui_session::{EntryKind, SessionStore};
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::projection::Projection;
use super::tests::{TestClient, isolated_home_child, sse_chat};
use super::{Live, Server, lock, watch_session_title};

struct SignalResponder {
    arrived: Arc<Notify>,
    template: ResponseTemplate,
}

impl wiremock::Respond for SignalResponder {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        self.arrived.notify_one();
        self.template.clone()
    }
}

async fn wait_arrived(arrived: &Notify) {
    tokio::time::timeout(Duration::from_secs(10), arrived.notified())
        .await
        .expect("timed out waiting for mock request arrival");
}

fn write_test_providers(home: &Path, main_uri: &str, light_uri: &str) {
    std::fs::write(
        home.join("providers.toml"),
        format!(
            "[alpha]\nwire = \"openai-completions\"\nbase_url = \"{main_uri}\"\napi_key = \"sk-alpha\"\nmodels = [{{ id = \"main\", context_window = 128000, max_output = 4096 }}]\n\n\
             [beta]\nwire = \"openai-completions\"\nbase_url = \"{light_uri}\"\napi_key = \"sk-beta\"\n\
             [[beta.models]]\nid = \"fast\"\ncontext_window = 128000\nmax_output = 4096\nreasoning = true\n\
             [beta.models.thinking_levels]\noff = \"none\"\nlow = \"low\"\nhigh = \"high\"\n"
        ),
    )
    .unwrap();
}

async fn single_lightweight_prompt(mock: &MockServer) -> (Value, String) {
    let reqs = mock.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["model"], "fast");
    assert_eq!(body["reasoning_effort"], "none");
    let prompt = body["messages"][1]["content"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned();
    (body, prompt)
}

async fn submit_text(client: &mut TestClient, thread_id: &ThreadId, text: &str) -> Vec<EventKind> {
    let _: Value = client
        .call_ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "user_input",
                    "delivery": "steer",
                    "content": [{ "kind": "text", "text": text }]
                }
            }),
        )
        .await;
    client.wait_turn_completed(thread_id).await
}

fn append_user_message(store: &mut SessionStore, text: &str, cancel: &CancellationToken) {
    store
        .append(
            EntryKind::Message {
                message: Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: text.into(),
                        opaque: None,
                    }],
                    opaque: None,
                },
                usage: None,
                model: None,
                stop_reason: None,
                reasoning: None,
                details: None,
            },
            cancel,
        )
        .unwrap();
}

fn thread_started_event(thread_id: &str) -> Event {
    Event {
        submission_id: None,
        thread_id: ThreadId(thread_id.into()),
        epoch: SessionEpoch(0),
        event: EventKind::ThreadStarted {
            thread: Thread {
                id: ThreadId(thread_id.into()),
                epoch: SessionEpoch(0),
                turns: vec![],
            },
            replay: false,
        },
    }
}

fn completed_turn_event(thread_id: &str, items: Vec<Item>) -> Event {
    Event {
        submission_id: None,
        thread_id: ThreadId(thread_id.into()),
        epoch: SessionEpoch(0),
        event: EventKind::TurnCompleted {
            turn: Turn {
                id: TurnId("t-1".into()),
                status: TurnStatus::Completed,
                items,
                started_at: 1,
                ended_at: Some(2),
            },
        },
    }
}

fn user_item(id: &str, text: &str) -> Item {
    Item::Known(KnownItem::UserMessage {
        id: ItemId(id.into()),
        content: vec![InputContent::Text { text: text.into() }],
    })
}

fn assistant_item(id: &str, text: &str) -> Item {
    Item::Known(KnownItem::AssistantMessage {
        id: ItemId(id.into()),
        text: text.into(),
    })
}

#[tokio::test]
async fn session_title_generation_lifecycle_and_guards() {
    let Some(home) = isolated_home_child(
        "TIECHUI_TEST_TITLE_GEN_HOME",
        "app_server::textgen_tests::session_title_generation_lifecycle_and_guards",
    ) else {
        return;
    };
    run_title_generation_cases(home).await;
}

async fn run_title_generation_cases(home: PathBuf) {
    let work = home.parent().unwrap().join("work");
    std::fs::create_dir_all(&work).unwrap();

    let main_mock = MockServer::start().await;
    let long_assistant = "这是助手针对鉴权超时的详细排查结论🔍".repeat(120);
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_chat(&long_assistant), "text/event-stream"),
        )
        .mount(&main_mock)
        .await;

    let light_mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("  \"修复鉴权超时\" \n多余行"), "text/event-stream"),
        )
        .mount(&light_mock)
        .await;

    std::fs::write(
        home.join("config.toml"),
        "model = \"alpha/main\"\nlightweight_model = \"beta/fast\"\n",
    )
    .unwrap();
    write_test_providers(&home, &main_mock.uri(), &light_mock.uri());

    let mut client = TestClient::new(Server::new(home.clone()));

    verify_first_turn_title(&mut client, &home, &work, &light_mock).await;
    let untitled_history_path =
        verify_unset_and_resume_guards(&mut client, &work, &light_mock).await;
    verify_fork_guard(&mut client, &home, &work, &light_mock).await;
    verify_rename_guards(&mut client, &work, &light_mock).await;
    verify_silent_failure(&mut client, &work, &light_mock).await;

    light_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_chat("绝不回填标题"), "text/event-stream"),
        )
        .mount(&light_mock)
        .await;
    let resumed: Snapshot = client
        .call_ok("session/open", json!({ "path": untitled_history_path }))
        .await;
    client.drain_notices();
    submit_text(&mut client, &resumed.thread_id, "继续第二轮").await;
    client.drain_title_tasks().await;
    assert_eq!(light_mock.received_requests().await.unwrap().len(), 0);
    assert_eq!(
        tiechui_session::meta(Path::new(&untitled_history_path))
            .unwrap()
            .title,
        None
    );

    client.close().await;
}

async fn verify_first_turn_title(
    client: &mut TestClient,
    home: &Path,
    work: &Path,
    light_mock: &MockServer,
) {
    let opened: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
    client.drain_notices();

    let long_user = "排查生产环境鉴权接口超时问题🚀".repeat(150);
    submit_text(client, &opened.thread_id, &long_user).await;
    client.wait_sessions_changed().await;
    client.drain_title_tasks().await;

    let (body, prompt) = single_lightweight_prompt(light_mock).await;
    assert!(body.get("prompt_cache_key").is_none());
    assert!(prompt.len() < 4_000, "prompt too large: {}", prompt.len());
    assert!(prompt.contains("排查生产环境鉴权接口超时问题🚀"));
    assert!(prompt.contains("这是助手针对鉴权超时的详细排查结论🔍"));

    let listed: Vec<SessionSummary> = client.call_ok("session/list", json!({ "cwd": work })).await;
    let item = listed.iter().find(|s| s.path == opened.path).unwrap();
    assert_eq!(item.title.as_deref(), Some("修复鉴权超时"));

    let (tui_list, _) =
        tiechui_session::summaries(home, Some(work), &CancellationToken::new()).unwrap();
    let tui_item = tui_list
        .iter()
        .find(|s| s.path == Path::new(&opened.path))
        .unwrap();
    assert_eq!(tui_item.meta.title.as_deref(), Some("修复鉴权超时"));

    submit_text(client, &opened.thread_id, "第二轮追问").await;
    client.drain_title_tasks().await;
    assert_eq!(light_mock.received_requests().await.unwrap().len(), 1);
    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": opened.thread_id }))
        .await;
}

async fn verify_unset_and_resume_guards(
    client: &mut TestClient,
    work: &Path,
    light_mock: &MockServer,
) -> String {
    light_mock.reset().await;
    let _: Value = client
        .call_ok(
            "config/update",
            json!({ "key": "lightweight_model", "value": null }),
        )
        .await;

    let opened: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
    client.drain_notices();
    submit_text(client, &opened.thread_id, "未配置轻量模型首条消息").await;
    client.drain_title_tasks().await;
    assert_eq!(light_mock.received_requests().await.unwrap().len(), 0);
    assert_eq!(
        tiechui_session::meta(Path::new(&opened.path))
            .unwrap()
            .title,
        None
    );

    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": opened.thread_id }))
        .await;

    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("热更新后生成标题"), "text/event-stream"),
        )
        .expect(1)
        .mount(light_mock)
        .await;
    let pre_opened: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
    client.drain_notices();
    let _: Value = client
        .call_ok(
            "config/update",
            json!({ "key": "lightweight_model", "value": "beta/fast" }),
        )
        .await;
    client.wait_reloaded(&pre_opened.thread_id).await;
    client.drain_notices();
    submit_text(client, &pre_opened.thread_id, "配置轻量模型后发送首轮").await;
    client.wait_sessions_changed().await;
    client.drain_title_tasks().await;
    assert_eq!(
        tiechui_session::meta(Path::new(&pre_opened.path))
            .unwrap()
            .title
            .as_deref(),
        Some("热更新后生成标题")
    );
    let _: Value = client
        .call_ok(
            "session/close",
            json!({ "thread_id": pre_opened.thread_id }),
        )
        .await;

    opened.path
}

async fn verify_fork_guard(
    client: &mut TestClient,
    home: &Path,
    work: &Path,
    light_mock: &MockServer,
) {
    light_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("分叉不应生成标题"), "text/event-stream"),
        )
        .mount(light_mock)
        .await;

    let cancel = CancellationToken::new();
    let root_store = SessionStore::create(home, work, &cancel).unwrap();
    let forked_path = {
        let forked = root_store.fork(&root_store.header().id, &cancel).unwrap();
        assert!(forked.header().parent_session.is_some());
        forked.path().to_owned()
    };
    drop(root_store);

    let opened: Snapshot = client
        .call_ok("session/open", json!({ "path": forked_path }))
        .await;
    client.drain_notices();
    submit_text(client, &opened.thread_id, "在分叉会话发送首轮").await;
    client.drain_title_tasks().await;
    assert_eq!(light_mock.received_requests().await.unwrap().len(), 0);
    assert_eq!(tiechui_session::meta(&forked_path).unwrap().title, None);
    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": opened.thread_id }))
        .await;
}

async fn verify_rename_guards(client: &mut TestClient, work: &Path, light_mock: &MockServer) {
    light_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_chat("不应覆盖"), "text/event-stream"),
        )
        .mount(light_mock)
        .await;

    let early: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
    let _: Value = client
        .call_ok(
            "session/rename",
            json!({ "path": early.path, "title": "手动早期标题" }),
        )
        .await;
    client.drain_notices();
    submit_text(client, &early.thread_id, "测试早期改名").await;
    client.drain_title_tasks().await;
    assert_eq!(light_mock.received_requests().await.unwrap().len(), 0);
    assert_eq!(
        tiechui_session::meta(Path::new(&early.path))
            .unwrap()
            .title
            .as_deref(),
        Some("手动早期标题")
    );

    light_mock.reset().await;
    let arrived = Arc::new(Notify::new());
    Mock::given(method("POST"))
        .respond_with(SignalResponder {
            arrived: arrived.clone(),
            template: ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(400))
                .set_body_raw(sse_chat("AI晚到标题"), "text/event-stream"),
        })
        .expect(1)
        .mount(light_mock)
        .await;

    let mid: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
    client.drain_notices();
    submit_text(client, &mid.thread_id, "测试中途改名").await;
    wait_arrived(&arrived).await;
    let _: Value = client
        .call_ok(
            "session/rename",
            json!({ "path": mid.path, "title": "手动中途改名" }),
        )
        .await;
    client.drain_title_tasks().await;
    assert_eq!(light_mock.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        tiechui_session::meta(Path::new(&mid.path))
            .unwrap()
            .title
            .as_deref(),
        Some("手动中途改名")
    );
    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": early.thread_id }))
        .await;
    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": mid.thread_id }))
        .await;
}

async fn verify_silent_failure(client: &mut TestClient, work: &Path, light_mock: &MockServer) {
    light_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream down"))
        .expect(1)
        .mount(light_mock)
        .await;

    let failing: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
    client.drain_notices();
    let events = submit_text(client, &failing.thread_id, "测试失败静默").await;
    client.drain_title_tasks().await;
    assert_eq!(light_mock.received_requests().await.unwrap().len(), 1);
    assert!(!events.iter().any(|e| matches!(e, EventKind::Error { .. })));
    assert_eq!(
        tiechui_session::meta(Path::new(&failing.path))
            .unwrap()
            .title,
        None
    );
    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": failing.thread_id }))
        .await;
}

#[tokio::test]
async fn first_turn_title_ignores_mid_turn_steers_and_queued_followup_entries_in_server_and_tui() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work).unwrap();

    let light_mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("修复鉴权并发竞态"), "text/event-stream"),
        )
        .expect(1)
        .mount(&light_mock)
        .await;

    write_test_providers(&home, "http://127.0.0.1:9", &light_mock.uri());
    std::fs::write(
        home.join("config.toml"),
        "model = \"alpha/main\"\nlightweight_model = \"beta/fast\"\n",
    )
    .unwrap();

    let cancel = CancellationToken::new();
    let tui_path = {
        let mut store = SessionStore::create(&home, &work, &cancel).unwrap();
        append_user_message(&mut store, "首轮初始提问：排查鉴权竞态", &cancel);
        append_user_message(&mut store, "中途 steer 补充说明", &cancel);
        append_user_message(&mut store, "第二轮 follow-up 提前落盘消息", &cancel);
        store.path().to_owned()
    };

    let (events_tx, events_rx) = broadcast::channel(16);
    let tui_handle = watch_session_title(home.clone(), tui_path.clone(), events_rx);
    events_tx.send(thread_started_event("tui-t1")).unwrap();
    events_tx
        .send(completed_turn_event(
            "tui-t1",
            vec![
                user_item("u1", "首轮初始提问：排查鉴权竞态"),
                assistant_item("a1", "先读取会话日志"),
                user_item("u2", "中途 steer 补充说明"),
                assistant_item("a2", "已修复首轮提取逻辑"),
            ],
        ))
        .unwrap();
    tui_handle.await.unwrap();

    let (_, tui_prompt) = single_lightweight_prompt(&light_mock).await;
    assert!(tui_prompt.contains("首轮初始提问：排查鉴权竞态"));
    assert!(tui_prompt.contains("先读取会话日志\n\n已修复首轮提取逻辑"));
    assert!(!tui_prompt.contains("中途 steer 补充说明"));
    assert!(!tui_prompt.contains("第二轮 follow-up 提前落盘消息"));
    assert_eq!(
        tiechui_session::meta(&tui_path).unwrap().title.as_deref(),
        Some("修复鉴权并发竞态")
    );

    verify_server_multi_user_turn_and_unset_tui(&home, &work, &light_mock).await;
}

async fn verify_server_multi_user_turn_and_unset_tui(
    home: &Path,
    work: &Path,
    light_mock: &MockServer,
) {
    light_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("服务端首轮标题"), "text/event-stream"),
        )
        .expect(1)
        .mount(light_mock)
        .await;

    let cancel = CancellationToken::new();
    let app_path = {
        let mut store = SessionStore::create(home, work, &cancel).unwrap();
        append_user_message(&mut store, "桌面端首轮提问", &cancel);
        append_user_message(&mut store, "BeforeStop 钩子续跑提示", &cancel);
        append_user_message(&mut store, "排队中的第二轮消息", &cancel);
        store.path().to_owned()
    };

    let server = Server::new(home.to_owned());
    let mut client = TestClient::new(server.clone());
    let projection = Arc::new(Mutex::new(Projection::default()));
    let (submissions, _rx) = mpsc::channel(1);
    let (ready_tx, started) = watch::channel(None);
    let thread = ThreadId("app-t1".into());
    lock(&server.sessions).insert(
        thread.clone(),
        Arc::new(Live {
            path: app_path.clone(),
            cwd: work.to_owned(),
            submissions,
            shutdown: CancellationToken::new(),
            projection: projection.clone(),
            started,
            pump: Mutex::new(None),
            stale: AtomicBool::new(false),
        }),
    );

    server.forward_event(
        &thread,
        &projection,
        &ready_tx,
        thread_started_event("app-t1"),
    );
    server.forward_event(
        &thread,
        &projection,
        &ready_tx,
        completed_turn_event(
            "app-t1",
            vec![
                user_item("u1", "桌面端首轮提问"),
                user_item("u2", "BeforeStop 钩子续跑提示"),
                assistant_item("a1", "桌面端首轮回答"),
            ],
        ),
    );
    client.wait_sessions_changed().await;
    client.drain_title_tasks().await;
    let (_, app_prompt) = single_lightweight_prompt(light_mock).await;
    assert!(app_prompt.contains("桌面端首轮提问"));
    assert!(!app_prompt.contains("BeforeStop 钩子续跑提示"));
    assert!(!app_prompt.contains("排队中的第二轮消息"));
    assert_eq!(
        tiechui_session::meta(&app_path).unwrap().title.as_deref(),
        Some("服务端首轮标题")
    );
    client.close().await;

    light_mock.reset().await;
    std::fs::write(home.join("config.toml"), "model = \"alpha/main\"\n").unwrap();
    let unset_path = SessionStore::create(home, work, &cancel)
        .unwrap()
        .path()
        .to_owned();
    let (unset_tx, unset_rx) = broadcast::channel(8);
    let unset_handle = watch_session_title(home.to_owned(), unset_path.clone(), unset_rx);
    unset_tx.send(thread_started_event("tui-unset")).unwrap();
    unset_tx
        .send(completed_turn_event(
            "tui-unset",
            vec![
                user_item("u1", "未配置轻量模型"),
                assistant_item("a1", "正常回复"),
            ],
        ))
        .unwrap();
    unset_handle.await.unwrap();
    assert!(light_mock.received_requests().await.unwrap().is_empty());
    assert_eq!(tiechui_session::meta(&unset_path).unwrap().title, None);
}

fn run_git(root: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(["-c", "user.name=T", "-c", "user.email=t@e.co"])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed");
}

#[tokio::test]
async fn git_commit_message_rpc_handles_diffs_errors_and_cancellation() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let non_git = dir.path().join("plain");
    std::fs::create_dir_all(&non_git).unwrap();

    let light_mock = MockServer::start().await;
    write_test_providers(&home, "http://127.0.0.1:9", &light_mock.uri());
    std::fs::write(home.join("config.toml"), "model = \"alpha/main\"\n").unwrap();

    let mut client = TestClient::new(Server::new(home.clone()));

    run_git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();
    let unset_err = client
        .call("git/commit_message", json!({ "root": repo }))
        .await
        .unwrap_err();
    assert!(
        unset_err["message"]
            .as_str()
            .unwrap_or_default()
            .contains("lightweight_model"),
        "{unset_err}"
    );
    assert_eq!(light_mock.received_requests().await.unwrap().len(), 0);

    let _: Value = client
        .call_ok(
            "config/update",
            json!({ "key": "lightweight_model", "value": "beta/fast" }),
        )
        .await;

    assert!(
        client
            .call("git/commit_message", json!({ "root": non_git }))
            .await
            .is_err()
    );
    run_git(&repo, &["add", "a.txt"]);
    run_git(&repo, &["commit", "-q", "-m", "initial"]);
    let clean_err = client
        .call("git/commit_message", json!({ "root": repo }))
        .await
        .unwrap_err();
    assert!(
        clean_err["message"]
            .as_str()
            .unwrap_or_default()
            .contains("No uncommitted changes"),
        "{clean_err}"
    );

    verify_commit_message_diffs(&mut client, &repo, &light_mock).await;
    verify_commit_message_cancellation(&mut client, &repo, &light_mock).await;

    let _: Value = client
        .call_ok(
            "providers/delete_model",
            json!({ "provider": "beta", "id": "fast" }),
        )
        .await;
    let undeclared = client
        .call("git/commit_message", json!({ "root": repo }))
        .await
        .unwrap_err();
    assert!(
        undeclared["message"]
            .as_str()
            .unwrap_or_default()
            .contains("beta/fast"),
        "{undeclared}"
    );

    client.close().await;
}

async fn verify_commit_message_cancellation(
    client: &mut TestClient,
    repo: &Path,
    light_mock: &MockServer,
) {
    light_mock.reset().await;
    let arrived = Arc::new(Notify::new());
    Mock::given(method("POST"))
        .respond_with(SignalResponder {
            arrived: arrived.clone(),
            template: ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_raw(sse_chat("too late"), "text/event-stream"),
        })
        .mount(light_mock)
        .await;

    let req_id = client.send_async(
        "git/commit_message",
        json!({ "root": repo, "request_id": "cancel-token-1" }),
    );
    wait_arrived(&arrived).await;
    let started = Instant::now();
    let cancel_res: Value = client
        .call_ok(
            "git/cancel_commit_message",
            json!({ "request_id": "cancel-token-1" }),
        )
        .await;
    assert_eq!(cancel_res, Value::Null);
    let cancelled = client.wait_reply(req_id).await.unwrap_err();
    assert_eq!(cancelled["code"], -32800, "{cancelled}");
    assert!(started.elapsed() < Duration::from_secs(5));

    let rpc_cancel_id = client.send_async("git/commit_message", json!({ "root": repo }));
    wait_arrived(&arrived).await;
    let rpc_started = Instant::now();
    client.send_notification("$/cancelRequest", json!({ "id": rpc_cancel_id }));
    let rpc_cancelled = client.wait_reply(rpc_cancel_id).await.unwrap_err();
    assert_eq!(rpc_cancelled["code"], -32800, "{rpc_cancelled}");
    assert!(rpc_started.elapsed() < Duration::from_secs(5));

    let immediate_started = Instant::now();
    let immediate_id = client.send_async("git/commit_message", json!({ "root": repo }));
    client.send_notification("$/cancelRequest", json!({ "id": immediate_id }));
    let immediate_cancelled = client.wait_reply(immediate_id).await.unwrap_err();
    assert_eq!(immediate_cancelled["code"], -32800, "{immediate_cancelled}");
    assert!(immediate_started.elapsed() < Duration::from_secs(5));
}

async fn verify_commit_message_diffs(
    client: &mut TestClient,
    repo: &Path,
    light_mock: &MockServer,
) {
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            sse_chat("```\nfeat(notes): update tracked and untracked files\n```"),
            "text/event-stream",
        ))
        .expect(1)
        .mount(light_mock)
        .await;

    std::fs::write(repo.join("a.txt"), "unstaged edit\n").unwrap();
    std::fs::write(
        repo.join("big_untracked.txt"),
        "多字节中文改动行🌟\n".repeat(1_800),
    )
    .unwrap();

    let fallback: CommitMessageResult = client
        .call_ok("git/commit_message", json!({ "root": repo }))
        .await;
    assert_eq!(
        fallback.message,
        "feat(notes): update tracked and untracked files"
    );

    let (_, diff_prompt) = single_lightweight_prompt(light_mock).await;
    assert!(diff_prompt.contains("+unstaged edit"));
    assert!(diff_prompt.contains("big_untracked.txt"));
    assert!(diff_prompt.contains("[diff truncated]"));

    let commits =
        tiechui_workspace::log_with_env(repo, 10, &std::collections::BTreeMap::new()).unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].subject, "initial");

    light_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("fix: add staged file only"), "text/event-stream"),
        )
        .expect(1)
        .mount(light_mock)
        .await;

    std::fs::write(repo.join("staged_only.txt"), "staged line\n").unwrap();
    run_git(repo, &["add", "staged_only.txt"]);

    let staged_res: CommitMessageResult = client
        .call_ok("git/commit_message", json!({ "root": repo }))
        .await;
    assert_eq!(staged_res.message, "fix: add staged file only");
    let (_, staged_prompt) = single_lightweight_prompt(light_mock).await;
    assert!(staged_prompt.contains("staged_only.txt"));
    assert!(!staged_prompt.contains("unstaged edit"));
    assert!(!staged_prompt.contains("big_untracked.txt"));
}
