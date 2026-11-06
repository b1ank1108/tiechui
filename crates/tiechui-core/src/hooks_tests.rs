use super::*;
use serde_json::json;
use tiechui_module_hooks::HooksModule;
use tiechui_module_permissions::{HeadlessPolicy, PermissionsModule};
use tiechui_plugin_api::{Module, ModuleConfig};

#[tokio::test]
async fn permission_request_hooks_resolve_only_calls_requiring_approval() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let home = cwd.join("TIECHUI_HOME");
    let capture = cwd.join("approval.json");
    std::fs::create_dir(&home).unwrap();
    let command = format!(
        "cat > {}; printf '%s' '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PermissionRequest\",\"decision\":{{\"behavior\":\"allow\"}}}}}}'",
        quote(capture.to_str().unwrap())
    );
    std::fs::write(
        home.join("hooks.json"),
        json!({"hooks":{"PermissionRequest":[{"hooks":[{"type":"command","command":command}]}]}})
            .to_string(),
    )
    .unwrap();
    let hooks = HooksModule::load(
        &cwd,
        &home,
        false,
        Default::default(),
        &CancellationToken::new(),
    )
    .unwrap();
    for action in ["ask", "allow", "deny"] {
        let mut registry = Registry::default();
        let mut config = ModuleConfig::default();
        config.permissions.rules = Some(
            serde_json::from_value(json!([{"surface":"bash","pattern":"*","action":action}]))
                .unwrap(),
        );
        hooks.register(&mut registry, &config).unwrap();
        PermissionsModule::headless(cwd.clone(), cwd.clone(), HeadlessPolicy::Unavailable)
            .register(&mut registry, &config)
            .unwrap();
        let verdict = registry.interceptors()[1]
            .interceptor
            .before(
                &ToolCall {
                    id: "call".into(),
                    name: "bash".into(),
                    arguments: json!({"command":"echo original"}),
                    opaque: None,
                },
                ToolContext {
                    invoker: None,
                    branch: Default::default(),
                    parent_session: None,
                    cwd: cwd.clone(),
                    cancel: CancellationToken::new(),
                    updates: Arc::new(|_| {}),
                    append_entry: Arc::new(|_| Ok(())),
                    epoch: SessionEpoch(0),
                    scope: ExecutionScope {
                        thread_id: ThreadId("session".into()),
                        turn_id: TurnId("turn".into()),
                    },
                    origin: Default::default(),
                },
            )
            .await
            .unwrap();
        assert_eq!(capture.exists(), action == "ask");
        assert_eq!(
            matches!(verdict, tiechui_plugin_api::BeforeToolCall::Allow),
            action != "deny"
        );
        if capture.exists() {
            std::fs::remove_file(&capture).unwrap();
        }
    }
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[tokio::test]
async fn hooks_run_in_actor_lifecycle_and_stop_can_continue_the_turn() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let home = root.path().join("TIECHUI_HOME");
    std::fs::create_dir(&home).unwrap();
    let ended = root.path().join("ended");
    let stopped = root.path().join("stopped");
    let stop = format!(
        "cat >/dev/null; if [ ! -e {} ]; then touch {}; printf '%s' '{{\"decision\":\"block\",\"reason\":\"keep going\"}}'; fi",
        quote(stopped.to_str().unwrap()),
        quote(stopped.to_str().unwrap())
    );
    std::fs::write(home.join("hooks.json"), json!({"hooks": {
        "SessionStart": [{"hooks": [{"type":"command","command":"cat >/dev/null; printf '%s' 'session context'"}]}],
        "UserPromptSubmit": [{"hooks": [{"type":"command","command":"cat >/dev/null; printf '%s' 'prompt context'"}]}],
        "Stop": [{"hooks": [{"type":"command","command":stop}]}],
        "SessionEnd": [{"hooks": [{"type":"command","command":format!("cat > {}", quote(ended.to_str().unwrap()))}]}]
    }}).to_string()).unwrap();
    let module = HooksModule::load(
        root.path(),
        &home,
        false,
        Default::default(),
        &CancellationToken::new(),
    )
    .unwrap();
    let mut h = Harness::with_contributions(
        vec![answer("first"), answer("second")],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| module.register(registry, &ModuleConfig::default()).unwrap(),
    )
    .await;
    h.input("hello", InputDelivery::FollowUp).await;
    let first = h.request().await;
    let system = format!("{:?}", first.messages[0]);
    assert!(system.contains("session context"), "{system}");
    assert!(system.contains("prompt context"), "{system}");
    let second = h.request().await;
    assert!(format!("{:?}", second.messages).contains("keep going"));
    h.end(TurnStatus::Completed).await;
    h.stop().await;
    let input: serde_json::Value = serde_json::from_slice(&std::fs::read(ended).unwrap()).unwrap();
    assert_eq!(input["hook_event_name"], "SessionEnd");
    assert!(!input["session_id"].as_str().unwrap().is_empty());
}

#[tokio::test]
async fn rewritten_arguments_are_checked_by_mandatory_permissions() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let cwd = root.path().canonicalize().unwrap();
    let home = cwd.join("TIECHUI_HOME");
    std::fs::create_dir(&home).unwrap();
    std::fs::write(home.join("hooks.json"), json!({"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"cat >/dev/null; printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"allow\",\"updatedInput\":{\"value\":\"rewritten\"}}}'"}]}]}}).to_string()).unwrap();
    let hooks = HooksModule::load(
        &cwd,
        &home,
        false,
        Default::default(),
        &CancellationToken::new(),
    )
    .unwrap();
    let mut config = ModuleConfig::default();
    config.permissions.rules = Some(
        serde_json::from_value(json!([{"surface":"probe","pattern":"*","action":"deny"}])).unwrap(),
    );
    let permissions = PermissionsModule::headless(cwd.clone(), cwd, HeadlessPolicy::Unavailable)
        .with_mode(tiechui_plugin_api::PermissionMode::Yolo);
    let mut h = Harness::with_contributions(
        vec![reply(vec![call("hooked")]), answer("done")],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}),
        |registry| {
            // Tool stages preserve permission ordering even if the policy registered first.
            permissions.register(registry, &config).unwrap();
            hooks.register(registry, &config).unwrap();
        },
    )
    .await;
    h.input("run", InputDelivery::FollowUp).await;
    h.request().await;
    let next = h.request().await;
    assert_eq!(h.tool.runs.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(results(&next)[0].is_error);
    assert!(format!("{:?}", results(&next)[0]).contains("Denied by policy"));
    h.end(TurnStatus::Completed).await;
    h.valid_transcript().await;
    h.stop().await;
}
