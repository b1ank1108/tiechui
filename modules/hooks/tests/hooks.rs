#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tiechui_module_hooks::{Event, Handler, HooksModule, approve, edit, trust};
use tiechui_plugin_api::{
    ApprovalVerdict, BeforeToolCall, CancellationToken, LifecycleContext, LifecycleEvent, Module,
    ModuleConfig, Registry, ToolContext,
    model::ToolCall,
    protocol::{InputContent, SessionEpoch, ThreadId, TurnId},
    serde_json::{self, Value, json},
    tools::ExecutionScope,
};

struct Fixture {
    _root: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let home = cwd.join("TIECHUI_HOME");
        std::fs::create_dir(&home).unwrap();
        Self {
            _root: root,
            cwd,
            home,
        }
    }
    fn write(&self, path: &Path, event: Event, handlers: Value, matcher: Option<&str>) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            json!({"hooks": {event.wire(): [{"matcher": matcher, "hooks": handlers}]}}).to_string(),
        )
        .unwrap();
    }
    fn command(&self, capture: &str, stdout: &str, stderr: &str, code: i32) -> String {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/command.sh");
        format!(
            "/bin/sh {} {} {} {} {code}",
            quote(script.to_str().unwrap()),
            quote(self.cwd.join(capture).to_str().unwrap()),
            quote(stdout),
            quote(stderr)
        )
    }
    fn hook(&self, event: Event, stdout: &str, stderr: &str, code: i32) -> HooksModule {
        self.write(
            &self.home.join("hooks.json"),
            event,
            json!([{"type":"command", "command":self.command("stdin.json", stdout, stderr, code)}]),
            None,
        );
        self.load(true)
    }
    fn load(&self, trusted: bool) -> HooksModule {
        HooksModule::load(
            &self.cwd,
            &self.home,
            trusted,
            Default::default(),
            &CancellationToken::new(),
        )
        .unwrap()
    }
    fn ctx(&self) -> ToolContext {
        ToolContext {
            invoker: None,
            branch: Default::default(),
            parent_session: None,
            cwd: self.cwd.clone(),
            cancel: CancellationToken::new(),
            updates: Arc::new(|_| {}),
            append_entry: Arc::new(|_| Ok(())),
            epoch: SessionEpoch(0),
            scope: ExecutionScope {
                thread_id: ThreadId("session".into()),
                turn_id: TurnId("turn".into()),
            },
            origin: Default::default(),
        }
    }
    fn lifecycle(&self) -> LifecycleContext {
        LifecycleContext {
            session_id: "session".into(),
            turn_id: "turn".into(),
            cwd: self.cwd.clone(),
            transcript_path: Some(self.cwd.join("session.jsonl")),
            model: "fixture".into(),
        }
    }
}

fn call() -> ToolCall {
    ToolCall {
        id: "call".into(),
        name: "bash".into(),
        arguments: json!({"command":"echo original"}),
        opaque: None,
    }
}

macro_rules! event_test {
    ($name:ident, $event:ident, $life:expr) => {
        #[tokio::test]
        async fn $name() {
            let f = Fixture::new();
            let hooks = f.hook(Event::$event, "", "", 0);
            let mut registry = Registry::default();
            hooks
                .register(&mut registry, &ModuleConfig::default())
                .unwrap();
            registry.lifecycle_interceptors()[0]
                .on_event(&$life, &f.lifecycle(), CancellationToken::new())
                .await
                .unwrap();
            let input: Value =
                serde_json::from_slice(&std::fs::read(f.cwd.join("stdin.json")).unwrap()).unwrap();
            assert_eq!(input["hook_event_name"], Event::$event.wire());
            assert_eq!(input["session_id"], "session");
            assert_eq!(input["cwd"], f.cwd.to_str().unwrap());
            assert_eq!(
                input["transcript_path"],
                f.cwd.join("session.jsonl").to_str().unwrap()
            );
            match Event::$event {
                Event::SessionStart => {
                    assert_eq!(input["source"], "startup");
                    assert!(input.get("turn_id").is_none());
                }
                Event::UserPromptSubmit => assert_eq!(input["prompt"], "hello"),
                Event::PreCompact => {
                    assert_eq!(input["trigger"], "manual");
                    assert!(input.get("permission_mode").is_none());
                }
                Event::Stop => {
                    assert_eq!(input["stop_hook_active"], false);
                    assert_eq!(input["last_assistant_message"], "done");
                }
                Event::SessionEnd => {
                    assert_eq!(input["reason"], "other");
                    assert!(input.get("model").is_none());
                }
                _ => unreachable!(),
            }
        }
    };
}

