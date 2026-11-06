#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tiechui_module_permissions::{
    ApprovalDecision, ApprovalRequest, ApprovalRequester, HeadlessPolicy, PermissionsModule,
};
use tiechui_plugin_api::{
    BeforeToolCall, CancellationToken, Module, ModuleConfig, PermissionMode, Registry, ToolContext,
    ToolError, async_trait,
    model::ToolCall,
    protocol::{SessionEpoch, ThreadId, TurnId},
    serde_json::{self, Value, json},
    tools::{CustomSessionEntry, ExecutionScope},
};

#[derive(Default)]
struct Prompter {
    requests: Mutex<Vec<ApprovalRequest>>,
    decision: Mutex<Option<ApprovalDecision>>,
}

#[async_trait]
impl ApprovalRequester for Prompter {
    async fn request(
        &self,
        request: ApprovalRequest,
        _: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError> {
        self.requests.lock().unwrap().push(request);
        Ok(self
            .decision
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(ApprovalDecision::Deny {
                reason: Some("Fixture refusal".into()),
            }))
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    cwd: PathBuf,
    home: PathBuf,
    registry: Registry,
    prompter: Arc<Prompter>,
    entries: Arc<Mutex<Vec<CustomSessionEntry>>>,
}

impl Fixture {
    fn new(rules: Option<Value>) -> Self {
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/permissions-tests");
        std::fs::create_dir_all(&directory).unwrap();
        let root = tempfile::tempdir_in(directory).unwrap();
        let cwd = root.path().canonicalize().unwrap();
        let home = cwd.join("home");
        std::fs::create_dir(&home).unwrap();
        let prompter = Arc::new(Prompter::default());
        let module = PermissionsModule::with_requester(cwd.clone(), home.clone(), prompter.clone());
        let mut config = ModuleConfig::default();
        config.permissions.rules = rules.map(|v| serde_json::from_value(v).unwrap());
        let registry = Registry::from_modules(&[&module], &config).unwrap();
        Self {
            _root: root,
            cwd,
            home,
            registry,
            prompter,
            entries: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn context(&self) -> ToolContext {
        let entries = self.entries.clone();
        ToolContext {
            invoker: None,
            branch: Default::default(),
            parent_session: None,
            cwd: self.cwd.clone(),
            cancel: CancellationToken::new(),
            updates: Arc::new(|_| {}),
            append_entry: Arc::new(move |entry| {
                entries.lock().unwrap().push(entry);
                Ok(())
            }),
            epoch: SessionEpoch(0),
            scope: ExecutionScope {
                thread_id: ThreadId("test".into()),
                turn_id: TurnId("turn".into()),
            },
            origin: Default::default(),
        }
    }
    async fn call_ctx(&self, name: &str, arguments: Value, ctx: ToolContext) -> BeforeToolCall {
        self.registry.interceptors()[0]
            .interceptor
            .before(
                &ToolCall {
                    id: "fixture".into(),
                    name: name.into(),
                    arguments,
                    opaque: None,
                },
                ctx,
            )
            .await
            .unwrap()
    }
    async fn call(&self, name: &str, arguments: Value) -> BeforeToolCall {
        self.call_ctx(name, arguments, self.context()).await
    }
    async fn bash(&self, command: &str) -> BeforeToolCall {
        self.call("bash", json!({"command": command})).await
    }
    fn asks(&self) -> usize {
        self.prompter.requests.lock().unwrap().len()
    }
}

fn allow() -> Value {
    json!({"surface":"*", "pattern":"*", "action":"allow"})
}

/// Allows everything except removal, which asks.
fn permissive() -> Option<Value> {
    Some(json!([
        allow(),
        rule("bash", "rm *", "ask"),
        rule("bash", "rm", "ask")
    ]))
}

#[test]
fn permissions_module_is_enabled_and_mandatory_by_default() {
    let f = Fixture::new(None);
    let module = PermissionsModule::headless(f.cwd, f.home, HeadlessPolicy::default());
    assert_eq!(module.id(), "permissions");
    assert!(module.default_enabled());
    assert!(module.mandatory());
}

#[tokio::test]
async fn empty_project_contributes_no_defaults_and_trusted_overrides_apply() {
    let mut f = Fixture::new(None);
    let module =
        PermissionsModule::with_requester(f.cwd.clone(), f.home.clone(), f.prompter.clone());
    let mut config = ModuleConfig::default();
    config.permissions.rules =
        Some(serde_json::from_value(json!([rule("*", "*", "deny")])).unwrap());
    f.registry = Registry::from_modules(&[&module], &config).unwrap();
    denied(f.call("write", json!({"path":"project.txt"})).await);
    assert_eq!(f.asks(), 0);
    config.permissions.project_rules =
        serde_json::from_value(json!([rule("write", "*", "allow")])).unwrap();
    f.registry = Registry::from_modules(&[&module], &config).unwrap();
    assert_eq!(
        f.call("write", json!({"path":"project.txt"})).await,
        BeforeToolCall::Allow
    );
    config.permissions.project_invalid = true;
    f.registry = Registry::from_modules(&[&module], &config).unwrap();
    denied(f.call("write", json!({"path":"project.txt"})).await);
    assert!(f.asks() > 0);
}

#[tokio::test]
async fn direction_overrides_sugar_independent_of_text_order_and_edit_checks_both() {
    for rules in [
        json!([
            allow(),
            rule("path", "*.env", "deny"),
            rule("path_read", "*.env", "allow")
        ]),
        json!([
            allow(),
            rule("path_read", "*.env", "allow"),
            rule("path", "*.env", "deny")
        ]),
    ] {
        let f = Fixture::new(Some(rules));
        assert_eq!(
            f.call("read", json!({"path":"private.env"})).await,
            BeforeToolCall::Allow
        );
        denied(f.call("write", json!({"path":"private.env"})).await);
        denied(f.call("edit", json!({"path":"private.env"})).await);
    }
}

struct WaitingApproval {
    ready: tokio::sync::Notify,
}

struct DeferredApproval {
    calls: std::sync::atomic::AtomicUsize,
    ready: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl ApprovalRequester for DeferredApproval {
    async fn request(
        &self,
        _: ApprovalRequest,
        _: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError> {
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            self.ready.notify_one();
            self.release.notified().await;
            Ok(ApprovalDecision::AllowSession {
                both_directions: false,
            })
        } else {
            Ok(ApprovalDecision::Deny { reason: None })
        }
    }
}

#[tokio::test]
async fn late_approval_cannot_grant_into_a_new_epoch() {
    let f = Fixture::new(None);
    let requester = Arc::new(DeferredApproval {
        calls: std::sync::atomic::AtomicUsize::new(0),
        ready: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let module =
        PermissionsModule::with_requester(f.cwd.clone(), f.home.clone(), requester.clone());
    let mut config = ModuleConfig::default();
    config.permissions.rules = Some(vec![]);
    let registry = Registry::from_modules(&[&module], &config).unwrap();
    let interceptor = registry.interceptors()[0].interceptor.clone();
    let call = ToolCall {
        id: "old".into(),
        name: "unfamiliar".into(),
        arguments: json!({}),
        opaque: None,
    };
    let old_call = call.clone();
    let old_ctx = f.context();
    let old_interceptor = interceptor.clone();
    let old = tokio::spawn(async move { old_interceptor.before(&old_call, old_ctx).await });
    requester.ready.notified().await;
    let mut ctx = f.context();
    ctx.epoch = SessionEpoch(1);
    denied(interceptor.before(&call, ctx.clone()).await.unwrap());
    requester.release.notify_one();
    assert!(
        old.await
            .unwrap()
            .unwrap_err()
            .0
            .contains("session changed")
    );
    denied(interceptor.before(&call, ctx).await.unwrap());
    assert_eq!(requester.calls.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[async_trait]
impl ApprovalRequester for WaitingApproval {
    async fn request(
        &self,
        _: ApprovalRequest,
        cancel: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError> {
        self.ready.notify_one();
        cancel.cancelled().await;
        Ok(ApprovalDecision::AllowSession {
            both_directions: true,
        })
    }
}

#[tokio::test]
async fn cancellation_releases_a_pending_approval_and_does_not_grant() {
    let mut f = Fixture::new(Some(json!([])));
    let requester = Arc::new(WaitingApproval {
        ready: tokio::sync::Notify::new(),
    });
    let module =
        PermissionsModule::with_requester(f.cwd.clone(), f.home.clone(), requester.clone());
    let mut config = ModuleConfig::default();
    config.permissions.rules = Some(vec![]);
    f.registry = Registry::from_modules(&[&module], &config).unwrap();
    let ctx = f.context();
    let cancel = ctx.cancel.clone();
    let pending = f.call_ctx("unfamiliar", json!({}), ctx);
    let interrupt = async {
        requester.ready.notified().await;
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(pending, interrupt);
    denied(result);
    assert!(
        f.entries
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry.data["outcome"] == "denied")
    );
}

#[tokio::test]
async fn shell_expansion_limit_fails_closed_even_in_yolo_and_records_failure() {
    let mut f = Fixture::new(None);
    let module =
        PermissionsModule::headless(f.cwd.clone(), f.home.clone(), HeadlessPolicy::Unavailable)
            .with_mode(PermissionMode::Yolo);
    f.registry = Registry::from_modules(&[&module], &ModuleConfig::default()).unwrap();
    let command = "x".repeat(256 * 1024 + 1);
    let call = ToolCall {
        id: "limit".into(),
        name: "bash".into(),
        arguments: json!({"command":command}),
        opaque: None,
    };
    let error = f.registry.interceptors()[0]
        .interceptor
        .before(&call, f.context())
        .await
        .unwrap_err();
    assert!(error.0.contains("limit"));
    assert_eq!(f.entries.lock().unwrap()[0].data["source"], "gate_error");
}
fn rule(surface: &str, pattern: &str, action: &str) -> Value {
    json!({"surface":surface,"pattern":pattern,"action":action})
}
fn denied(result: BeforeToolCall) -> String {
    match result {
        BeforeToolCall::Deny(reason) => reason,
        other => panic!("expected denial, got {other:?}"),
    }
}

#[tokio::test]
async fn defaults_allow_reads_and_read_only_commands_and_empty_rules_ask() {
    let f = Fixture::new(None);
    for (tool, arguments) in [
        ("read", json!({"path":"fixture.txt"})),
        ("grep", json!({"pattern":"x"})),
        ("tool_search", json!({"query":"x"})),
        ("mcp", json!({})),
    ] {
        assert_eq!(
            f.call(tool, arguments).await,
            BeforeToolCall::Allow,
            "{tool}"
        );
    }
    for command in [
        "git status",
        "cat src/lib.rs | grep needle 2>/dev/null",
        "rg -n needle src && git diff --stat",
        "pwd; ls -la src",
    ] {
        assert_eq!(f.bash(command).await, BeforeToolCall::Allow, "{command}");
    }
    assert_eq!(f.asks(), 0);
    for command in [
        "command git status",
        "cargo test",
        "echo x > out.txt",
        "PAGER=sh git log",
        "sort -o out names",
        "rg --pre ./script needle",
        "git diff --output=patch",
        "cat file | sh",
    ] {
        denied(f.bash(command).await);
    }
    assert_eq!(f.asks(), 8);
    denied(f.call("write", json!({"path":"fixture.txt"})).await);
    denied(
        f.call(
            "read",
            json!({"path":f.cwd.parent().unwrap().join("outside")}),
        )
        .await,
    );
    assert_eq!(f.asks(), 10);
    let f = Fixture::new(Some(json!([])));
    denied(f.call("write", json!({"path":"fixture.txt"})).await);
    assert_eq!(f.asks(), 1);
}

#[tokio::test]
async fn unknown_asks_and_policy_denial_explain_without_prompting() {
    let f = Fixture::new(Some(json!([])));
    denied(
        f.call("unfamiliar", json!({"value":"sensitive raw input"}))
            .await,
    );
    assert_eq!(f.asks(), 1);
    let f = Fixture::new(Some(
        json!([allow(), {"surface":"write","pattern":"*","action":"deny","reason":"Read only"}]),
    ));
    let reason = denied(f.call("write", json!({"path":"never-written"})).await);
    assert_eq!(
        reason,
        format!(
            "Denied by policy: 'write' for path '{}' (rule '*'). Reason: Read only.",
            f.cwd.join("never-written").display()
        )
    );
    assert_eq!(f.asks(), 0);
    assert_eq!(f.entries.lock().unwrap()[0].data["outcome"], "denied");
}

#[tokio::test]
async fn last_matching_rule_wins_and_repeated_pattern_moves_to_end() {
    let f = Fixture::new(Some(json!([
        allow(),
        rule("bash", "*", "deny"),
        rule("bash", "git status", "allow")
    ])));
    assert_eq!(f.bash("git status").await, BeforeToolCall::Allow);
    let f = Fixture::new(Some(json!([
        allow(),
        rule("bash", "*", "deny"),
        rule("bash", "git status", "allow"),
        rule("bash", "*", "deny")
    ])));
    denied(f.bash("git status").await);
}

#[cfg(unix)]
#[tokio::test]
async fn path_denies_cover_lexical_relative_url_and_symlink_names_despite_read_allow() {
    let f = Fixture::new(Some(json!([allow(), rule("path", "*.env", "deny")])));
    let secret = f.cwd.join("private.env");
    std::fs::write(&secret, "fixture").unwrap();
    let alias = f.cwd.join("visible.txt");
    std::os::unix::fs::symlink(&secret, &alias).unwrap();
    for path in [
        secret.to_str().unwrap().into(),
        "private.env".into(),
        url_string(&secret),
        alias.to_str().unwrap().into(),
    ] {
        denied(f.call("read", json!({"path":path})).await);
    }
    assert_eq!(f.asks(), 0);
}

#[cfg(unix)]
fn url_string(path: &std::path::Path) -> String {
    format!("file://{}", path.display())
}

#[tokio::test]
async fn hard_path_deny_precedes_external_directory_ask() {
    let f = Fixture::new(Some(json!([
        allow(),
        rule("external_directory", "*", "ask"),
        rule("path", "*.env", "deny")
    ])));
    denied(
        f.call(
            "read",
            json!({"path":f.cwd.parent().unwrap().join("outside.env")}),
        )
        .await,
    );
    assert_eq!(f.asks(), 0);
}

#[tokio::test]
async fn compound_commands_and_redirect_write_paths_are_checked() {
    let f = Fixture::new(Some(json!([allow(), rule("bash", "rm *", "deny")])));
    denied(f.bash("git status && rm -rf unwanted").await);
    let f = Fixture::new(Some(json!([allow(), rule("path_write", "*.env", "deny")])));
    denied(f.bash("printf harmless > private.env").await);
    assert_eq!(f.bash("cat private.env").await, BeforeToolCall::Allow);
}

#[tokio::test]
async fn catch_all_allow_has_no_synthetic_wrapper_or_parse_prompts() {
    let f = Fixture::new(permissive());
    for command in [
        "git status",
        "bash -c 'printf opaque'",
        "zsh -lc 'git status && printf done'",
        "sudo touch temporary",
        "env TC_TEST=yes npm test",
        "env bash -c 'printf ok'",
        "xargs printf",
        "find . -exec printf '%s' {} \\;",
        "printf '%s' 'rm -rf is just text'",
        "printf '%s' '/bin/rm -rf is just text'",
        "bash -c 'printf \"/bin/rm -rf is just text\"'",
        "python3 -c 'print(42)'",
        "printf x > /tmp/tc-permission-fixture",
        "cat <<'EOF'\nunfinished",
    ] {
        assert_eq!(f.bash(command).await, BeforeToolCall::Allow, "{command}");
    }
    for tool in ["read", "write", "edit"] {
        assert_eq!(
            f.call(tool, json!({"path":"/tmp/tc-permission-outside.txt"}))
                .await,
            BeforeToolCall::Allow
        );
    }
    assert_eq!(f.asks(), 0);
}

#[tokio::test]
async fn rm_asks_in_chains_substitutions_paths_quotes_escapes_and_wrappers() {
    for command in [
        "rm -rf temporary",
        "rm -fr temporary",
        "rm -r -f temporary",
        "/bin/rm -rf temporary",
        "git status && rm temporary",
        "printf '%s' $(rm temporary)",
        "sudo rm -rf temporary",
        "env /bin/rm -rf temporary",
        "bash -c 'printf ok; rm -rf temporary'",
        "env bash -c 'rm -rf temporary'",
        "find . -exec rm -f {} \\;",
        "printf temporary | xargs rm -f",
        "'rm' -rf temporary",
        "\"/bin/rm\" -rf temporary",
        "r\\m -rf temporary",
        "rm\t-rf temporary",
        "rm",
        "eval 'printf ok;' 'rm -rf temporary'",
        "eval rm -rf temporary",
        "bash -c \"rm -rf temporary\"",
        "bash -c 'bash -c \"rm -rf temporary\"'",
        "'sudo' rm -rf temporary",
        "env '/bin/bash' -c 'rm -rf temporary'",
        "'bash' -c 'rm -rf temporary'",
    ] {
        let f = Fixture::new(permissive());
        denied(f.bash(command).await);
        assert_eq!(f.asks(), 1, "{command}");
        assert!(
            f.prompter.requests.lock().unwrap()[0]
                .matched_pattern
                .as_deref()
                .unwrap()
                .contains("rm"),
            "{command}"
        );
    }
}

#[tokio::test]
async fn command_and_exec_inspect_operands_and_skip_lookup_or_argv_zero() {
    for command in [
        "command rm -rf temporary",
        "command -- rm -rf temporary",
        "command -p -- /bin/rm -rf temporary",
        "'command' '-p' 'rm' temporary",
        "exec rm temporary",
        "exec -- rm temporary",
        "exec -cl -a 'display name' rm temporary",
        "exec -aalias rm temporary",
        "exec -cla alias rm temporary",
        "command exec -a alias command -- rm temporary",
        "env command rm temporary",
        "command sh -c 'rm temporary'",
        "exec bash -c 'command rm temporary'",
    ] {
        let f = Fixture::new(permissive());
        denied(f.bash(command).await);
        assert_eq!(f.asks(), 1, "{command}");
        let f = Fixture::new(Some(json!([allow(), rule("bash", "rm *", "deny")])));
        denied(f.bash(command).await);
        assert_eq!(f.asks(), 0, "{command}");
    }
    let f = Fixture::new(permissive());
    for command in [
        "command -v rm",
        "command -V rm",
        "command -pv rm",
        "command -pV -- rm",
        "command -p git status",
        "command printf '%s' 'rm -rf temporary'",
        "exec printf rm",
        "exec -a rm printf hello",
        "exec -arm printf hello",
        "exec > log.txt",
    ] {
        assert_eq!(f.bash(command).await, BeforeToolCall::Allow, "{command}");
    }
    assert_eq!(f.asks(), 0);
}

#[tokio::test]
async fn wrapped_programs_keep_path_denies_and_external_checks() {
    for command in [
        "bash -c 'cat private.env'",
        "env sh -c 'printf x > private.env'",
        "command cat private.env",
        "exec sh -c 'cat private.env'",
    ] {
        let f = Fixture::new(Some(json!([allow(), rule("path", "*.env", "deny")])));
        denied(f.bash(command).await);
        assert_eq!(f.asks(), 0, "{command}");
    }
    let f = Fixture::new(Some(json!([
        allow(),
        rule("external_directory", "*", "deny")
    ])));
    denied(f.bash("bash -c 'cat /tmp/tc-permission-outside.txt'").await);
}

#[tokio::test]
async fn explicit_shell_asks_and_denies_survive_permissive_wrappers() {
    let f = Fixture::new(Some(json!([
        allow(),
        rule("bash", "git push *", "ask"),
        rule("bash", "rm *", "deny")
    ])));
    denied(f.bash("bash -c 'git push origin main'").await);
    assert_eq!(f.asks(), 1);
    denied(f.bash("sudo bash -c 'rm -rf temporary'").await);
    assert_eq!(f.asks(), 1);
    let f = Fixture::new(Some(json!([])));
    denied(f.bash("bash -c 'printf opaque'").await);
    assert_eq!(f.asks(), 1);
}

#[tokio::test]
async fn mcp_and_shell_aliases_use_the_same_pipeline() {
    let f = Fixture::new(Some(json!([
        allow(),
        rule("mcp", "example:*", "deny"),
        rule("bash", "rm *", "deny")
    ])));
    denied(
        f.call("mcp", json!({"server":"example","tool":"erase"}))
            .await,
    );
    denied(f.call("mcp__example__erase", json!({})).await);
    denied(
        f.call("exec_command", json!({"cmd":"rm -rf temporary"}))
            .await,
    );
    assert_eq!(f.asks(), 0);
    let f = Fixture::new(Some(json!([allow(), rule("mcp__example__*", "*", "deny")])));
    denied(
        f.call("mcp__example__erase", json!({"server":"innocent"}))
            .await,
    );
    let f = Fixture::new(Some(json!([
        rule("mcp", "example:*", "deny"),
        rule("mcp", "mcp__example__read", "allow")
    ])));
    assert_eq!(
        f.call("mcp__example__read", json!({})).await,
        BeforeToolCall::Allow
    );
    denied(f.call("mcp", json!({"tool":"example:erase"})).await);
    denied(
        f.call("mcp", json!({"server":"example","describe":"erase"}))
            .await,
    );
    assert_eq!(f.call("mcp", json!({})).await, BeforeToolCall::Allow);
}

#[tokio::test]
async fn mcp_path_arguments_and_sanitized_server_names_cannot_bypass_rules() {
    let f = Fixture::new(Some(json!([allow(), rule("path", "*.env", "deny")])));
    denied(
        f.call("mcp__example__read", json!({"path":"private.env"}))
            .await,
    );
    denied(
        f.call(
            "mcp",
            json!({"server":"example","tool":"read","arguments":{"path":"private.env"}}),
        )
        .await,
    );
    let mut f = Fixture::new(None);
    let module =
        PermissionsModule::with_requester(f.cwd.clone(), f.home.clone(), f.prompter.clone());
    let mut config = ModuleConfig::default();
    config.permissions.rules = Some(
        serde_json::from_value(json!([allow(), rule("mcp", "danger-srv:*", "deny")])).unwrap(),
    );
    config.mcp_servers.insert(
        "danger-srv".into(),
        tiechui_plugin_api::McpServerConfig::default(),
    );
    f.registry = Registry::from_modules(&[&module], &config).unwrap();
    denied(f.call("mcp__danger_srv__erase", json!({})).await);
    assert_eq!(f.asks(), 0);
}

#[tokio::test]
async fn parent_boundary_survives_child_cwd_for_file_and_redirect() {
    let f = Fixture::new(Some(json!([
        allow(),
        rule("external_directory_write", "*", "deny")
    ])));
    let child = f.cwd.parent().unwrap().to_owned();
    let mut ctx = f.context();
    ctx.cwd = child;
    denied(
        f.call_ctx("write", json!({"path":"child.txt"}), ctx.clone())
            .await,
    );
    denied(
        f.call_ctx("bash", json!({"command":"printf x > child.txt"}), ctx)
            .await,
    );
}

#[tokio::test]
async fn session_approval_keeps_direction_and_epoch_changes_clear_grants() {
    let f = Fixture::new(Some(json!([
        allow(),
        rule("external_directory", "*", "ask")
    ])));
    *f.prompter.decision.lock().unwrap() = Some(ApprovalDecision::AllowSession {
        both_directions: false,
    });
    let outside = f.cwd.parent().unwrap().join("permission-direction");
    assert_eq!(
        f.call("read", json!({"path":outside.join("a.txt")})).await,
        BeforeToolCall::Allow
    );
    let first = f.asks();
    assert!(first > 0);
    assert_eq!(
        f.call("read", json!({"path":outside.join("b.txt")})).await,
        BeforeToolCall::Allow
    );
    assert_eq!(f.asks(), first);
    assert_eq!(
        f.call("write", json!({"path":outside.join("c.txt")})).await,
        BeforeToolCall::Allow
    );
    assert!(f.asks() > first);
    let before = f.asks();
    let mut ctx = f.context();
    ctx.epoch = SessionEpoch(1);
    assert_eq!(
        f.call_ctx("read", json!({"path":outside.join("a.txt")}), ctx)
            .await,
        BeforeToolCall::Allow
    );
    assert!(f.asks() > before);
}

#[tokio::test]
async fn session_grant_for_a_wrapper_does_not_cover_an_asking_nested_command() {
    let f = Fixture::new(Some(json!([
        allow(),
        rule("bash", "bash *", "ask"),
        rule("bash", "rm *", "ask")
    ])));
    *f.prompter.decision.lock().unwrap() = Some(ApprovalDecision::AllowSession {
        both_directions: false,
    });
    assert_eq!(
        f.bash("bash -c 'printf hello'").await,
        BeforeToolCall::Allow
    );
    assert_eq!(f.asks(), 1);
    *f.prompter.decision.lock().unwrap() = None;
    denied(f.bash("bash -c 'rm secret'").await);
    assert_eq!(f.asks(), 2);
    assert_eq!(
        f.prompter.requests.lock().unwrap()[1]
            .matched_pattern
            .as_deref(),
        Some("rm *")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_cwd_keeps_its_files_inside_the_workspace() {
    let f = Fixture::new(None);
    std::fs::write(f.cwd.join("a.txt"), "a").unwrap();
    let links = tempfile::tempdir_in(f.cwd.parent().unwrap()).unwrap();
    let link = links.path().join("work");
    std::os::unix::fs::symlink(&f.cwd, &link).unwrap();
    let mut ctx = f.context();
    ctx.cwd = link.clone();
    assert_eq!(
        f.call_ctx("read", json!({"path":"a.txt"}), ctx.clone())
            .await,
        BeforeToolCall::Allow
    );
    assert_eq!(f.asks(), 0);
    denied(
        f.call_ctx("read", json!({"path":links.path().join("b.txt")}), ctx)
            .await,
    );
    assert_eq!(f.asks(), 1);
}

#[tokio::test]
async fn edit_combines_a_read_session_grant_with_a_configured_write_allow() {
    let f = Fixture::new(Some(json!([
        allow(),
        rule("external_directory_read", "*", "ask")
    ])));
    *f.prompter.decision.lock().unwrap() = Some(ApprovalDecision::AllowSession {
        both_directions: false,
    });
    let outside = f.cwd.parent().unwrap().join("mixed-direction.txt");
    assert_eq!(
        f.call("read", json!({"path":outside})).await,
        BeforeToolCall::Allow
    );
    assert_eq!(f.asks(), 1);
    *f.prompter.decision.lock().unwrap() = None;
    assert_eq!(
        f.call("edit", json!({"path":outside})).await,
        BeforeToolCall::Allow
    );
    assert_eq!(f.asks(), 1);
}

#[tokio::test]
async fn headless_modes_preserve_denies_and_default_never_prompts() {
    for (policy, mode) in [
        (HeadlessPolicy::Deny, PermissionMode::Yolo),
        (HeadlessPolicy::Unavailable, PermissionMode::Rules),
        (HeadlessPolicy::Unavailable, PermissionMode::Yolo),
    ] {
        let mut f = Fixture::new(None);
        let module =
            PermissionsModule::headless(f.cwd.clone(), f.home.clone(), policy).with_mode(mode);
        f.registry = Registry::from_modules(&[&module], &ModuleConfig::default()).unwrap();
        if policy == HeadlessPolicy::Unavailable && mode == PermissionMode::Yolo {
            assert_eq!(f.bash("rm temporary").await, BeforeToolCall::Allow);
        } else {
            let text = denied(f.bash("rm temporary").await);
            assert!(text.contains(if policy == HeadlessPolicy::Deny {
                "Denied by policy"
            } else {
                "no interactive UI"
            }));
        }
        let mut config = ModuleConfig::default();
        config.permissions.rules =
            Some(serde_json::from_value(json!([rule("bash", "rm *", "deny")])).unwrap());
        f.registry = Registry::from_modules(&[&module], &config).unwrap();
        denied(f.bash("rm temporary").await);
    }
}

#[tokio::test]
async fn audit_keeps_scopes_and_outcomes_without_raw_arguments() {
    let f = Fixture::new(Some(json!([allow(), rule("bash", "rm *", "ask")])));
    denied(f.bash("rm sensitive-path-name").await);
    let entries = f.entries.lock().unwrap();
    assert!(
        entries
            .iter()
            .all(|e| e.kind == tiechui_module_permissions::ENTRY_KIND)
    );
    let text = serde_json::to_string(&entries.iter().map(|e| &e.data).collect::<Vec<_>>()).unwrap();
    assert!(!text.contains("sensitive-path-name"));
    assert!(text.contains("denied"));
    assert!(text.contains("rm *"));
}

#[tokio::test]
async fn child_grants_are_private_and_parent_policy_sources_survive_cwd_changes() {
    let f = Fixture::new(Some(json!([
        allow(),
        rule("write", "*", "ask"),
        rule("read", "*secret*", "deny")
    ])));
    let module =
        PermissionsModule::with_requester(f.cwd.clone(), f.home.clone(), f.prompter.clone());
    let mut config = ModuleConfig::default();
    config.permissions.rules = Some(
        serde_json::from_value(json!([
            allow(),
            rule("write", "*", "ask"),
            rule("read", "*secret*", "deny")
        ]))
        .unwrap(),
    );
    let mut first = Registry::default();
    let mut second = Registry::default();
    module
        .register_child(&mut first, &config, &f.home, true, "worker")
        .unwrap();
    module
        .register_child(&mut second, &config, &f.home, true, "worker")
        .unwrap();
    let mut ctx = f.context();
    ctx.cwd = f.cwd.join("nested");
    std::fs::create_dir(&ctx.cwd).unwrap();
    // This directory's policy is deliberately irrelevant to the child's policy sources.
    std::fs::create_dir(ctx.cwd.join(".agents")).unwrap();
    std::fs::write(
        ctx.cwd.join(".agents/config.toml"),
        "[[permissions.rules]]\nsurface='*'\npattern='*'\naction='allow'\n",
    )
    .unwrap();
    ctx.scope.thread_id = ThreadId("child-1".into());
    let write = ToolCall {
        id: "write".into(),
        name: "write".into(),
        arguments: json!({"path":"new.txt","content":"data"}),
        opaque: None,
    };
    *f.prompter.decision.lock().unwrap() = Some(ApprovalDecision::AllowSession {
        both_directions: false,
    });
    assert_eq!(
        first.interceptors()[0]
            .interceptor
            .before(&write, ctx.clone())
            .await
            .unwrap(),
        BeforeToolCall::Allow
    );
    let asked = f.asks();
    assert_eq!(
        first.interceptors()[0]
            .interceptor
            .before(&write, ctx.clone())
            .await
            .unwrap(),
        BeforeToolCall::Allow
    );
    assert_eq!(f.asks(), asked);
    *f.prompter.decision.lock().unwrap() = Some(ApprovalDecision::Deny { reason: None });
    ctx.scope.thread_id = ThreadId("child-2".into());
    denied(
        second.interceptors()[0]
            .interceptor
            .before(&write, ctx.clone())
            .await
            .unwrap(),
    );
    denied(
        f.call("write", json!({"path":"nested/new.txt","content":"data"}))
            .await,
    );
    let read = ToolCall {
        id: "read".into(),
        name: "read".into(),
        arguments: json!({"path":"secret.txt"}),
        opaque: None,
    };
    denied(
        second.interceptors()[0]
            .interceptor
            .before(&read, ctx)
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn agent_frontmatter_policy_uses_parent_trust_and_rejects_traversal() {
    let f = Fixture::new(None);
    std::fs::create_dir_all(f.home.join("agents")).unwrap();
    std::fs::create_dir_all(f.cwd.join(".agents/agents")).unwrap();
    std::fs::write(
        f.home.join("agents/worker.md"),
        "---\npermission:\n  write: deny\n---\n",
    )
    .unwrap();
    std::fs::write(
        f.cwd.join(".agents/agents/worker.md"),
        "---\npermission:\n  read: deny\n---\n",
    )
    .unwrap();
    let module =
        PermissionsModule::with_requester(f.cwd.clone(), f.home.clone(), f.prompter.clone());
    for trusted in [false, true] {
        let mut registry = Registry::default();
        module
            .register_child(
                &mut registry,
                &ModuleConfig::default(),
                &f.home,
                trusted,
                "worker",
            )
            .unwrap();
        let call = |name: &str| ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: json!({"path":"file.txt","content":"x"}),
            opaque: None,
        };
        denied(
            registry.interceptors()[0]
                .interceptor
                .before(&call("write"), f.context())
                .await
                .unwrap(),
        );
        let read = registry.interceptors()[0]
            .interceptor
            .before(&call("read"), f.context())
            .await
            .unwrap();
        if trusted {
            denied(read);
        } else {
            assert_eq!(read, BeforeToolCall::Allow);
        }
    }
    assert!(
        module
            .register_child(
                &mut Registry::default(),
                &ModuleConfig::default(),
                &f.home,
                true,
                "../escape"
            )
            .is_err()
    );
}

#[tokio::test]
async fn diagnostics_show_rules_mode_session_grants_and_only_instance_decisions() {
    let f = Fixture::new(permissive());
    let command = f
        .registry
        .commands()
        .iter()
        .find(|c| c.info().name == "permissions")
        .unwrap();
    assert!(command.interactive());
    assert!(command.allows_busy(""));
    let before = command.run("", CancellationToken::new()).await.unwrap();
    assert!(before.starts_with("Mode: rules\n"));
    assert!(before.contains("bash · rm * · Ask · user"));
    assert!(before.ends_with("Recent decisions (this instance):\nNone"));
    *f.prompter.decision.lock().unwrap() = Some(ApprovalDecision::AllowSession {
        both_directions: false,
    });
    f.bash("rm private-filename").await;
    let text = command.run("", CancellationToken::new()).await.unwrap();
    assert!(
        text.contains("Temporary session grants:\nbash · rm * · Allow"),
        "{text}"
    );
    assert!(text.contains("approved_for_session · bash · rm * · approval"));
    assert!(!text.contains("private-filename"));
    let fresh = Fixture::new(None);
    let fresh = fresh.registry.commands()[0]
        .run("", CancellationToken::new())
        .await
        .unwrap();
    assert!(!fresh.contains("approved_for_session"));
    assert_eq!(
        command.run("auto", CancellationToken::new()).await.unwrap(),
        "Permission mode: auto."
    );
    let text = command.run("", CancellationToken::new()).await.unwrap();
    assert!(text.starts_with("Mode: auto\nClassifier blocks: 0 in a row, 0 total\n"));
    assert!(text.contains("Temporary session grants:\nbash · rm * · Allow"));
    assert!(
        command
            .run("sideways", CancellationToken::new())
            .await
            .unwrap()
            .starts_with("Usage:")
    );
}

struct Model {
    answers: Mutex<Vec<&'static str>>,
    requests: Mutex<Vec<String>>,
}

#[async_trait]
impl tiechui_plugin_api::ToolInvoker for Model {
    fn get(&self, _: &str) -> Option<Arc<dyn tiechui_plugin_api::Tool>> {
        None
    }
    async fn invoke(&self, _: ToolCall, _: ToolContext) -> tiechui_plugin_api::ToolOutput {
        unreachable!()
    }
    fn model(&self) -> Option<tiechui_plugin_api::model::ModelId> {
        Some(tiechui_plugin_api::model::ModelId("fixture/model".into()))
    }
    async fn model_stream(
        &self,
        request: tiechui_plugin_api::model::ModelRequest,
        _: CancellationToken,
    ) -> Result<tiechui_plugin_api::ModelStream, ToolError> {
        use tiechui_plugin_api::model::{Content, StreamEvent};
        self.requests
            .lock()
            .unwrap()
            .push(serde_json::to_string(&request.messages).unwrap());
        let answer = self.answers.lock().unwrap().remove(0);
        let event = StreamEvent::ContentComplete {
            index: 0,
            content: Content::Text {
                text: answer.into(),
                opaque: None,
            },
        };
        Ok(Box::pin(tiechui_plugin_api::futures::stream::iter([Ok(
            event,
        )])))
    }
}

impl Fixture {
    fn mode(&mut self, mode: PermissionMode, config: ModuleConfig) {
        let module = PermissionsModule::with_requester(
            self.cwd.clone(),
            self.home.clone(),
            self.prompter.clone(),
        )
        .with_mode(mode);
        self.registry = Registry::from_modules(&[&module], &config).unwrap();
    }
    fn reviewed(&self, answers: Vec<&'static str>) -> (ToolContext, Arc<Model>) {
        let model = Arc::new(Model {
            answers: Mutex::new(answers),
            requests: Mutex::default(),
        });
        let mut ctx = self.context();
        ctx.invoker = Some(model.clone());
        ctx.branch = Arc::new(vec![tiechui_plugin_api::protocol::BranchEntry {
            id: "user".into(),
            parent_id: None,
            timestamp: 0,
            content: Some(vec![tiechui_plugin_api::protocol::InputContent::Text {
                text: "please run the tests".into(),
            }]),
            custom: None,
        }]);
        (ctx, model)
    }
}

fn rules(value: Value) -> ModuleConfig {
    let mut config = ModuleConfig::default();
    config.permissions.rules = Some(serde_json::from_value(value).unwrap());
    config
}

#[tokio::test]
async fn protected_paths_ask_in_every_mode_unless_user_rules_decide() {
    for mode in [
        PermissionMode::Rules,
        PermissionMode::Auto,
        PermissionMode::Yolo,
    ] {
        let mut f = Fixture::new(None);
        f.mode(mode, rules(json!([allow()])));
        for (tool, arguments) in [
            ("write", json!({"path":".git/hooks/pre-commit"})),
            ("edit", json!({"path":".ENV"})),
            ("write", json!({"path":f.home.join(".zshrc")})),
            (
                "bash",
                json!({"command":"printf x >> .claude/settings.json"}),
            ),
        ] {
            let reason = denied(f.call(tool, arguments).await);
            assert!(reason.contains("'protected_path'"), "{reason}");
        }
        assert_eq!(f.asks(), 4, "{mode:?}");
        for (tool, arguments) in [
            ("read", json!({"path":".env"})),
            ("write", json!({"path":".env.example"})),
            ("write", json!({"path":".claude/worktrees/a/src/lib.rs"})),
            ("bash", json!({"command":"cat .git/config"})),
        ] {
            // Auto ignores the catch-all allow for bash and has no classifier here.
            if mode == PermissionMode::Auto && tool == "bash" {
                continue;
            }
            assert_eq!(f.call(tool, arguments).await, BeforeToolCall::Allow);
        }
    }
    let mut f = Fixture::new(None);
    f.mode(
        PermissionMode::Yolo,
        rules(json!([allow(), rule("protected_path", "*.env", "allow")])),
    );
    assert_eq!(
        f.call("write", json!({"path":".env"})).await,
        BeforeToolCall::Allow
    );
    let mut config = rules(json!([allow()]));
    config.permissions.project_rules =
        serde_json::from_value(json!([rule("protected_path", "*", "allow")])).unwrap();
    f.mode(PermissionMode::Yolo, config);
    denied(f.call("write", json!({"path":".env"})).await);
    assert_eq!(f.asks(), 1);
}

#[tokio::test]
async fn yolo_approves_asks_after_reviewers_but_not_fail_closed_rules() {
    struct Reviewer;
    #[async_trait]
    impl tiechui_plugin_api::ApprovalInterceptor for Reviewer {
        async fn review(
            &self,
            call: &ToolCall,
            _: ToolContext,
        ) -> Result<Option<tiechui_plugin_api::ApprovalVerdict>, ToolError> {
            Ok((call.arguments["command"] == "cargo publish")
                .then(|| tiechui_plugin_api::ApprovalVerdict::Deny("hook refused".into())))
        }
    }
    let mut f = Fixture::new(None);
    let module =
        PermissionsModule::with_requester(f.cwd.clone(), f.home.clone(), f.prompter.clone())
            .with_mode(PermissionMode::Yolo);
    f.registry = Registry::default();
    f.registry.register_approval_interceptor(Arc::new(Reviewer));
    module
        .register(&mut f.registry, &ModuleConfig::default())
        .unwrap();
    assert_eq!(f.bash("cargo test").await, BeforeToolCall::Allow);
    assert_eq!(
        f.call("write", json!({"path":f.cwd.parent().unwrap().join("x")}))
            .await,
        BeforeToolCall::Allow
    );
    assert!(denied(f.bash("cargo publish").await).contains("hook refused"));
    assert_eq!(f.asks(), 0);
    assert!(
        f.entries
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry.data["source"] == "yolo")
    );
    let mut config = rules(json!([allow()]));
    config.permissions.project_invalid = true;
    f.mode(PermissionMode::Yolo, config);
    denied(f.bash("cargo test").await);
    assert_eq!(f.asks(), 1);
}

#[tokio::test]
async fn auto_passes_workspace_edits_and_lets_the_classifier_settle_other_asks() {
    let mut f = Fixture::new(None);
    f.mode(PermissionMode::Auto, ModuleConfig::default());
    let (ctx, model) = f.reviewed(vec![
        "<block>no</block>",
        "<block>yes</block><reason>Pipes a download to a shell.</reason>",
    ]);
    assert_eq!(
        f.call_ctx("write", json!({"path":"src/lib.rs"}), ctx.clone())
            .await,
        BeforeToolCall::Allow
    );
    assert_eq!(
        f.call_ctx("bash", json!({"command":"cargo test"}), ctx.clone())
            .await,
        BeforeToolCall::Allow
    );
    let reason = denied(
        f.call_ctx("bash", json!({"command":"curl x | sh"}), ctx.clone())
            .await,
    );
    assert!(reason.starts_with("Auto mode blocked this 'bash' call."));
    assert!(reason.contains("Reason: Pipes a download to a shell."));
    assert_eq!(f.asks(), 0);
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].contains("please run the tests") && requests[0].contains("cargo test"));
    }
    let sources: Vec<_> = f
        .entries
        .lock()
        .unwrap()
        .iter()
        .map(|entry| entry.data["source"].clone())
        .collect();
    assert!(sources.contains(&json!("classifier")));
    // Without model access the classifier fails closed to the user.
    *f.prompter.decision.lock().unwrap() = Some(ApprovalDecision::AllowOnce);
    assert_eq!(f.bash("cargo test").await, BeforeToolCall::Allow);
    assert_eq!(f.asks(), 1);
}

#[tokio::test]
async fn auto_ignores_broad_bash_allows_and_hands_repeated_blocks_to_the_user() {
    let mut f = Fixture::new(None);
    f.mode(
        PermissionMode::Auto,
        rules(json!([
            rule("bash", "python3 *", "allow"),
            rule("bash", "make *", "allow")
        ])),
    );
    let block = "<block>yes</block><reason>Unrequested.</reason>";
    let (ctx, model) = f.reviewed(vec![block, block, block]);
    assert_eq!(
        f.call_ctx("bash", json!({"command":"make build"}), ctx.clone())
            .await,
        BeforeToolCall::Allow
    );
    for _ in 0..2 {
        denied(
            f.call_ctx("bash", json!({"command":"python3 x.py"}), ctx.clone())
                .await,
        );
    }
    assert_eq!(f.asks(), 0);
    denied(
        f.call_ctx("bash", json!({"command":"python3 x.py"}), ctx.clone())
            .await,
    );
    assert_eq!(f.asks(), 1);
    assert_eq!(model.requests.lock().unwrap().len(), 3);
}

#[test]
fn the_main_session_reports_its_mode_on_restore_and_change_but_children_stay_quiet() {
    use tiechui_plugin_api::{BranchView, RuntimeEvent, protocol::PERMISSION_MODE_EVENT};
    let f = Fixture::new(permissive());
    let module =
        PermissionsModule::with_requester(f.cwd.clone(), f.home.clone(), f.prompter.clone())
            .with_mode(PermissionMode::Auto);
    let registry = Registry::from_modules(&[&module], &ModuleConfig::default()).unwrap();
    let [feed] = registry.runtime_extensions() else {
        panic!("one runtime extension");
    };
    let branch = BranchView {
        session_id: "s".into(),
        leaf: "l".into(),
        cwd: f.cwd.clone(),
        parent_session: None,
        entries: vec![],
    };
    let events = |event| feed.event(&event, &branch).unwrap().events;
    let restore = || RuntimeEvent::Restore { navigation: false };
    assert_eq!(
        events(restore()),
        [(PERMISSION_MODE_EVENT.into(), json!("auto"))]
    );
    assert!(events(RuntimeEvent::Tick).is_empty());
    module.mode().set(PermissionMode::Yolo);
    assert_eq!(
        events(RuntimeEvent::Tick),
        [(PERMISSION_MODE_EVENT.into(), json!("yolo"))]
    );
    assert_eq!(
        events(restore()).len(),
        1,
        "a restored client hears it again"
    );
    let mut child = Registry::default();
    module
        .register_child(
            &mut child,
            &ModuleConfig::default(),
            &f.home,
            true,
            "worker",
        )
        .unwrap();
    assert!(child.runtime_extensions().is_empty());
}
