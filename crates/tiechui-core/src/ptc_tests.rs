use super::*;
use tiechui_module_ptc::PtcModule;
use tiechui_protocol::{ProgramCallState, ProgramRun};

fn program(code: &str) -> ToolCall {
    call_tool("program", "run_code", json!({"code":code}))
}
fn details(output: &ToolOutput) -> ProgramRun {
    serde_json::from_value(output.details.clone().unwrap()).unwrap()
}
async fn execute(registry: Arc<Registry>, call: ToolCall, cwd: &std::path::Path) -> ToolOutput {
    let scheduler = Arc::new(ToolScheduler::default());
    let mut ctx = context(registry.clone(), scheduler.clone());
    ctx.cwd = cwd.to_owned();
    tokio::time::timeout(
        Duration::from_secs(8),
        interception::execute_call(&registry, &scheduler, call, ctx),
    )
    .await
    .expect("tool call must settle")
}
fn register(registry: &mut Registry) {
    PtcModule
        .register(registry, &ModuleConfig::default())
        .unwrap();
}

#[tokio::test]
async fn sample_program_aggregates_real_temporary_files_and_keeps_builtin_definitions() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.rs"), "// TODO: first\nfn a() {}\n").unwrap();
    std::fs::write(root.path().join("b.rs"), "fn b() {}\n").unwrap();
    std::fs::write(root.path().join("c.rs"), "// TODO: third\n").unwrap();
    let mut registry = Registry::default();
    tiechui_module_editing::EditingModule
        .register_child(&mut registry, &ModuleConfig::default())
        .unwrap();
    registry
        .register_tool(Arc::new(tiechui_tools::BashTool::default()))
        .unwrap();
    let before = registry.tools().specs();
    register(&mut registry);
    assert!(
        before
            .iter()
            .all(|spec| registry.tools().specs().contains(spec))
    );
    for name in ["read", "grep", "find", "ls", "edit", "write", "bash"] {
        assert!(registry.tools().get(name).unwrap().programmatic(), "{name}");
    }
    let result = execute(
        Arc::new(registry),
        program(
            r#"
const files = (await tools.find({pattern:"*.rs"})).split("\n").filter(Boolean);
const hits = await Promise.all(files.map(async path => {
  const text = await tools.read({path});
  const todos = text.split("\n").filter(line => line.includes("TODO"));
  return todos.length ? {path, todos} : null;
}));
return hits.filter(Boolean);
"#,
        ),
        root.path(),
    )
    .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(
        text(&result),
        "[\n  {\n    \"path\": \"a.rs\",\n    \"todos\": [\n      \"// TODO: first\"\n    ]\n  },\n  {\n    \"path\": \"c.rs\",\n    \"todos\": [\n      \"// TODO: third\"\n    ]\n  }\n]\n\ntool calls: 4 succeeded"
    );
}

struct Rewrite;
#[async_trait]
impl ToolInterceptor for Rewrite {
    fn stage(&self) -> InterceptorStage {
        InterceptorStage::BeforePolicy
    }
    async fn before(&self, call: &ToolCall, _: ToolContext) -> Result<BeforeToolCall, ToolError> {
        if call.name == "mcp__fixture__tool_0000" {
            assert_eq!(call.arguments["count"], 7);
            Ok(BeforeToolCall::Rewrite(
                json!({"count":9,"value":"rewritten"}),
            ))
        } else {
            Ok(BeforeToolCall::Allow)
        }
    }
}

