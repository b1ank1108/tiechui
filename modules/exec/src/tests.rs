#![allow(clippy::unwrap_used)]

use super::*;
use std::time::Duration;
use tiechui_plugin_api::{
    model::Content,
    protocol::{SessionEpoch, ShellState, ShellsView, ThreadId, TurnId},
    tools::{ExecutionScope, ToolOrigin},
};

struct Harness {
    shells: Arc<Shells>,
    registry: Registry,
}

fn branch() -> BranchView {
    BranchView {
        session_id: "thread".into(),
        leaf: "leaf".into(),
        cwd: std::env::temp_dir(),
        parent_session: None,
        entries: vec![],
    }
}

fn context(cancel: CancellationToken, origin: ToolOrigin) -> ToolContext {
    ToolContext {
        invoker: None,
        branch: Default::default(),
        parent_session: None,
        cwd: std::env::temp_dir(),
        cancel,
        updates: Arc::new(|_| {}),
        append_entry: Arc::new(|_| Ok(())),
        epoch: SessionEpoch(1),
        scope: ExecutionScope {
            thread_id: ThreadId("thread".into()),
            turn_id: TurnId("turn".into()),
        },
        origin,
    }
}

impl Harness {
    fn new() -> Self {
        let registry = Registry::from_modules(
            &[&ExecModule { background: true }],
            &ModuleConfig::default(),
        )
        .unwrap();
        let shells = Arc::new(Shells::default());
        shells.restore("thread");
        Self { shells, registry }
    }

    fn bash(&self) -> Bash {
        Bash {
            shells: self.shells.clone(),
            spec: bash_spec(),
        }
    }

    fn manage(&self, kind: Kind) -> Manage {
        Manage {
            shells: self.shells.clone(),
            kind,
            spec: ToolSpec {
                name: String::new(),
                description: String::new(),
                input_schema: json!({}),
            },
        }
    }

    async fn run(&self, tool: &dyn Tool, arguments: Value) -> Result<ToolOutput, ToolError> {
        tool.run(
            call(arguments),
            context(CancellationToken::new(), ToolOrigin::Model),
        )
        .await
    }

    fn view(&self) -> ShellsView {
        self.shells.view(false, None).unwrap()
    }

    async fn settled(&self, id: &str) {
        for _ in 0..500 {
            if !self.shells.task(id).unwrap().state.active() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{id} did not settle");
    }

    async fn running(&self, id: &str) {
        for _ in 0..500 {
            if self
                .shells
                .task(id)
                .is_ok_and(|t| t.state == ShellState::Running)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{id} did not start");
    }
}

fn call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "call".into(),
        name: "bash".into(),
        arguments,
        opaque: None,
    }
}

fn text(output: &ToolOutput) -> &str {
    match &output.content[..] {
        [Content::Text { text, .. }] => text,
        _ => panic!("expected one text block"),
    }
}

#[test]
fn registration_extends_bash_and_children_keep_the_plain_tool() {
    let h = Harness::new();
    let tools = h.registry.tools();
    let bash = tools.get("bash").unwrap();
    assert_eq!(
        bash.spec().input_schema["properties"]["background"]["type"],
        "boolean"
    );
    for name in ["shell_list", "shell_output", "shell_stop"] {
        assert!(tools.get(name).is_some(), "{name}");
    }
    assert!(
        h.registry
            .commands()
            .iter()
            .any(|c| c.info().name == "shells")
    );
    assert_eq!(h.registry.background_work().len(), 1);
    let child = Registry::from_modules(
        &[&ExecModule { background: false }],
        &ModuleConfig::default(),
    )
    .unwrap();
    assert_eq!(
        child.tools().get("bash").unwrap().spec(),
        BashTool::default().spec()
    );
    assert!(child.tools().get("shell_output").is_none());
    assert!(child.background_work().is_empty() && child.commands().is_empty());
}

#[tokio::test]
async fn background_start_counts_and_notifies_the_model_once() {
    let h = Harness::new();
    let output = h
        .run(
            &h.bash(),
            json!({"command":"printf hi; sleep 1","background":true}),
        )
        .await
        .unwrap();
    let structured = output.structured_content.clone().unwrap();
    assert_eq!(structured["task_id"], "sh1");
    assert_eq!(structured["state"], "running");
    assert_eq!(structured["background"], true);
    assert_eq!(structured["cursor"], 2);
    assert_eq!(
        text(&output),
        "hi\n\n[sh1 · started in the background · cursor 2 · you will be told when it finishes]"
    );
    let shell = &output.details.as_ref().unwrap()["shell"];
    assert_eq!(shell["command"], "printf hi; sleep 1");
    assert_eq!(shell["new_output"], true);
    assert_eq!(h.view().background(), 1);
    assert!(h.shells.continuation().is_none());
    h.settled("sh1").await;
    assert_eq!(h.view().background(), 0);
    let next = h.shells.continuation().unwrap();
    assert_eq!(next.kind, CONTINUATION);
    assert_eq!(next.identity, "sh1");
    assert!(next.text.contains("sh1 (exit 0"), "{}", next.text);
    let events = h.shells.events();
    assert!(events.iter().any(|(name, data)| {
        name == "module.notice"
            && data
                .as_str()
                .unwrap()
                .starts_with("Shell sh1 finished · exit 0")
    }));
    assert!(events.iter().any(|(name, _)| name == SHELLS_EVENT));
    assert!(
        h.shells.events().is_empty(),
        "unchanged state emits nothing"
    );
    let event = RuntimeEvent::Input {
        continuation: Some(next),
        retry: false,
    };
    h.shells.event(&event, &branch()).unwrap();
    assert!(h.shells.continuation().is_none());
}

