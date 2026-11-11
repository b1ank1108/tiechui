use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Default)]
struct RegistryProbe {
    named: Mutex<Vec<String>>,
    unnamed: AtomicUsize,
}
impl ChildRegistry for RegistryProbe {
    fn build(&self, _: &ToolContext, identity: &str) -> Result<Registry, ToolError> {
        self.named.lock().unwrap().push(identity.to_owned());
        Ok(Registry::default())
    }
    fn build_default(&self, _: &ToolContext) -> Result<Registry, ToolError> {
        self.unnamed.fetch_add(1, Ordering::SeqCst);
        Ok(Registry::default())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecordedRequest {
    task: String,
    system: String,
    model: Option<String>,
    thinking: Option<ThinkingLevel>,
    tools: Option<Vec<String>>,
}

struct Runner {
    current: AtomicUsize,
    maximum: AtomicUsize,
    started: AtomicUsize,
    cleaned: AtomicUsize,
    tasks: Mutex<Vec<String>>,
    requests: Mutex<Vec<RecordedRequest>>,
    catalog: Mutex<ModelCatalog>,
    hold_until_eight: Option<Arc<tokio::sync::Barrier>>,
}

impl Default for Runner {
    fn default() -> Self {
        Self {
            current: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
            started: AtomicUsize::new(0),
            cleaned: AtomicUsize::new(0),
            tasks: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            catalog: Mutex::new(sample_catalog(false, false)),
            hold_until_eight: None,
        }
    }
}

fn event(kind: EventKind) -> Event {
    Event {
        submission_id: None,
        thread_id: ThreadId("child".into()),
        epoch: SessionEpoch(0),
        event: kind,
    }
}
#[async_trait]
impl ChildRunner for Runner {
    fn model_catalog(&self) -> ModelCatalog {
        self.catalog.lock().unwrap().clone()
    }
    fn validate_model(&self, model: &str) -> Result<(), ToolError> {
        let catalog = self.model_catalog();
        if catalog.models.iter().any(|entry| entry.model == model) {
            return Ok(());
        }
        let available: Vec<_> = catalog.models.iter().map(|e| e.model.as_str()).collect();
        Err(ToolError(format!(
            "unavailable '{model}': {}",
            available.join(", ")
        )))
    }
    async fn run(
        &self,
        request: ChildRequest,
        _: Registry,
        cancel: CancellationToken,
        events: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<(), ToolError> {
        self.tasks.lock().unwrap().push(request.task.clone());
        self.requests.lock().unwrap().push(RecordedRequest {
            task: request.task.clone(),
            system: request.system.clone(),
            model: request.model.clone(),
            thinking: request.thinking,
            tools: request.tools.clone(),
        });
        if request.task == "panic" {
            panic!("isolated fixture");
        }
        let order = self.started.fetch_add(1, Ordering::SeqCst) + 1;
        let active = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        if let Some(barrier) = &self.hold_until_eight
            && order <= 8
        {
            barrier.wait().await;
        }
        let resolved_model = request
            .model
            .clone()
            .unwrap_or_else(|| self.catalog.lock().unwrap().active.clone());
        let (provider, model_id) = resolved_model.split_once('/').unwrap();
        events(event(EventKind::SessionUpdated {
            model: ModelInfo {
                provider: provider.into(),
                id: model_id.into(),
                context_window: 32000,
                reasoning: false,
                thinking_levels: vec![ThinkingLevel::Off],
            },
            thinking: ThinkingLevel::Off,
            context: None,
            total: TokenUsage::default(),
        }));
        if request.task == "wait" {
            cancel.cancelled().await;
        } else {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let turn_id = TurnId("child-turn".into());
        let id = ItemId("text".into());
        events(event(EventKind::ItemStarted {
            turn_id: turn_id.clone(),
            item: Item::Known(KnownItem::AssistantMessage {
                id: id.clone(),
                text: String::new(),
            }),
        }));
        events(event(EventKind::ItemDelta {
            turn_id: turn_id.clone(),
            item_id: id,
            delta: request.task.clone(),
        }));
        events(event(EventKind::TokenUsageUpdated {
            turn_id: turn_id.clone(),
            last: TokenUsage::default(),
            total: TokenUsage {
                input: 7,
                output: 3,
                ..Default::default()
            },
        }));
        if request.task == "fail" {
            events(event(EventKind::Error {
                message: "Child provider failed".into(),
            }));
        }
        let status = if cancel.is_cancelled() {
            TurnStatus::Interrupted
        } else if request.task == "fail" {
            TurnStatus::Failed
        } else {
            TurnStatus::Completed
        };
        events(event(EventKind::TurnCompleted {
            turn: Turn {
                started_at: 0,
                ended_at: None,
                id: turn_id,
                status,
                items: vec![],
            },
        }));
        // Cleanup is deliberately later than the terminal event.
        tokio::time::sleep(Duration::from_millis(10)).await;
        self.current.fetch_sub(1, Ordering::SeqCst);
        self.cleaned.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
struct Harness {
    root: tempfile::TempDir,
    service: Arc<Service>,
    children: Arc<RegistryProbe>,
    runner: Arc<Runner>,
    ctx: ToolContext,
    entries: Arc<Mutex<Vec<tools::CustomSessionEntry>>>,
}
impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let children = Arc::new(RegistryProbe::default());
        let runner = Arc::new(Runner::default());
        let service = Arc::new(Service {
            home: root.path().join("home"),
            trusted: false,
            confirmation: None,
            children: children.clone(),
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TASKS)),
            runner: Mutex::new(Some(runner.clone())),
            state: Mutex::new(State {
                session: "parent".into(),
                ..Default::default()
            }),
        });
        let entries = Arc::new(Mutex::new(vec![]));
        let output = entries.clone();
        let ctx = ToolContext {
            invoker: None,
            cwd: root.path().into(),
            branch: Arc::new(vec![]),
            parent_session: None,
            cancel: CancellationToken::new(),
            updates: Arc::new(|_| {}),
            append_entry: Arc::new(move |entry| {
                output.lock().unwrap().push(entry);
                Ok(())
            }),
            epoch: SessionEpoch(1),
            scope: tools::ExecutionScope {
                thread_id: ThreadId("parent".into()),
                turn_id: TurnId("turn".into()),
            },
            origin: Default::default(),
        };
        Self {
            root,
            service,
            children,
            runner,
            ctx,
            entries,
        }
    }
    async fn run(&self, value: Value) -> Result<AgentDispatch, ToolError> {
        self.run_id("dispatch", value).await
    }
    async fn run_id(
        &self,
        id: impl Into<String>,
        value: Value,
    ) -> Result<AgentDispatch, ToolError> {
        let result = self
            .service
            .dispatch(
                ToolCall {
                    id: id.into(),
                    name: "subagent".into(),
                    arguments: value,
                    opaque: None,
                },
                self.ctx.clone(),
            )
            .await?;
        Ok(serde_json::from_value(result.details.unwrap()).unwrap())
    }
}

fn sample_catalog(is_child: bool, single_only: bool) -> ModelCatalog {
    let mut models = vec![CatalogModel {
        model: "alpha/fast".into(),
        reasoning: false,
        thinking_levels: vec![ThinkingLevel::Off],
    }];
    if !single_only {
        models.push(CatalogModel {
            model: "beta/judge".into(),
            reasoning: true,
            thinking_levels: vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
        });
    }
    ModelCatalog {
        is_child,
        active: "alpha/fast".into(),
        models,
    }
}

#[tokio::test]
async fn modes_limits_chain_substitution_and_real_failure_skip() {
    let h = Harness::new();
    let single = h.run(json!({"task":"literal $& {x}"})).await.unwrap();
    assert_eq!(single.tasks[0].agent, "default");
    assert_eq!(single.tasks[0].model, "alpha/fast");
    assert_eq!(single.tasks[0].output, "literal $& {x}");
    assert_eq!(single.tasks[0].usage.input, 7);
    let chain = h
        .run_id(
            "chain",
            json!({"chain":[{"task":"first"},{"task":"{previous} + {previous}"},{"task":"fail"},{"task":"never"}]}),
        )
        .await
        .unwrap();
    assert_eq!(chain.tasks[1].output, "first + first");
    assert_eq!(chain.tasks[2].state, AgentState::Failed);
    assert_eq!(chain.tasks[3].state, AgentState::Skipped);
    assert_eq!(chain.tasks[3].model, "");
    let chain_summary = super::result(&chain);
    let Content::Text {
        text: chain_text, ..
    } = &chain_summary.content[0]
    else {
        unreachable!()
    };
    assert!(
        chain_text.contains("### [default · alpha/fast] Completed"),
        "{chain_text}"
    );
    assert!(chain_text.contains("### [default] Skipped"), "{chain_text}");
    assert!(!h.runner.tasks.lock().unwrap().iter().any(|t| t == "never"));
    for value in [
        json!({}),
        json!({"tasks":[]}),
        json!({"task":"a","chain":[]}),
        json!({"model":"alpha/fast","tasks":[{"task":"x"}]}),
        json!({"tasks":vec![json!({"task":"x"});9]}),
    ] {
        assert!(h.run(value).await.is_err());
    }
    let parallel = h
        .run_id(
            "parallel",
            json!({"tasks":(0..8).map(|i| json!({"task":i.to_string()})).collect::<Vec<_>>()}),
        )
        .await
        .unwrap();
    assert_eq!(h.runner.maximum.load(Ordering::SeqCst), 8);
    for (i, t) in parallel.tasks.iter().enumerate() {
        assert_eq!(t.output, i.to_string());
        assert_eq!(t.state, AgentState::Completed);
    }
}

#[tokio::test]
async fn default_subagent_uses_build_default_and_rejects_removed_builtins_or_blank_agent() {
    let h = Harness::new();
    let agents_dir = h.service.home.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("default.md"),
        "---\nname: default\ndescription: disk policy\n---\nDisk prompt",
    )
    .unwrap();

    let unnamed = h
        .run_id("unnamed", json!({"task":"plain work"}))
        .await
        .unwrap();
    assert_eq!(unnamed.tasks[0].agent, "default");
    assert_eq!(h.children.unnamed.load(Ordering::SeqCst), 1);
    assert!(h.children.named.lock().unwrap().is_empty());
    let req = h.runner.requests.lock().unwrap()[0].clone();
    assert_eq!(req.system, "");
    assert_eq!(req.tools, None);

    std::fs::remove_file(agents_dir.join("default.md")).unwrap();
    let discovered =
        definitions::discover(&h.service.home, &h.ctx.cwd, "both", &h.ctx.cancel).unwrap();
    assert!(discovered.is_empty());

    for removed in ["scout", "planner", "reviewer", "worker"] {
        let err = h
            .run_id(removed, json!({"agent": removed, "task": "check"}))
            .await
            .unwrap_err()
            .0;
        assert!(err.contains(&format!("Unknown agent: {removed}")), "{err}");
        assert!(err.contains("omit `agent`"), "{err}");
    }
    for blank in ["", "   "] {
        assert!(
            h.run_id("blank", json!({"agent": blank, "task": "check"}))
                .await
                .is_err()
        );
    }
    assert_eq!(h.runner.started.load(Ordering::SeqCst), 1);

    std::fs::write(
        agents_dir.join("auditor.md"),
        "---\nname: auditor\ndescription: custom\n---\nAudit prompt",
    )
    .unwrap();
    let err = h
        .run_id("missing", json!({"agent": "scout", "task": "check"}))
        .await
        .unwrap_err()
        .0;
    assert!(err.contains("Unknown agent: scout"), "{err}");
    assert!(err.contains("auditor"), "{err}");
    assert_eq!(h.runner.started.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn model_and_thinking_priority_inherit_preflight_and_attribution() {
    let h = Harness::new();
    let agents_dir = h.service.home.join("agents");
    std::fs::create_dir_all(&agents_dir).unwrap();
    std::fs::write(
        agents_dir.join("critic.md"),
        "---\nname: critic\ndescription: custom\ntools: [read]\nmodel: alpha/fast\nthinking: low\n---\nCritic prompt",
    )
    .unwrap();
    std::fs::write(
        agents_dir.join("inheritor.md"),
        "---\nname: inheritor\ndescription: inherit\nmodel: inherit\nthinking: inherit\n---\nInherit prompt",
    )
    .unwrap();

    for (id, payload) in [
        ("bad-model", json!({"task": "x", "model": "missing/nope"})),
        ("bare-model", json!({"task": "x", "model": "judge"})),
        ("bad-thinking", json!({"task": "x", "thinking": "turbo"})),
        (
            "batch-preflight",
            json!({"tasks": [{"task": "ok"}, {"task": "bad", "model": "missing/nope"}]}),
        ),
    ] {
        let err = h.run_id(id, payload).await.unwrap_err().0;
        if id != "bad-thinking" {
            assert!(
                err.contains("alpha/fast") && err.contains("beta/judge"),
                "{err}"
            );
        } else {
            assert!(err.contains("turbo"), "{err}");
        }
    }
    assert_eq!(h.runner.started.load(Ordering::SeqCst), 0);

    let batch = h
        .run_id(
            "priority",
            json!({"tasks": [
                {"task": "t0", "model": "beta/judge"},
                {"agent": "critic", "task": "t1"},
                {"agent": "critic", "task": "t2", "model": "inherit", "thinking": "inherit"},
                {"agent": "critic", "task": "t3", "model": "beta/judge", "thinking": "high"},
                {"agent": "inheritor", "task": "t4"}
            ]}),
        )
        .await
        .unwrap();
    assert_eq!(batch.tasks[0].agent, "default");
    assert_eq!(batch.tasks[0].model, "beta/judge");
    assert_eq!(batch.tasks[1].agent, "critic");
    assert_eq!(batch.tasks[1].model, "alpha/fast");
    assert_eq!(batch.tasks[2].model, "alpha/fast");
    assert_eq!(batch.tasks[3].model, "beta/judge");
    assert_eq!(batch.tasks[4].model, "alpha/fast");

    let requests = h.runner.requests.lock().unwrap().clone();
    let by_task = |name: &str| requests.iter().find(|r| r.task == name).unwrap().clone();
    assert_eq!(by_task("t0").model.as_deref(), Some("beta/judge"));
    assert_eq!(by_task("t0").thinking, None);
    assert_eq!(by_task("t1").model.as_deref(), Some("alpha/fast"));
    assert_eq!(by_task("t1").thinking, Some(ThinkingLevel::Low));
    assert_eq!(by_task("t1").tools, Some(vec!["read".into()]));
    assert_eq!(by_task("t2").model, None);
    assert_eq!(by_task("t2").thinking, None);
    assert_eq!(by_task("t3").model.as_deref(), Some("beta/judge"));
    assert_eq!(by_task("t3").thinking, Some(ThinkingLevel::High));
    assert_eq!(by_task("t4").model, None);
    assert_eq!(by_task("t4").thinking, None);

    let summary = super::result(&batch);
    let Content::Text { text, .. } = &summary.content[0] else {
        unreachable!()
    };
    assert!(
        text.contains("### [default · beta/judge] Completed"),
        "{text}"
    );
    assert!(
        text.contains("### [critic · alpha/fast] Completed"),
        "{text}"
    );

    let resources = h.service.resources(&["subagent".into()], &[]);
    assert!(resources.iter().any(|r| {
        r.description == "default · beta/judge · Completed"
            && r.search_terms.iter().any(|s| s == "beta/judge")
            && r.reference.contains("(default · beta/judge, Completed)")
    }));
}

#[tokio::test]
async fn context_provider_exposes_active_and_candidates_to_parent_only() {
    let root = tempfile::tempdir().unwrap();
    let runner = Arc::new(Runner::default());
    let mut registry = Registry::default();
    SubagentsModule {
        home: root.path().join("home"),
        trusted: false,
        confirmation: None,
        children: Arc::new(RegistryProbe::default()),
    }
    .register(&mut registry, &ModuleConfig::default())
    .unwrap();

    let tool = registry.tools().get("subagent").unwrap();
    assert_eq!(tool.concurrency(&json!({})), Concurrency::Shared);
    for client in registry.actor_clients() {
        client.bind(runner.clone());
    }
    assert_eq!(registry.context_providers().len(), 1);
    let provider = &registry.context_providers()[0];

    let input = TurnInput {
        cwd: root.path().into(),
        content: vec![],
    };
    let multi = provider
        .fragments(&input, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(multi.len(), 1);
    assert!(multi[0].contains("alpha/fast"), "{}", multi[0]);
    assert!(multi[0].contains("beta/judge"), "{}", multi[0]);
    assert!(multi[0].contains("low, medium, high"), "{}", multi[0]);
    assert!(!multi[0].contains("providers.toml"), "{}", multi[0]);
    let described = provider.describe(&multi);
    assert_eq!(described.len(), 1);
    assert_eq!(described[0].label, "subagent_models");

    *runner.catalog.lock().unwrap() = sample_catalog(false, true);
    let single = provider
        .fragments(&input, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(single.len(), 1);
    assert!(single[0].contains("alpha/fast"), "{}", single[0]);
    assert!(
        single[0].contains("No other candidate models"),
        "{}",
        single[0]
    );

    *runner.catalog.lock().unwrap() = sample_catalog(true, false);
    let child = provider
        .fragments(&input, CancellationToken::new())
        .await
        .unwrap();
    assert!(child.is_empty());
}

#[tokio::test]
async fn session_wide_semaphore_caps_concurrent_dispatches_at_eight_and_queues_overflow() {
    let mut h = Harness::new();
    let runner = Arc::new(Runner {
        hold_until_eight: Some(Arc::new(tokio::sync::Barrier::new(8))),
        ..Runner::default()
    });
    h.runner = runner.clone();
    *h.service.runner.lock().unwrap() = Some(runner.clone());

    let jobs = (0..12).map(|i| h.run_id(format!("d-{i}"), json!({"task": format!("job-{i}")})));
    let results = futures::future::join_all(jobs).await;
    for res in results {
        let record = res.unwrap();
        assert_eq!(record.tasks[0].state, AgentState::Completed);
    }
    assert_eq!(runner.started.load(Ordering::SeqCst), 12);
    assert_eq!(runner.maximum.load(Ordering::SeqCst), 8);
}

#[tokio::test]
async fn cancellation_waits_for_cleanup_starts_nothing_new_and_retains_finished() {
    let h = Harness::new();
    let cancel = h.ctx.cancel.clone();
    let runner = h.runner.clone();
    let first = h.run_id(
        "first",
        json!({"tasks":std::iter::once(json!({"task":"done"})).chain((0..7).map(|_| json!({"task":"wait"}))).collect::<Vec<_>>()}),
    );
    let second = h.run_id(
        "second",
        json!({"tasks":(0..4).map(|_| json!({"task":"wait"})).collect::<Vec<_>>()}),
    );
    let stop = async move {
        while runner.cleaned.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        cancel.cancel();
    };
    let (first, second, ()) = tokio::join!(first, second, stop);
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.tasks[0].state, AgentState::Completed);
    assert!(
        first.tasks[1..]
            .iter()
            .chain(&second.tasks)
            .all(|t| t.state == AgentState::Cancelled)
    );
    assert!(h.runner.started.load(Ordering::SeqCst) <= 9);
    assert_eq!(h.runner.current.load(Ordering::SeqCst), 0);
    assert_eq!(
        h.runner.started.load(Ordering::SeqCst),
        h.runner.cleaned.load(Ordering::SeqCst)
    );
}
#[tokio::test]
async fn panic_scope_and_stale_completion_are_isolated() {
    let h = Harness::new();
    let result = h
        .run(json!({"tasks":[{"task":"panic"},{"task":"still works"}]}))
        .await
        .unwrap();
    assert_eq!(result.tasks[0].state, AgentState::Failed);
    assert!(
        result.tasks[0]
            .diagnostic
            .as_ref()
            .unwrap()
            .contains("panicked")
    );
    assert_eq!(result.tasks[1].state, AgentState::Completed);
    let result = h
        .run_id(
            "sibling",
            json!({"tasks":[{"task":"fail"},{"task":"survives sibling error"}]}),
        )
        .await
        .unwrap();
    assert_eq!(result.tasks[0].state, AgentState::Failed);
    assert_eq!(result.tasks[1].state, AgentState::Completed);
    let outside = tempfile::tempdir().unwrap();
    let result = h
        .run_id("escape", json!({"task":"escape","cwd":outside.path()}))
        .await
        .unwrap();
    assert!(
        result.tasks[0]
            .diagnostic
            .as_ref()
            .unwrap()
            .contains("parent workspace")
    );
    let run = h.run_id("stale", json!({"task":"wait"}));
    let change = async {
        while h.runner.current.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        h.service
            .event(
                &RuntimeEvent::Restore { navigation: true },
                &BranchView {
                    session_id: "new".into(),
                    leaf: "new".into(),
                    cwd: h.ctx.cwd.clone(),
                    parent_session: None,
                    entries: vec![],
                },
            )
            .unwrap();
    };
    let (result, ()) = tokio::join!(run, change);
    assert!(result.is_err());
    assert!(h.service.state.lock().unwrap().records.is_empty());
    assert_eq!(h.runner.current.load(Ordering::SeqCst), 0);
}
struct Confirm(AtomicUsize);
#[async_trait]
impl ProjectConfirmation for Confirm {
    async fn confirm(&self, _: Vec<PathBuf>, _: CancellationToken) -> Result<bool, ToolError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(true)
    }
}
#[tokio::test]
async fn project_definitions_support_claude_and_prefer_agents() {
    let h = Harness::new();
    for (source, prompt) in [(".claude/agents", "Claude"), (".agents/agents", "Agents")] {
        let directory = h.ctx.cwd.join(source);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("custom.md"),
            format!("---\nname: custom\ndescription: fixture\n---\n{prompt}"),
        )
        .unwrap();
        let agents =
            definitions::discover(&h.service.home, &h.ctx.cwd, "project", &h.ctx.cancel).unwrap();
        assert_eq!(agents["custom"].prompt, prompt);
        assert!(agents["custom"].project);
    }
}

#[tokio::test]
async fn rendered_definitions_read_back_and_keep_other_frontmatter() {
    let h = Harness::new();
    let directory = h.service.home.join("agents");
    std::fs::create_dir_all(&directory).unwrap();
    let empty = definitions::discover(&h.service.home, &h.ctx.cwd, "user", &h.ctx.cancel).unwrap();
    assert!(empty.is_empty());

    let existing = "---\n# header comment\nname: old\ndescription: >\n  folded\nthinking: low\npermission:\n  # rule comment\n  rules:\n  - allow: read\ntools:\n- bash\n# kept\n---\nOld";
    let mut definition = definitions::Definition {
        name: "custom".into(),
        description: "Review: \"changes\"\nonly".into(),
        prompt: "\nNew prompt\n".into(),
        tools: Some(vec!["read".into(), "grep".into()]),
        model: Some("fake/b".into()),
        thinking: Some(ChildThinking::Level(ThinkingLevel::Medium)),
        project: false,
        path: PathBuf::new(),
    };
    let text = definitions::render(existing, &definition);
    assert!(text.contains("thinking: \"medium\"\n"), "{text}");
    assert!(!text.contains("thinking: low"), "{text}");
    assert!(text.contains("# header comment\n"), "{text}");
    assert!(
        text.contains("permission:\n  # rule comment\n  rules:\n  - allow: read\n# kept\n---"),
        "{text}"
    );
    std::fs::write(directory.join("custom.md"), &text).unwrap();
    let agents = definitions::discover(&h.service.home, &h.ctx.cwd, "user", &h.ctx.cancel).unwrap();
    assert_eq!(agents.len(), 1);
    let custom = &agents["custom"];
    assert_eq!(custom.description, "Review: \"changes\" only");
    assert_eq!(custom.prompt, "New prompt");
    assert_eq!(
        custom.tools.as_deref(),
        Some(&["read".to_owned(), "grep".to_owned()][..])
    );
    assert_eq!(custom.model.as_deref(), Some("fake/b"));
    assert_eq!(
        custom.thinking,
        Some(ChildThinking::Level(ThinkingLevel::Medium))
    );

    definition.thinking = None;
    let cleared = definitions::render(&text, &definition);
    assert!(!cleared.contains("thinking:"), "{cleared}");
    assert!(cleared.contains("# header comment\n"), "{cleared}");
    assert!(
        cleared.contains("permission:\n  # rule comment\n  rules:\n  - allow: read\n# kept\n---"),
        "{cleared}"
    );
    std::fs::write(directory.join("custom.md"), cleared).unwrap();
    let agents = definitions::discover(&h.service.home, &h.ctx.cwd, "user", &h.ctx.cancel).unwrap();
    assert_eq!(agents["custom"].thinking, None);

    for (valid, expected) in [
        ("off", ChildThinking::Level(ThinkingLevel::Off)),
        ("minimal", ChildThinking::Level(ThinkingLevel::Minimal)),
        ("low", ChildThinking::Level(ThinkingLevel::Low)),
        ("medium", ChildThinking::Level(ThinkingLevel::Medium)),
        ("high", ChildThinking::Level(ThinkingLevel::High)),
        ("xhigh", ChildThinking::Level(ThinkingLevel::Xhigh)),
        ("max", ChildThinking::Level(ThinkingLevel::Max)),
        ("inherit", ChildThinking::Inherit),
    ] {
        std::fs::write(
            directory.join("valid.md"),
            format!("---\nname: valid\ndescription: ok\nthinking: {valid}\n---\nBody"),
        )
        .unwrap();
        let agents =
            definitions::discover(&h.service.home, &h.ctx.cwd, "user", &h.ctx.cancel).unwrap();
        assert_eq!(agents["valid"].thinking, Some(expected));
        assert_eq!(agents["valid"].thinking.unwrap().as_str(), valid);
    }

    std::fs::write(
        directory.join("null_thinking.md"),
        "---\nname: null_thinking\ndescription: ok\nthinking:\n---\nBody",
    )
    .unwrap();
    let agents = definitions::discover(&h.service.home, &h.ctx.cwd, "user", &h.ctx.cancel).unwrap();
    assert_eq!(agents["null_thinking"].thinking, None);

    for invalid in ["bogus", "\"\"", "HIGH", "42", "true", "[low]"] {
        std::fs::write(
            directory.join("invalid.md"),
            format!("---\nname: invalid\ndescription: bad\nthinking: {invalid}\n---\nBody"),
        )
        .unwrap();
        let agents =
            definitions::discover(&h.service.home, &h.ctx.cwd, "user", &h.ctx.cancel).unwrap();
        assert!(!agents.contains_key("invalid"), "accepted {invalid}");
    }
}

#[tokio::test]
async fn discovery_precedence_trust_cannot_be_bypassed_and_records_restore() {
    let mut h = Harness::new();
    let project = h.root.path().join(".agents/agents");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("custom.md"),
        "---\nname: custom\ndescription: fixture\ntools: [read, grep]\n---\nProject prompt",
    )
    .unwrap();
    assert!(
        h.run(
            json!({"agent":"custom","task":"task","agentScope":"both","confirmProjectAgents":false})
        )
        .await
        .unwrap_err()
        .0
        .contains("interactive terminal")
    );
    let confirm = Arc::new(Confirm(AtomicUsize::new(0)));
    Arc::get_mut(&mut h.service).unwrap().confirmation = Some(confirm.clone());
    h.run(json!({"agent":"custom","task":"approved","agentScope":"project"}))
        .await
        .unwrap();
    assert_eq!(confirm.0.load(Ordering::SeqCst), 1);
    std::fs::create_dir_all(h.service.home.join("agents")).unwrap();
    std::fs::write(
        h.service.home.join("agents/custom.md"),
        "---\nname: custom\ndescription: fixture\nmodel: fake/b\n---\nUser prompt",
    )
    .unwrap();
    let agents = definitions::discover(&h.service.home, &h.ctx.cwd, "both", &h.ctx.cancel).unwrap();
    assert_eq!(agents["custom"].prompt, "User prompt");
    assert!(!agents["custom"].project);
    let entries = h
        .entries
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, e)| BranchEntry {
            id: i.to_string(),
            parent_id: None,
            timestamp: 0,
            content: None,
            custom: Some((e.kind.clone(), e.data.clone())),
        })
        .collect();
    h.service
        .event(
            &RuntimeEvent::Restore { navigation: true },
            &BranchView {
                session_id: "parent".into(),
                leaf: "leaf".into(),
                cwd: h.ctx.cwd.clone(),
                parent_session: None,
                entries,
            },
        )
        .unwrap();
    assert_eq!(
        h.service.state.lock().unwrap().records["dispatch"].tasks[0].output,
        "approved"
    );
}
#[tokio::test]
async fn spill_is_private_bounded_retrievable_and_chain_gets_full_text() {
    let h = Harness::new();
    let text = "字👩‍💻".repeat(6000);
    let result = h
        .run(json!({"chain":[{"task":text},{"task":"{previous}"}]}))
        .await
        .unwrap();
    assert_eq!(result.tasks[0].output, text);
    let mut parallel = result.clone();
    parallel.mode = "parallel".into();
    let summary = super::result(&parallel);
    let Content::Text { text: summary, .. } = &summary.content[0] else {
        unreachable!()
    };
    assert!(summary.contains("### [default · alpha/fast] Completed"));
    assert!(summary.len() < 2 * (output::PREVIEW_BYTES + 1024));
    let path = result.tasks[0].output_path.as_ref().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    assert_eq!(
        std::fs::read_to_string(result.tasks[1].output_path.as_ref().unwrap()).unwrap(),
        text
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o077,
            0
        );
    }
    assert_eq!(output::prefix("x👩‍💻z", 5), "x");
    let mut output = output::Output::new(h.root.path());
    assert!(output.push(&"x".repeat(64 * 1024 * 1024 + 1)).is_err());
}

#[tokio::test]
async fn live_memory_and_block_limits_fail_with_diagnostics_after_cleanup() {
    let h = Harness::new();
    let result = h
        .run(json!({"task":"x".repeat(8 * 1024 * 1024 + 1)}))
        .await
        .unwrap();
    assert_eq!(result.tasks[0].state, AgentState::Failed);
    assert!(
        result.tasks[0]
            .diagnostic
            .as_ref()
            .unwrap()
            .contains("memory limit")
    );
    assert_eq!(h.runner.current.load(Ordering::SeqCst), 0);
    assert!(result.tasks[0].output.len() <= output::PREVIEW_BYTES);
    let mut collector = Collector::new(h.root.path());
    for i in 0..257 {
        let result = collector.event(&EventKind::ItemStarted {
            turn_id: TurnId("turn".into()),
            item: Item::Known(KnownItem::Thinking {
                id: ItemId(i.to_string()),
                text: String::new(),
            }),
        });
        assert_eq!(result.is_err(), i == 256);
    }
}