#[tokio::test]
async fn deferred_mcp_has_one_approval_per_child_and_same_hooks_and_settlement_as_direct_calls() {
    let fixture = McpFixture::new(Some(1)).await;
    let cwd = std::env::current_dir().unwrap();
    let mut registry = Registry::default();
    fixture.register(&mut registry);
    register(&mut registry);
    let approvals = Arc::new(Approvals::default());
    let mut config = ModuleConfig::default();
    config.permissions.rules = Some(vec![]);
    tiechui_module_permissions::PermissionsModule::with_requester(
        cwd.clone(),
        cwd.clone(),
        approvals.clone(),
    )
    .register(&mut registry, &config)
    .unwrap();
    registry.register_tool_interceptor(Arc::new(Rewrite), Duration::from_secs(1));
    let log: HookLog = Arc::default();
    for stage in [
        InterceptorStage::BeforePolicy,
        InterceptorStage::Policy,
        InterceptorStage::AfterPolicy,
    ] {
        registry.register_tool_interceptor(
            Arc::new(Recorder {
                stage,
                log: log.clone(),
            }),
            Duration::from_secs(1),
        );
    }
    let definitions = present(&registry, 1);
    assert!(
        !definitions
            .iter()
            .any(|spec| spec.name == "mcp__fixture__tool_0000")
    );
    let registry = Arc::new(registry);
    let output = execute(
        registry.clone(),
        program("return await tools.mcp__fixture__tool_0000({count:'7',value:'original'});"),
        &cwd,
    )
    .await;
    assert!(!output.is_error, "{output:?}");
    assert_eq!(text(&output), "rewritten\n\ntool calls: 1 succeeded");
    {
        let approvals = approvals.0.lock().unwrap();
        assert_eq!(approvals.len(), 1);
        assert_eq!(approvals[0].call.id, "program.1");
        assert_eq!(approvals[0].call.name, "mcp__fixture__tool_0000");
        assert_eq!(
            approvals[0].call.arguments,
            json!({"count":9,"value":"rewritten"})
        );
    }
    let program_log = log.lock().unwrap().clone();
    for stage in [
        InterceptorStage::BeforePolicy,
        InterceptorStage::Policy,
        InterceptorStage::AfterPolicy,
    ] {
        for phase in ["before", "after", "settled"] {
            assert_eq!(
                program_log
                    .iter()
                    .filter(|entry| entry == &&(stage, phase.into(), "run_code".into()))
                    .count(),
                usize::from(stage != InterceptorStage::Policy || phase == "settled")
            );
        }
    }
    log.lock().unwrap().clear();
    let direct = execute(
        registry.clone(),
        call_tool(
            "direct",
            "mcp__fixture__tool_0000",
            json!({"count":"7","value":"original"}),
        ),
        &cwd,
    )
    .await;
    assert_eq!(text(&direct), "rewritten");
    assert_eq!(
        program_log
            .into_iter()
            .filter(|(_, _, name)| name != "run_code")
            .collect::<Vec<_>>(),
        *log.lock().unwrap()
    );
    assert_eq!(approvals.0.lock().unwrap().len(), 2);
    assert_eq!(present(&registry, 1), definitions);
    let output = execute(registry, program("await Promise.all([tools.mcp__fixture__tool_0000({count:'7'}), tools.mcp__fixture__tool_0000({count:'7'})]);"), &cwd).await;
    assert!(!output.is_error, "{output:?}");
    let approvals = approvals.0.lock().unwrap();
    assert_eq!(approvals.len(), 4);
    assert!(
        approvals
            .iter()
            .all(|request| request.call.name == "mcp__fixture__tool_0000")
    );
}

