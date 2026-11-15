use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tiechui_protocol::app::{SessionEvent, SessionSummary, Snapshot};
use tiechui_protocol::rpc::Id;
use tiechui_protocol::workspace::CommitMessageResult;
use tiechui_protocol::{EventKind, ThreadId, TurnStatus};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{ClientOut, Server};

struct RpcHarness {
    server: Arc<Server>,
    client_id: u64,
    out: ClientOut,
    lines: mpsc::UnboundedReceiver<String>,
    buffered: Vec<Value>,
    next_id: i64,
}

#[allow(clippy::unwrap_used)]
impl RpcHarness {
    fn new(server: Arc<Server>) -> Self {
        let (client_id, out, lines) = server.add_client();
        Self {
            server,
            client_id,
            out,
            lines,
            buffered: Vec::new(),
            next_id: 0,
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        self.next_id += 1;
        let req_id = self.next_id;
        self.server
            .dispatch(
                self.client_id,
                &self.out,
                Id::Number(req_id),
                method,
                params,
            )
            .await;
        self.wait_reply(req_id).await
    }

    fn send_async(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let req_id = self.next_id;
        let raw = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params
        })
        .to_string();
        self.server
            .receive(self.client_id, &self.out, raw.as_bytes());
        req_id
    }

    fn send_notification(&self, method: &str, params: Value) {
        let raw = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        })
        .to_string();
        self.server
            .receive(self.client_id, &self.out, raw.as_bytes());
    }

    async fn wait_reply(&mut self, req_id: i64) -> Result<Value, Value> {
        loop {
            let raw = self.lines.recv().await.expect("missing rpc reply");
            let msg: Value = serde_json::from_str(&raw).unwrap();
            if msg.get("id") != Some(&json!(req_id)) {
                self.buffered.push(msg);
                continue;
            }
            return match msg.get("error") {
                Some(error) => Err(error.clone()),
                None => Ok(msg["result"].clone()),
            };
        }
    }

    async fn wait_matching<T>(
        &mut self,
        label: &str,
        mut pred: impl FnMut(&Value) -> Option<T>,
    ) -> T {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut idx = 0;
            loop {
                while idx < self.buffered.len() {
                    if let Some(found) = pred(&self.buffered[idx]) {
                        self.buffered.remove(idx);
                        return found;
                    }
                    idx += 1;
                }
                let raw = self.lines.recv().await.expect("expected notification");
                self.buffered.push(serde_json::from_str(&raw).unwrap());
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
    }

    async fn wait_turn_completed(&mut self, thread_id: &ThreadId) -> Vec<EventKind> {
        let mut seen = Vec::new();
        loop {
            let kind = self
                .wait_matching("TurnCompleted", |msg| {
                    if msg.get("method") != Some(&json!("session/event")) {
                        return None;
                    }
                    let ev: SessionEvent = serde_json::from_value(msg["params"].clone()).unwrap();
                    (&ev.event.thread_id == thread_id).then_some(ev.event.event)
                })
                .await;
            let done = matches!(&kind, EventKind::TurnCompleted { turn } if turn.status == TurnStatus::Completed);
            seen.push(kind);
            if done {
                return seen;
            }
        }
    }

    async fn wait_sessions_changed(&mut self) {
        self.wait_matching("sessions/changed", |msg| {
            (msg.get("method") == Some(&json!("sessions/changed"))).then_some(())
        })
        .await;
    }

    async fn wait_reloaded(&mut self, thread_id: &ThreadId) {
        self.wait_matching("session reload", |msg| {
            if msg.get("method") != Some(&json!("session/event")) {
                return None;
            }
            let ev: SessionEvent = serde_json::from_value(msg["params"].clone()).unwrap();
            (&ev.event.thread_id == thread_id
                && ev.event.epoch.0 >= 1
                && matches!(ev.event.event, EventKind::ThreadStarted { .. }))
            .then_some(())
        })
        .await;
    }

    fn drain_notices(&mut self) {
        self.buffered.clear();
        while self.lines.try_recv().is_ok() {}
    }

    async fn close(self) {
        self.server.remove_client(self.client_id);
        self.server.shutdown_all().await;
    }
}

fn sse_chat(content: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices": [{
                "index": 0,
                "delta": { "content": content },
                "finish_reason": "stop"
            }]
        })
    )
}