#[tokio::test]
async fn foreground_command_moves_to_the_background_without_restarting() {
    let h = Harness::new();
    let bash = h.bash();
    let cancel = CancellationToken::new();
    let turn = cancel.clone();
    let waiter = tokio::spawn(async move {
        bash.run(
            call(json!({"command":"echo start; sleep 0.3; echo end"})),
            context(turn, ToolOrigin::Model),
        )
        .await
    });
    h.running("sh1").await;
    assert_eq!(h.view().background(), 0, "foreground keeps its tool card");
    let command = ShellsCommand(h.shells.clone());
    assert_eq!(
        command.text("background sh1").unwrap(),
        "sh1 moved to the background."
    );
    let output = waiter.await.unwrap().unwrap();
    assert!(text(&output).contains("moved this command to the background as sh1"));
    assert_eq!(h.view().background(), 1);
    cancel.cancel();
    h.settled("sh1").await;
    let read = h
        .run(&h.manage(Kind::Output), json!({"task_id":"sh1"}))
        .await
        .unwrap();
    assert_eq!(
        read.structured_content.as_ref().unwrap()["output"],
        "start\nend\n"
    );
    assert_eq!(read.structured_content.unwrap()["state"], "completed");
    assert!(
        h.shells.continuation().is_none(),
        "a completion the model already read is not announced"
    );
    assert!(command.text("background sh1").is_err());
}

#[tokio::test]
async fn turn_cancellation_stops_foreground_and_keeps_background() {
    let h = Harness::new();
    h.run(&h.bash(), json!({"command":"sleep 30","background":true}))
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let bash = h.bash();
    let turn = cancel.clone();
    let waiter = tokio::spawn(async move {
        bash.run(
            call(json!({"command":"printf partial; sleep 30"})),
            context(turn, ToolOrigin::Model),
        )
        .await
    });
    h.running("sh2").await;
    cancel.cancel();
    let error = waiter.await.unwrap().unwrap_err();
    assert!(error.0.ends_with("Command aborted"), "{}", error.0);
    assert_eq!(h.shells.task("sh2").unwrap().state, ShellState::Cancelled);
    assert_eq!(h.shells.task("sh1").unwrap().state, ShellState::Running);
    assert!(h.shells.continuation().is_none());
    h.shells.quiesce().await;
    assert_eq!(h.shells.task("sh1").unwrap().state, ShellState::Cancelled);
    assert!(
        h.shells.continuation().is_none(),
        "session cleanup is silent"
    );
}

#[tokio::test]
async fn dropped_foreground_waiter_kills_its_process() {
    let h = Harness::new();
    let bash = h.bash();
    let waiter = tokio::spawn(async move {
        bash.run(
            call(json!({"command":"sleep 30"})),
            context(CancellationToken::new(), ToolOrigin::Model),
        )
        .await
    });
    h.running("sh1").await;
    waiter.abort();
    h.settled("sh1").await;
    assert_eq!(h.shells.task("sh1").unwrap().state, ShellState::Cancelled);
}

#[tokio::test]
async fn stop_is_idempotent_and_reports_the_final_state() {
    let h = Harness::new();
    h.run(&h.bash(), json!({"command":"sleep 30","background":true}))
        .await
        .unwrap();
    let stop = h.manage(Kind::Stop);
    for _ in 0..2 {
        let output = h.run(&stop, json!({"task_id":"sh1"})).await.unwrap();
        assert_eq!(output.structured_content.unwrap()["state"], "cancelled");
    }
    assert!(h.shells.continuation().is_none());
    assert!(h.run(&stop, json!({"task_id":"sh9"})).await.is_err());
}