#[tokio::test]
async fn user_hooks_see_the_child_tool_and_the_wrapper_at_the_expected_stages() {
    let fixture = McpFixture::new(None).await;
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut hooks = serde_json::Map::new();
    for event in ["PreToolUse", "PostToolUse"] {
        hooks.insert(event.into(), json!(["*", "mcp__fixture__echo"].map(|matcher| {
            let path = root.path().join(format!("{event}-{}.jsonl", if matcher == "*" {"all"} else {"real"}));
            let quoted = format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
            json!({"matcher":matcher, "hooks":[{"type":"command", "command":format!("cat >> {quoted}; printf '\\n' >> {quoted}")}]})
        })));
    }
    std::fs::write(home.join("hooks.json"), json!({"hooks":hooks}).to_string()).unwrap();
    let hooks = tiechui_module_hooks::HooksModule::load(
        root.path(),
        &home,
        false,
        Default::default(),
        &CancellationToken::new(),
    )
    .unwrap();
    let mut registry = Registry::default();
    fixture.register(&mut registry);
    register(&mut registry);
    hooks
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let registry = Arc::new(registry);
    for call in [
        program("await tools.mcp__fixture__echo({value:'ok'});"),
        call_tool("direct", "mcp__fixture__echo", json!({"value":"ok"})),
    ] {
        let result = execute(registry.clone(), call, root.path()).await;
        assert!(!result.is_error, "{result:?}");
    }
    for event in ["PreToolUse", "PostToolUse"] {
        for (scope, expected) in [("all", 3), ("real", 2)] {
            let log = std::fs::read_to_string(root.path().join(format!("{event}-{scope}.jsonl")))
                .unwrap();
            let calls: Vec<serde_json::Value> = log
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(calls.len(), expected);
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call["tool_name"] == "mcp__fixture__echo")
                    .count(),
                2
            );
        }
    }
}

#[tokio::test]
async fn cancellation_settles_mcp_and_releases_exclusivity_for_a_top_level_write() {
    let fixture = McpFixture::new(None).await;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("file.txt");
    let mut registry = Registry::default();
    fixture.register(&mut registry);
    register(&mut registry);
    tiechui_module_editing::EditingModule
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let registry = Arc::new(registry);
    let scheduler = Arc::new(ToolScheduler::default());
    let ctx = context(registry.clone(), scheduler.clone());
    let cancel = ctx.cancel.clone();
    let program = interception::execute_call(
        &registry,
        &scheduler,
        program("await tools.mcp__fixture__wait({});"),
        ctx,
    );
    let write = async {
        while fixture.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        interception::execute_call(
            &registry,
            &scheduler,
            call_tool("top", "write", json!({"path":path,"content":"done"})),
            context(registry.clone(), scheduler.clone()),
        )
        .await
    };
    let interrupt = async {
        while fixture.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!path.exists(), "write must wait for the MCP call");
        cancel.cancel();
    };
    let (program, write, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(program, write, interrupt)
    })
    .await
    .expect("cancellation must release exclusive scheduling");
    assert!(program.is_error);
    assert!(!write.is_error, "{write:?}");
    assert_eq!(
        details(&program).calls[0].state,
        ProgramCallState::Cancelled
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "done");
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.cancelled.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("server must receive cancellation");
}