#[allow(clippy::unwrap_used)]
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

#[allow(clippy::unwrap_used)]
async fn submit_text(client: &mut RpcHarness, thread_id: &ThreadId, text: &str) -> Vec<EventKind> {
    client
        .call(
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
        .await
        .unwrap();
    client.wait_turn_completed(thread_id).await
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn session_title_generation_lifecycle_and_guards() {
    const MARKER: &str = "TIECHUI_TEST_TITLE_GEN_HOME";
    let Some(home_env) = std::env::var_os(MARKER) else {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app_server::textgen_tests::session_title_generation_lifecycle_and_guards",
                "--nocapture",
            ])
            .env(MARKER, &home)
            .env("TIECHUI_HOME", &home)
            .env("HOME", dir.path())
            .output()
            .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };

    run_title_generation_cases(PathBuf::from(home_env)).await;
}

#[allow(clippy::unwrap_used)]
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

    let mut client = RpcHarness::new(Server::new(home.clone()));

    verify_first_turn_title(&mut client, &home, &work, &light_mock).await;
    let untitled_history_path =
        verify_unset_and_resume_guards(&mut client, &work, &light_mock).await;
    verify_rename_guards(&mut client, &work, &light_mock).await;
    verify_silent_failure(&mut client, &work, &light_mock).await;

    light_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_chat("绝不回填标题"), "text/event-stream"),
        )
        .mount(&light_mock)
        .await;
    let resumed: Snapshot = serde_json::from_value(
        client
            .call("session/open", json!({ "path": untitled_history_path }))
            .await
            .unwrap(),
    )
    .unwrap();
    client.drain_notices();
    submit_text(&mut client, &resumed.thread_id, "继续第二轮").await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(light_mock.received_requests().await.unwrap().len(), 0);
    assert_eq!(
        tiechui_session::meta(Path::new(&untitled_history_path))
            .unwrap()
            .title,
        None
    );

    client.close().await;
}

#[allow(clippy::unwrap_used)]
async fn verify_first_turn_title(
    client: &mut RpcHarness,
    home: &Path,
    work: &Path,
    light_mock: &MockServer,
) {
    let opened: Snapshot = serde_json::from_value(
        client
            .call("session/open", json!({ "cwd": work }))
            .await
            .unwrap(),
    )
    .unwrap();
    client.drain_notices();

    let long_user = "排查生产环境鉴权接口超时问题🚀".repeat(150);
    submit_text(client, &opened.thread_id, &long_user).await;
    client.wait_sessions_changed().await;

    let reqs = light_mock.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["model"], "fast");
    assert_eq!(body["reasoning_effort"], "none");
    assert!(body.get("prompt_cache_key").is_none());
    let prompt = body["messages"][1]["content"][0]["text"].as_str().unwrap();
    assert!(prompt.len() < 4_000, "prompt too large: {}", prompt.len());
    assert!(prompt.contains("排查生产环境鉴权接口超时问题🚀"));
    assert!(prompt.contains("这是助手针对鉴权超时的详细排查结论🔍"));

    let listed: Vec<SessionSummary> = serde_json::from_value(
        client
            .call("session/list", json!({ "cwd": work }))
            .await
            .unwrap(),
    )
    .unwrap();
    let item = listed.iter().find(|s| s.path == opened.path).unwrap();
    assert_eq!(item.title.as_deref(), Some("修复鉴权超时"));

    let (tui_list, _) =
        tiechui_session::summaries(home, Some(work), &CancellationToken::new()).unwrap();
    let tui_item = tui_list
        .iter()
        .find(|s| s.path == Path::new(&opened.path))
        .unwrap();
    assert_eq!(tui_item.meta.title.as_deref(), Some("修复鉴权超时"));

    // Second turn must not trigger another title request.
    submit_text(client, &opened.thread_id, "第二轮追问").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(light_mock.received_requests().await.unwrap().len(), 1);
    client
        .call("session/close", json!({ "thread_id": opened.thread_id }))
        .await
        .unwrap();
}