event_test!(
    session_start,
    SessionStart,
    LifecycleEvent::SessionStart { resumed: false }
);
event_test!(
    user_prompt_submit,
    UserPromptSubmit,
    LifecycleEvent::UserInput {
        content: vec![InputContent::Text {
            text: "hello".into()
        }]
    }
);
event_test!(
    pre_compact,
    PreCompact,
    LifecycleEvent::BeforeCompaction { automatic: false }
);
event_test!(
    stop,
    Stop,
    LifecycleEvent::BeforeStop {
        active: false,
        last_message: Some("done".into())
    }
);
event_test!(session_end, SessionEnd, LifecycleEvent::SessionEnd);

#[tokio::test]
async fn permission_mode_is_read_when_a_hook_runs() {
    let f = Fixture::new();
    f.hook(Event::UserPromptSubmit, "", "", 0);
    let mode = tiechui_plugin_api::SharedPermissionMode::default();
    let hooks = HooksModule::load(
        &f.cwd,
        &f.home,
        true,
        mode.clone(),
        &CancellationToken::new(),
    )
    .unwrap();
    let mut registry = Registry::default();
    hooks
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    for (set, wire) in [
        (tiechui_plugin_api::PermissionMode::Rules, "default"),
        (tiechui_plugin_api::PermissionMode::Auto, "auto"),
        (
            tiechui_plugin_api::PermissionMode::Yolo,
            "bypassPermissions",
        ),
    ] {
        mode.set(set);
        let event = LifecycleEvent::UserInput {
            content: vec![InputContent::Text {
                text: "hello".into(),
            }],
        };
        registry.lifecycle_interceptors()[0]
            .on_event(&event, &f.lifecycle(), CancellationToken::new())
            .await
            .unwrap();
        let input: Value =
            serde_json::from_slice(&std::fs::read(f.cwd.join("stdin.json")).unwrap()).unwrap();
        assert_eq!(input["permission_mode"], wire);
    }
}

#[tokio::test]
async fn pre_tool_use_blocks_allows_and_rewrites_without_bypassing_policy() {
    let f = Fixture::new();
    for (stdout, stderr, code, expected) in [
        ("", "blocked", 2, BeforeToolCall::Deny("blocked".into())),
        (
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"policy"}}"#,
            "",
            0,
            BeforeToolCall::Deny("policy".into()),
        ),
        (
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"echo rewritten"}}}"#,
            "",
            0,
            BeforeToolCall::Rewrite(json!({"command":"echo rewritten"})),
        ),
        (
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}"#,
            "",
            0,
            BeforeToolCall::Allow,
        ),
        (r#"{"continue":false}"#, "", 0, BeforeToolCall::Allow),
        (
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"bad"}}}"#,
            "",
            0,
            BeforeToolCall::Allow,
        ),
        ("{malformed", "", 0, BeforeToolCall::Allow),
        ("", "", 2, BeforeToolCall::Allow),
        ("", "error", 1, BeforeToolCall::Allow),
    ] {
        let hooks = f.hook(Event::PreToolUse, stdout, stderr, code);
        let mut registry = Registry::default();
        hooks
            .register(&mut registry, &ModuleConfig::default())
            .unwrap();
        assert_eq!(
            registry.interceptors()[0]
                .interceptor
                .before(&call(), f.ctx())
                .await
                .unwrap(),
            expected
        );
        let input: Value =
            serde_json::from_slice(&std::fs::read(f.cwd.join("stdin.json")).unwrap()).unwrap();
        assert_eq!(input["tool_name"], "Bash");
        assert_eq!(input["tool_use_id"], "call");
        assert_eq!(input["tool_input"], call().arguments);
    }
}

#[tokio::test]
async fn post_tool_use_injects_feedback_after_success() {
    let f = Fixture::new();
    let hooks = f.hook(
        Event::PostToolUse,
        r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"feedback"}}"#,
        "",
        0,
    );
    let mut registry = Registry::default();
    hooks
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let output = registry.interceptors()[0]
        .interceptor
        .after(
            &call(),
            &tiechui_plugin_api::ToolOutput::text("done"),
            f.ctx(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.content.len(), 2);
    let input: Value =
        serde_json::from_slice(&std::fs::read(f.cwd.join("stdin.json")).unwrap()).unwrap();
    assert_eq!(input["hook_event_name"], "PostToolUse");
    assert_eq!(input["tool_response"]["content"][0]["text"], "done");
}