#[tokio::test]
async fn denied_restricted_invalid_and_control_calls_never_execute() {
    let fixture = McpFixture::new(Some(1)).await;
    let cwd = std::env::current_dir().unwrap();
    for restricted in [false, true] {
        let mut registry = Registry::default();
        fixture.register(&mut registry);
        register(&mut registry);
        let mut config = ModuleConfig::default();
        config.permissions.rules = Some(vec![tiechui_plugin_api::PermissionRule {
            surface: "mcp".into(),
            pattern: "mcp__fixture__tool_0000".into(),
            action: tiechui_plugin_api::PermissionAction::Deny,
            reason: None,
        }]);
        tiechui_module_permissions::PermissionsModule::with_requester(
            cwd.clone(),
            cwd.clone(),
            Arc::new(Approvals::default()),
        )
        .register(&mut registry, &config)
        .unwrap();
        if restricted {
            registry.restrict_tools(vec!["run_code".into()]);
        }
        let output = execute(Arc::new(registry), program("try { await tools.mcp__fixture__tool_0000({count:7}); } catch(e) { return e.message; }"), &cwd).await;
        assert!(!output.is_error);
        assert_eq!(details(&output).calls[0].state, ProgramCallState::Failed);
        assert!(
            text(&output).contains(if restricted {
                "Unknown or unavailable"
            } else {
                "Denied"
            }),
            "{output:?}"
        );
    }
    let mut registry = Registry::default();
    fixture.register(&mut registry);
    register(&mut registry);
    let registry = Arc::new(registry);
    for name in ["run_code", "call_tool", "tool_search"] {
        let result = execute(
            registry.clone(),
            program(&format!("await tools.{name}({{}});")),
            &cwd,
        )
        .await;
        assert!(text(&result).contains(&format!("{name} cannot be called from run_code")));
    }
    let result = execute(
        registry,
        program("await tools.mcp__fixture__tool_0000({count:'invalid'});"),
        &cwd,
    )
    .await;
    assert!(result.is_error);
    assert!(text(&result).contains("Validation failed"));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

struct TrackedTool {
    tool: Arc<dyn Tool>,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for TrackedTool {
    fn spec(&self) -> &ToolSpec {
        self.tool.spec()
    }
    fn programmatic(&self) -> bool {
        self.tool.programmatic()
    }
    fn concurrency(&self, input: &serde_json::Value) -> Concurrency {
        self.tool.concurrency(input)
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(40)).await;
        let result = self.tool.run(call, ctx).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
}

#[tokio::test]
async fn program_edits_writes_and_non_read_only_mcp_share_exclusivity_with_top_level_calls() {
    let fixture = McpFixture::new(Some(1)).await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("a.txt"), "before").unwrap();
    let mut source = Registry::default();
    fixture.register(&mut source);
    tiechui_module_editing::EditingModule
        .register(&mut source, &ModuleConfig::default())
        .unwrap();
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut registry = Registry::default();
    register(&mut registry);
    for name in ["edit", "write", "mcp__fixture__tool_0000"] {
        registry
            .register_tool(Arc::new(TrackedTool {
                tool: source.tools().get(name).unwrap(),
                active: active.clone(),
                peak: peak.clone(),
            }))
            .unwrap();
    }
    let registry = Arc::new(registry);
    let scheduler = Arc::new(ToolScheduler::default());
    let mut ctx = context(registry.clone(), scheduler.clone());
    ctx.cwd = root.path().to_owned();
    let calls = [
        program(
            "await Promise.all([tools.edit({path:'a.txt',edits:[{oldText:'before',newText:'after'}]}), tools.write({path:'b.txt',content:'new'}), tools.mcp__fixture__tool_0000({count:1})]);",
        ),
        call_tool("top", "write", json!({"path":"top.txt","content":"top"})),
    ];
    let outputs = tokio::time::timeout(
        Duration::from_secs(5),
        futures::future::join_all(
            calls
                .into_iter()
                .map(|call| interception::execute_call(&registry, &scheduler, call, ctx.clone())),
        ),
    )
    .await
    .expect("delegated and exclusive calls must not deadlock");
    assert!(outputs.iter().all(|output| !output.is_error), "{outputs:?}");
    assert_eq!(peak.load(Ordering::SeqCst), 1);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "after"
    );
}

#[tokio::test]
async fn mcp_annotations_select_shared_only_for_explicit_read_only() {
    let fixture = McpFixture::new(None).await;
    let mut registry = Registry::default();
    fixture.register(&mut registry);
    for (name, expected) in [
        ("echo", Concurrency::Shared),
        ("error", Concurrency::Exclusive),
        ("wait", Concurrency::Exclusive),
    ] {
        let tool = registry
            .tools()
            .get(&format!("mcp__fixture__{name}"))
            .unwrap();
        assert!(tool.programmatic());
        assert_eq!(tool.concurrency(&json!({})), expected);
        // Auto mode skips the classifier only for the same explicit hint.
        assert_eq!(tool.read_only(), expected == Concurrency::Shared);
    }
}