#[tokio::test]
async fn output_reads_resume_from_the_cursor_and_wait_for_news() {
    let h = Harness::new();
    h.run(
        &h.bash(),
        json!({"command":"printf abc; sleep 1; printf def","background":true}),
    )
    .await
    .unwrap();
    let output = h.manage(Kind::Output);
    let first = h
        .run(&output, json!({"task_id":"sh1","cursor":0,"wait_ms":5000}))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(first["output"], "abc");
    assert_eq!(first["cursor"], 3);
    let next = h
        .run(&output, json!({"task_id":"sh1","cursor":3,"wait_ms":5000}))
        .await
        .unwrap()
        .structured_content
        .unwrap();
    assert_eq!(next["output"], "def");
    h.settled("sh1").await;
    let tail = h
        .run(&output, json!({"task_id":"sh1","max_bytes":2}))
        .await
        .unwrap();
    assert_eq!(tail.structured_content.as_ref().unwrap()["output"], "ef");
    assert!(text(&tail).contains("earlier output omitted"));
    let empty = h
        .run(&output, json!({"task_id":"sh1","cursor":6}))
        .await
        .unwrap();
    assert!(text(&empty).starts_with("(no new output)"));
    let shell = &empty.details.as_ref().unwrap()["shell"];
    assert_eq!(shell["state"], "completed");
    assert_eq!(shell["new_output"], false);
    assert!(
        h.run(&output, json!({"task_id":"sh1","cursor":7}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn user_commands_and_limits() {
    let h = Harness::new();
    let bash = h.bash();
    let waiter = tokio::spawn(async move {
        bash.run(
            call(json!({"command":"sleep 0.2"})),
            context(CancellationToken::new(), ToolOrigin::User),
        )
        .await
    });
    h.running("sh1").await;
    h.shells.background("sh1").unwrap();
    waiter.await.unwrap().unwrap();
    assert!(h.shells.task("sh1").unwrap().user);
    h.settled("sh1").await;
    assert!(
        h.shells.continuation().is_none(),
        "user commands only notify the interface"
    );
    for _ in 0..shells::MAX_ACTIVE {
        h.run(&h.bash(), json!({"command":"sleep 30","background":true}))
            .await
            .unwrap();
    }
    let error = h
        .run(&h.bash(), json!({"command":"true","background":true}))
        .await
        .unwrap_err();
    assert!(error.0.starts_with("At most 8"), "{}", error.0);
    let list = h.run(&h.manage(Kind::List), json!({})).await.unwrap();
    assert_eq!(text(&list).lines().count(), 1 + shells::MAX_ACTIVE);
    h.shells.event(&RuntimeEvent::Shutdown, &branch()).unwrap();
    h.shells.quiesce().await;
    assert!(h.shells.tasks().unwrap().iter().all(|t| !t.state.active()));
}

#[tokio::test]
async fn background_commands_that_end_at_once_return_their_whole_result() {
    let h = Harness::new();
    let output = h
        .run(
            &h.bash(),
            json!({"command":"printf done","background":true}),
        )
        .await
        .unwrap();
    assert_eq!(text(&output), "done");
    assert_eq!(h.shells.task("sh1").unwrap().state, ShellState::Completed);
    assert!(
        h.shells.continuation().is_none(),
        "the model already has the result"
    );
    let output = h
        .run(&h.bash(), json!({"command":"sleep 30","background":true}))
        .await
        .unwrap();
    assert!(text(&output).starts_with("(no output yet)\n\n[sh2 · started"));
    assert_eq!(output.details.unwrap()["shell"]["new_output"], false);
    h.shells.quiesce().await;
}

#[tokio::test]
async fn instant_failures_never_count_as_running() {
    let h = Harness::new();
    let output = h
        .run(&h.bash(), json!({"command":"exit 3","background":true}))
        .await
        .unwrap();
    assert!(output.is_error);
    assert_eq!(output.structured_content.unwrap()["exit_code"], 3);
    assert_eq!(h.shells.task("sh1").unwrap().state, ShellState::Failed);
    assert!(h.shells.continuation().is_none());
    assert_eq!(h.shells.task("sh1").unwrap().exit_code, Some(3));
    let mut ctx = context(CancellationToken::new(), ToolOrigin::Model);
    ctx.cwd = std::env::temp_dir().join("tiechui-missing-shell-cwd");
    assert!(
        h.bash()
            .run(call(json!({"command":"true","background":true})), ctx)
            .await
            .unwrap_err()
            .0
            .starts_with("Working directory does not exist")
    );
    assert_eq!(
        h.shells.tasks().unwrap().len(),
        1,
        "no task for a refused start"
    );
}

#[tokio::test]
async fn foreground_and_background_exec_modules_inject_network_env() {
    let mut cfg = ModuleConfig::default();
    cfg.network.proxy = Some("socks5://127.0.0.1:1080".into());
    cfg.network.ca_cert = Some(std::path::PathBuf::from("/etc/ssl/corp.pem"));
    for background in [false, true] {
        let registry = Registry::from_modules(&[&ExecModule { background }], &cfg).unwrap();
        for runtime in registry.runtime_extensions() {
            runtime
                .event(&RuntimeEvent::Restore { navigation: false }, &branch())
                .unwrap();
        }
        let tools = registry.tools();
        let tool = tools.get("bash").unwrap();
        let output = tool
            .run(
                call(json!({
                    "command": "printf '%s|%s|<%s>' \"$HTTP_PROXY\" \"$SSL_CERT_FILE\" \"$NO_PROXY\""
                })),
                context(CancellationToken::new(), ToolOrigin::Model),
            )
            .await
            .unwrap();
        assert_eq!(
            text(&output),
            "socks5://127.0.0.1:1080|/etc/ssl/corp.pem|<>",
            "background={background}"
        );
    }
}