#[tokio::test]
async fn permission_request_input_and_denies_win() {
    let f = Fixture::new();
    let hooks = f.hook(Event::PermissionRequest, r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#, "", 0);
    let mut registry = Registry::default();
    hooks
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    assert_eq!(
        registry.approval_interceptors()[0]
            .review(&call(), f.ctx())
            .await
            .unwrap(),
        Some(ApprovalVerdict::Allow)
    );
    let input: Value =
        serde_json::from_slice(&std::fs::read(f.cwd.join("stdin.json")).unwrap()).unwrap();
    assert_eq!(input["hook_event_name"], "PermissionRequest");
    assert!(input.get("tool_use_id").is_none());
    let deny = f.command("deny.json", r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"denied"}}}"#, "", 0);
    let allow = f.command("allow.json", r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#, "", 0);
    f.write(
        &f.home.join("hooks.json"),
        Event::PermissionRequest,
        json!([{"type":"command","command":deny},{"type":"command","command":allow}]),
        None,
    );
    let outcome = f
        .load(true)
        .dispatch(
            Event::PermissionRequest,
            json!({}),
            Some("bash"),
            &f.cwd,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.approval,
        Some(ApprovalVerdict::Deny("denied".into()))
    );
}

#[tokio::test]
async fn changed_project_hook_is_disabled_until_reapproved_including_live_sessions() {
    for source in [
        ".agents/hooks.json",
        ".codex/hooks.json",
        ".claude/settings.json",
        ".claude/settings.local.json",
    ] {
        let f = Fixture::new();
        let path = f.cwd.join(source);
        f.write(
            &path,
            Event::PreToolUse,
            json!([{"type":"command","command":f.command("project.json", "", "blocked", 2)}]),
            None,
        );
        assert!(
            !f.load(true)
                .hooks()
                .iter()
                .find(|h| h.path == path)
                .unwrap()
                .enabled
        );
        trust(&f.cwd, &f.home, &CancellationToken::new()).unwrap();
        assert!(
            !f.load(false)
                .hooks()
                .iter()
                .find(|h| h.path == path)
                .unwrap()
                .enabled
        );
        let original = f.load(true);
        assert!(
            original
                .hooks()
                .iter()
                .find(|h| h.path == path)
                .unwrap()
                .enabled
        );
        let bytes = std::fs::read(&path).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
        assert!(
            f.load(true)
                .hooks()
                .iter()
                .find(|h| h.path == path)
                .unwrap()
                .enabled
        );
        f.write(
            &path,
            Event::PreToolUse,
            json!([{"type":"command","command":f.command("changed.json", "", "changed", 2)}]),
            None,
        );
        assert_eq!(
            f.load(true)
                .hooks()
                .iter()
                .find(|h| h.path == path)
                .unwrap()
                .trust,
            "changed"
        );
        let outcome = original
            .dispatch(
                Event::PreToolUse,
                json!({}),
                Some("bash"),
                &f.cwd,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!outcome.diagnostics.is_empty());
        assert!(!f.cwd.join("changed.json").exists());
        trust(&f.cwd, &f.home, &CancellationToken::new()).unwrap();
        assert!(
            f.load(true)
                .hooks()
                .iter()
                .find(|h| h.path == path)
                .unwrap()
                .enabled
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn timeouts_and_cancellation_kill_process_groups_and_fail_open() {
    let f = Fixture::new();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/timeout.sh");
    let pidfile = f.cwd.join("child.pid");
    let command = format!(
        "/bin/sh {} {}",
        quote(script.to_str().unwrap()),
        quote(pidfile.to_str().unwrap())
    );
    f.write(
        &f.home.join("hooks.json"),
        Event::PreToolUse,
        json!([{"type":"command","command":command,"timeout":1}]),
        None,
    );
    let hooks = f.load(true);
    let started = Instant::now();
    let outcome = hooks
        .dispatch(
            Event::PreToolUse,
            json!({}),
            Some("bash"),
            &f.cwd,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(outcome.block.is_none());
    assert!(outcome.diagnostics.iter().any(|d| d.contains("timed out")));
    let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().parse().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_ne!(unsafe { libc::kill(pid, 0) }, 0);
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    assert!(
        hooks
            .dispatch(Event::PreToolUse, json!({}), Some("bash"), &f.cwd, cancel)
            .await
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn matchers_parallel_rewrites_and_session_end_limits() {
    let f = Fixture::new();
    let slow = format!("sleep 0.1; {}", f.command("slow.json", r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"slow"}}}"#, "", 0));
    let fast = f.command("fast.json", r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"command":"fast"}}}"#, "", 0);
    f.write(
        &f.home.join("hooks.json"),
        Event::PreToolUse,
        json!([{"type":"command","command":slow}, {"type":"command","command":fast}]),
        Some("^ba.*$"),
    );
    let hooks = f.load(true);
    assert!(
        hooks
            .dispatch(
                Event::PreToolUse,
                json!({}),
                Some("read"),
                &f.cwd,
                CancellationToken::new()
            )
            .await
            .unwrap()
            .updated_input
            .is_none()
    );
    assert_eq!(
        hooks
            .dispatch(
                Event::PreToolUse,
                json!({}),
                Some("bash"),
                &f.cwd,
                CancellationToken::new()
            )
            .await
            .unwrap()
            .updated_input,
        Some(json!({"command":"slow"}))
    );
    f.write(
        &f.home.join("hooks.json"),
        Event::SessionEnd,
        json!([{"type":"command","command":"true","async":true,"timeout":600}]),
        None,
    );
    let hooks = f.load(true);
    assert_eq!(hooks.hooks()[0].timeout, 3);
    assert!(!hooks.hooks()[0].asynchronous);
}

#[tokio::test]
async fn lifecycle_control_and_async_effects() {
    let f = Fixture::new();
    for (event, stdout, stderr, code, stopped, blocked) in [
        (
            Event::SessionStart,
            r#"{"continue":false,"stopReason":"pause"}"#,
            "",
            0,
            true,
            false,
        ),
        (
            Event::UserPromptSubmit,
            r#"{"decision":"block","reason":"pause"}"#,
            "",
            0,
            false,
            true,
        ),
        (
            Event::PreCompact,
            r#"{"continue":false}"#,
            "",
            0,
            true,
            false,
        ),
        (Event::Stop, "", "continue work", 2, false, true),
        (Event::Stop, r#"{"continue":false}"#, "", 0, true, false),
        (Event::PermissionRequest, "", "denied", 2, false, false),
    ] {
        let hooks = f.hook(event, stdout, stderr, code);
        let outcome = hooks
            .dispatch(event, json!({}), None, &f.cwd, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(outcome.stop.is_some(), stopped, "{event:?}");
        assert_eq!(outcome.block.is_some(), blocked, "{event:?}");
        if event == Event::PermissionRequest {
            assert_eq!(
                outcome.approval,
                Some(ApprovalVerdict::Deny("denied".into()))
            );
        }
    }
    f.write(&f.home.join("hooks.json"), Event::UserPromptSubmit, json!([{"type":"command", "async":true,
        "command": f.command("async.json", r#"{"decision":"block","reason":"ignored","hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"async context"}}"#, "", 0)}]), Some("[invalid regex ignored"));
    let hooks = f.load(true);
    let result = hooks
        .dispatch(
            Event::UserPromptSubmit,
            json!({}),
            None,
            &f.cwd,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(result.block.is_none());
    let mut registry = Registry::default();
    hooks
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let mut context = result.context;
    for _ in 0..100 {
        let outcome = registry.lifecycle_interceptors()[0]
            .on_event(
                &LifecycleEvent::BeforeRequest,
                &f.lifecycle(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(outcome.block.is_none() && outcome.stop.is_none());
        context.extend(outcome.context);
        if !context.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(context, vec!["async context"]);
}

#[tokio::test]
async fn bash_matchers_work_unchanged_and_stalled_stdin_is_inside_deadline() {
    let f = Fixture::new();
    f.write(
        &f.home.join("hooks.json"),
        Event::PreToolUse,
        json!([{"type":"command", "command":f.command("canonical.json", "", "Bash matcher", 2)}]),
        Some("Bash"),
    );
    let mut registry = Registry::default();
    f.load(true)
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    assert_eq!(
        registry.interceptors()[0]
            .interceptor
            .before(&call(), f.ctx())
            .await
            .unwrap(),
        BeforeToolCall::Deny("Bash matcher".into())
    );
    let input: Value =
        serde_json::from_slice(&std::fs::read(f.cwd.join("canonical.json")).unwrap()).unwrap();
    assert_eq!(input["tool_name"], "Bash");
    f.write(
        &f.home.join("hooks.json"),
        Event::PreToolUse,
        json!([{"type":"command", "command":"sleep 30", "timeout":1}]),
        None,
    );
    let started = Instant::now();
    let outcome = f
        .load(true)
        .dispatch(
            Event::PreToolUse,
            json!({"prompt":"x".repeat(256 * 1024)}),
            None,
            &f.cwd,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(outcome.diagnostics.iter().any(|d| d.contains("timed out")));
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[test]
fn approve_covers_the_reviewed_handlers_not_later_edits() {
    let f = Fixture::new();
    let path = f.cwd.join(".agents/hooks.json");
    let handler = |command: &str| json!([{"type": "command", "command": command}]);
    f.write(&path, Event::Stop, handler("true"), None);
    let cancel = CancellationToken::new();
    let reviewed = f.load(true).hooks().to_vec();
    f.write(&path, Event::Stop, handler("false"), None);
    approve(&f.home, &reviewed, &cancel).unwrap();
    let edited = f.load(true);
    assert_eq!(edited.hooks()[0].trust, "changed");
    assert!(!edited.hooks()[0].enabled);
    approve(&f.home, edited.hooks(), &cancel).unwrap();
    assert!(f.load(true).hooks()[0].enabled);
}

#[test]
fn normalized_hash_matches_canonical_json_with_default_fields() {
    let f = Fixture::new();
    let file = f.cwd.join(".agents/hooks.json");
    f.write(
        &file,
        Event::PreToolUse,
        json!([{"type":"command","command":"true"}]),
        None,
    );
    let hooks = f.load(true);
    assert_eq!(
        hooks.hooks()[0].hash,
        "sha256:a7de97b60e1412f8e936f33ad7e97c04dfe69190d43e61d14c46265643383835"
    );
    trust(&f.cwd, &f.home, &CancellationToken::new()).unwrap();
    f.write(
        &file,
        Event::PreToolUse,
        json!([{"type":"command","command":"true","timeout":600,"async":false}]),
        None,
    );
    assert!(f.load(true).hooks()[0].enabled);
    f.write(
        &file,
        Event::PreToolUse,
        json!([{"type":"command","command":"true","timeout":10}]),
        None,
    );
    assert_eq!(f.load(true).hooks()[0].trust, "changed");
}

#[test]
fn edits_keep_approvals_and_approve_the_written_handler() {
    let f = Fixture::new();
    let path = f.cwd.join(".agents/hooks.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let group =
        |command: &str| json!({"hooks": [{"type": "command", "command": command, "async": true}]});
    std::fs::write(
        &path,
        json!({"hooks": {"Stop": [group("first"), group("second")]}}).to_string(),
    )
    .unwrap();
    let cancel = CancellationToken::new();
    approve(&f.home, &f.load(true).hooks()[1..], &cancel).unwrap();
    let listed = || {
        f.load(true)
            .hooks()
            .iter()
            .map(|hook| (hook.command.clone(), hook.trust.clone(), hook.key.clone()))
            .collect::<Vec<_>>()
    };
    let handler = |project, event, matcher: Option<&str>, command: &str| Handler {
        project,
        event,
        matcher: matcher.map(Into::into),
        command: command.into(),
        timeout: 5,
    };
    let first = listed()[0].2.clone();
    edit(
        &f.cwd,
        &f.home,
        Some(&first),
        Some(handler(true, Event::Stop, None, " edited ")),
        &cancel,
    )
    .unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"async\": true") && text.contains("\"timeout\": 5"));
    assert_eq!(listed()[0].0, "edited");
    assert!(listed().iter().all(|(_, trust, _)| trust == "trusted"));

    edit(&f.cwd, &f.home, Some(&first), None, &cancel).unwrap();
    let [(command, trust, second)] = &listed()[..] else {
        unreachable!()
    };
    assert_eq!((command.as_str(), trust.as_str()), ("second", "trusted"));
    assert_eq!(second, &first);

    edit(
        &f.cwd,
        &f.home,
        None,
        Some(handler(true, Event::PreToolUse, Some("Bash"), "added")),
        &cancel,
    )
    .unwrap();
    edit(
        &f.cwd,
        &f.home,
        Some(second),
        Some(handler(false, Event::Stop, None, "second")),
        &cancel,
    )
    .unwrap();
    let hooks = f.load(true);
    let summary: Vec<_> = hooks
        .hooks()
        .iter()
        .map(|hook| {
            (
                hook.command.as_str(),
                hook.source(),
                hook.trust.as_str(),
                hook.matcher.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("second", "user", "user", None),
            ("added", "project", "trusted", Some("Bash"))
        ]
    );

    let claude = f.cwd.join(".claude/settings.json");
    f.write(
        &claude,
        Event::Stop,
        json!([{"type": "command", "command": "x"}]),
        None,
    );
    let key = f.load(true).hooks().last().unwrap().key.clone();
    let error = edit(&f.cwd, &f.home, Some(&key), None, &cancel).unwrap_err();
    assert_eq!(error.0, "Only hooks in hooks.json files can be edited");
    let error = edit(
        &f.cwd,
        &f.home,
        None,
        Some(handler(false, Event::Stop, None, " ")),
        &cancel,
    )
    .unwrap_err();
    assert_eq!(error.0, "The command is empty");
}