#[allow(clippy::unwrap_used)]
async fn verify_unset_and_resume_guards(
    client: &mut RpcHarness,
    work: &Path,
    light_mock: &MockServer,
) -> String {
    light_mock.reset().await;
    client
        .call(
            "config/update",
            json!({ "key": "lightweight_model", "value": null }),
        )
        .await
        .unwrap();

    let opened: Snapshot = serde_json::from_value(
        client
            .call("session/open", json!({ "cwd": work }))
            .await
            .unwrap(),
    )
    .unwrap();
    client.drain_notices();
    submit_text(client, &opened.thread_id, "未配置轻量模型首条消息").await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(light_mock.received_requests().await.unwrap().len(), 0);
    assert_eq!(
        tiechui_session::meta(Path::new(&opened.path))
            .unwrap()
            .title,
        None
    );

    client
        .call("session/close", json!({ "thread_id": opened.thread_id }))
        .await
        .unwrap();

    // Open a new empty session while lightweight_model is unset, then configure lightweight_model
    // (triggering session reload before turn 1) and verify turn 1 still generates a title.
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("热更新后生成标题"), "text/event-stream"),
        )
        .expect(1)
        .mount(light_mock)
        .await;
    let pre_opened: Snapshot = serde_json::from_value(
        client
            .call("session/open", json!({ "cwd": work }))
            .await
            .unwrap(),
    )
    .unwrap();
    client.drain_notices();
    client
        .call(
            "config/update",
            json!({ "key": "lightweight_model", "value": "beta/fast" }),
        )
        .await
        .unwrap();
    client.wait_reloaded(&pre_opened.thread_id).await;
    client.drain_notices();
    submit_text(client, &pre_opened.thread_id, "配置轻量模型后发送首轮").await;
    client.wait_sessions_changed().await;
    assert_eq!(
        tiechui_session::meta(Path::new(&pre_opened.path))
            .unwrap()
            .title
            .as_deref(),
        Some("热更新后生成标题")
    );
    client
        .call(
            "session/close",
            json!({ "thread_id": pre_opened.thread_id }),
        )
        .await
        .unwrap();

    opened.path
}

#[allow(clippy::unwrap_used)]
async fn verify_rename_guards(client: &mut RpcHarness, work: &Path, light_mock: &MockServer) {
    // Early rename before turn 1 -> zero lightweight requests.
    light_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(sse_chat("不应覆盖"), "text/event-stream"),
        )
        .mount(light_mock)
        .await;

    let early: Snapshot = serde_json::from_value(
        client
            .call("session/open", json!({ "cwd": work }))
            .await
            .unwrap(),
    )
    .unwrap();
    client
        .call(
            "session/rename",
            json!({ "path": early.path, "title": "手动早期标题" }),
        )
        .await
        .unwrap();
    client.drain_notices();
    submit_text(client, &early.thread_id, "测试早期改名").await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(light_mock.received_requests().await.unwrap().len(), 0);
    assert_eq!(
        tiechui_session::meta(Path::new(&early.path))
            .unwrap()
            .title
            .as_deref(),
        Some("手动早期标题")
    );

    // Mid-flight rename while slow title request is running -> custom rename wins.
    light_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(500))
                .set_body_raw(sse_chat("AI晚到标题"), "text/event-stream"),
        )
        .mount(light_mock)
        .await;

    let mid: Snapshot = serde_json::from_value(
        client
            .call("session/open", json!({ "cwd": work }))
            .await
            .unwrap(),
    )
    .unwrap();
    client.drain_notices();
    submit_text(client, &mid.thread_id, "测试中途改名").await;
    client
        .call(
            "session/rename",
            json!({ "path": mid.path, "title": "手动中途改名" }),
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        tiechui_session::meta(Path::new(&mid.path))
            .unwrap()
            .title
            .as_deref(),
        Some("手动中途改名")
    );
    client
        .call("session/close", json!({ "thread_id": early.thread_id }))
        .await
        .unwrap();
    client
        .call("session/close", json!({ "thread_id": mid.thread_id }))
        .await
        .unwrap();
}

#[allow(clippy::unwrap_used)]
async fn verify_silent_failure(client: &mut RpcHarness, work: &Path, light_mock: &MockServer) {
    light_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream down"))
        .mount(light_mock)
        .await;

    let failing: Snapshot = serde_json::from_value(
        client
            .call("session/open", json!({ "cwd": work }))
            .await
            .unwrap(),
    )
    .unwrap();
    client.drain_notices();
    let events = submit_text(client, &failing.thread_id, "测试失败静默").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!events.iter().any(|e| matches!(e, EventKind::Error { .. })));
    assert_eq!(
        tiechui_session::meta(Path::new(&failing.path))
            .unwrap()
            .title,
        None
    );
    client
        .call("session/close", json!({ "thread_id": failing.thread_id }))
        .await
        .unwrap();
}