#[tokio::test]
async fn rewind_restores_edits_made_inside_a_program() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::write(cwd.join("file.txt"), "before").unwrap();
    let directory = root.path().join("rewind");
    let mut registry = Registry::default();
    register(&mut registry);
    tiechui_module_editing::EditingModule
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    tiechui_module_rewind::RewindModule {
        directory: directory.clone(),
    }
    .register(&mut registry, &ModuleConfig::default())
    .unwrap();
    let registry = Arc::new(registry);
    let scheduler = Arc::new(ToolScheduler::default());
    let mut ctx = context(registry.clone(), scheduler.clone());
    ctx.cwd = cwd.clone();
    ctx.branch = Arc::new(vec![tiechui_protocol::BranchEntry {
        id: "control".into(),
        parent_id: None,
        timestamp: 0,
        content: None,
        custom: Some((
            tiechui_module_rewind::CONTROL_CHECKPOINT.into(),
            json!({"version":1,"text":"checkpoint"}),
        )),
    }]);
    let output = tokio::time::timeout(Duration::from_secs(5), interception::execute_call(&registry, &scheduler, program("await tools.read({path:'file.txt'}); await tools.edit({path:'file.txt',edits:[{oldText:'before',newText:'after'}]});"), ctx)).await.unwrap();
    assert!(!output.is_error, "{output:?}");
    assert_eq!(
        std::fs::read_to_string(cwd.join("file.txt")).unwrap(),
        "after"
    );
    let store = tiechui_module_rewind::store::Store::new(&directory, &cwd, "test").unwrap();
    let cancel = CancellationToken::new();
    let plan = store.plan(&["control".into()], &cancel).unwrap();
    let _guard = store.restore(&plan, &cancel).unwrap();
    store.finish(&plan.target).unwrap();
    assert_eq!(
        std::fs::read_to_string(cwd.join("file.txt")).unwrap(),
        "before"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bash_errors_throw_and_cancellation_reaps_the_process_group() {
    let root = tempfile::tempdir().unwrap();
    let mut registry = Registry::default();
    register(&mut registry);
    registry
        .register_tool(Arc::new(tiechui_tools::BashTool::default()))
        .unwrap();
    let registry = Arc::new(registry);
    let output = execute(
        registry.clone(),
        program("try { await tools.bash({command:'exit 7'}); } catch(e) { return e.tool; }"),
        root.path(),
    )
    .await;
    assert!(!output.is_error);
    assert_eq!(details(&output).calls[0].state, ProgramCallState::Failed);
    assert!(text(&output).contains("Command exited with code 7"));
    assert_eq!(
        text(&output),
        "bash\n\ntool calls: 0 succeeded, 1 failed\n✗ bash {\"command\":\"exit 7\"}: (no output) Command exited with code 7"
    );
    let scheduler = Arc::new(ToolScheduler::default());
    let mut ctx = context(registry.clone(), scheduler.clone());
    ctx.cwd = root.path().to_owned();
    let cancel = ctx.cancel.clone();
    let execution = interception::execute_call(
        &registry,
        &scheduler,
        program("await tools.bash({command:'sleep 300 & child=$!; echo $$ $child > pids; wait'});"),
        ctx,
    );
    let interrupt = async {
        let path = root.path().join("pids");
        loop {
            if std::fs::read_to_string(&path).is_ok_and(|text| text.split_whitespace().count() == 2)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        cancel.cancel();
    };
    let (output, ()) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(execution, interrupt)
    })
    .await
    .expect("shell cancellation must settle");
    assert!(output.is_error);
    assert_eq!(details(&output).calls[0].state, ProgramCallState::Cancelled);
    let pids = std::fs::read_to_string(root.path().join("pids")).unwrap();
    for pid in pids
        .split_whitespace()
        .map(|pid| pid.parse::<i32>().unwrap())
    {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let missing = unsafe { libc::kill(pid, 0) } == -1
                    && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
                let zombie = cfg!(target_os = "linux")
                    && std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                        stat.rsplit_once(')')
                            .is_some_and(|(_, state)| state.trim_start().starts_with('Z'))
                    });
                if missing || zombie {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("process group must exit");
    }
    assert!(
        !execute(registry, program("return 'responsive';"), root.path())
            .await
            .is_error
    );
}