fn git_cmd(root: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status()
        .expect("git failed to spawn");
    assert!(status.success(), "git {args:?} failed");
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
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

    let mut client = RpcHarness::new(Server::new(home.clone()));

    git_cmd(&repo, &["init", "-q"]);
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

    client
        .call(
            "config/update",
            json!({ "key": "lightweight_model", "value": "beta/fast" }),
        )
        .await
        .unwrap();

    assert!(
        client
            .call("git/commit_message", json!({ "root": non_git }))
            .await
            .is_err()
    );
    git_cmd(&repo, &["add", "a.txt"]);
    git_cmd(&repo, &["commit", "-q", "-m", "initial"]);
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

    light_mock.reset().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(10))
                .set_body_raw(sse_chat("too late"), "text/event-stream"),
        )
        .mount(&light_mock)
        .await;

    let started = Instant::now();
    let req_id = client.send_async(
        "git/commit_message",
        json!({ "root": repo, "request_id": "cancel-token-1" }),
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    let cancel_res = client
        .call(
            "git/cancel_commit_message",
            json!({ "request_id": "cancel-token-1" }),
        )
        .await
        .unwrap();
    assert_eq!(cancel_res, Value::Null);

    let cancelled = client.wait_reply(req_id).await.unwrap_err();
    assert_eq!(cancelled["code"], -32800, "{cancelled}");
    assert!(started.elapsed() < Duration::from_secs(5));

    // Also support JSON-RPC $/cancelRequest notification by id.
    let rpc_started = Instant::now();
    let rpc_cancel_id = client.send_async("git/commit_message", json!({ "root": repo }));
    tokio::time::sleep(Duration::from_millis(80)).await;
    client.send_notification("$/cancelRequest", json!({ "id": rpc_cancel_id }));
    let rpc_cancelled = client.wait_reply(rpc_cancel_id).await.unwrap_err();
    assert_eq!(rpc_cancelled["code"], -32800, "{rpc_cancelled}");
    assert!(rpc_started.elapsed() < Duration::from_secs(5));

    // Deleting the declared model surfaces a clear error rather than falling back to main model.
    client
        .call(
            "providers/delete_model",
            json!({ "provider": "beta", "id": "fast" }),
        )
        .await
        .unwrap();
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

#[allow(clippy::unwrap_used)]
async fn verify_commit_message_diffs(
    client: &mut RpcHarness,
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

    let fallback: CommitMessageResult = serde_json::from_value(
        client
            .call("git/commit_message", json!({ "root": repo }))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        fallback.message,
        "feat(notes): update tracked and untracked files"
    );

    let reqs = light_mock.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["model"], "fast");
    assert_eq!(body["reasoning_effort"], "none");
    let diff_prompt = body["messages"][1]["content"][0]["text"].as_str().unwrap();
    assert!(diff_prompt.contains("+unstaged edit"));
    assert!(diff_prompt.contains("big_untracked.txt"));
    assert!(diff_prompt.contains("[diff truncated]"));

    // Verify HEAD did not advance (chui never executes git commit).
    let commits = tiechui_workspace::log(repo, 10).unwrap();
    assert_eq!(commits.len(), 1);
    assert_eq!(commits[0].subject, "initial");

    // Stage a single file -> only staged diff is sent.
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
    git_cmd(repo, &["add", "staged_only.txt"]);

    let staged_res: CommitMessageResult = serde_json::from_value(
        client
            .call("git/commit_message", json!({ "root": repo }))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(staged_res.message, "fix: add staged file only");
    let staged_reqs = light_mock.received_requests().await.unwrap();
    let staged_body: Value = serde_json::from_slice(&staged_reqs[0].body).unwrap();
    let staged_prompt = staged_body["messages"][1]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(staged_prompt.contains("staged_only.txt"));
    assert!(!staged_prompt.contains("unstaged edit"));
    assert!(!staged_prompt.contains("big_untracked.txt"));
}
