use super::*;
use tiechui_plugin_api::{ChildRegistry, Module, ModuleConfig};

struct Children(PathBuf);
impl ChildRegistry for Children {
    fn build(&self, parent: &ToolContext, _: &str) -> Result<Registry, ToolError> {
        self.build_default(parent)
    }

    fn build_default(&self, parent: &ToolContext) -> Result<Registry, ToolError> {
        let mut registry = Registry::default();
        tiechui_module_editing::EditingModule
            .register_child(&mut registry, &ModuleConfig::default())?;
        tiechui_module_rewind::RewindModule {
            directory: self.0.clone(),
        }
        .register_child(&mut registry, parent.clone());
        Ok(registry)
    }
}
fn invoke(name: &str, args: serde_json::Value) -> Script {
    reply(vec![Content::ToolCall(ToolCall {
        id: format!("call-{name}"),
        name: name.into(),
        arguments: args,
        opaque: None,
    })])
}
async fn harness(scripts: Vec<Script>, home: &std::path::Path) -> Harness {
    Harness::with_contributions(
        scripts,
        Arc::new(BoundedRetry {
            max_retries: 0,
            ..Default::default()
        }),
        json!({"type":"object"}),
        |registry| {
            tiechui_module_subagents::SubagentsModule {
                home: home.into(),
                trusted: true,
                confirmation: None,
                children: Arc::new(Children(home.join("rewind"))),
            }
            .register(registry, &ModuleConfig::default())
            .unwrap();
            tiechui_module_goal::GoalModule
                .register(registry, &ModuleConfig::default())
                .unwrap();
        },
    )
    .await
}
fn records(store: &SessionStore) -> Vec<AgentDispatch> {
    store
        .entries()
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Custom { kind, data } if kind == "tiechui.subagents.record" => {
                serde_json::from_value(data.clone()).ok()
            }
            _ => None,
        })
        .collect()
}
#[tokio::test]
async fn actual_child_actor_edits_parent_checkpoint_inherits_model_and_isolates_tools() {
    let root = tempfile::tempdir().unwrap();
    let mut h = harness(
        vec![
            invoke("subagent", json!({"task":"child task"})),
            invoke("write", json!({"path":"child.txt","content":"child edit"})),
            answer("CHILD_DONE"),
            answer("PARENT_DONE"),
        ],
        root.path(),
    )
    .await;
    h.input("parent task", InputDelivery::FollowUp).await;
    h.end(TurnStatus::Completed).await;
    let parent = h.request().await;
    let child = h.request().await;
    assert!(parent.tools.iter().any(|s| s.name == "subagent"));
    assert_eq!(child.model, parent.model);
    assert_eq!(child.reasoning, parent.reasoning);
    assert!(child.tools.iter().any(|s| s.name == "write"));
    assert!(child.tools.iter().any(|s| s.name == "ls"));
    assert!(
        !child
            .tools
            .iter()
            .any(|s| matches!(s.name.as_str(), "probe" | "subagent" | "create_goal"))
    );
    assert_eq!(users(&child), ["child task"]);
    let child_json = serde_json::to_string(&child).unwrap();
    assert!(!child_json.contains("parent task"));
    assert!(!child_json.contains("worker agent"));
    assert_eq!(
        std::fs::read_to_string(h._root.path().join("child.txt")).unwrap(),
        "child edit"
    );
    let snapshot = h.snapshot().await;
    let records = records(&snapshot);
    let record = records.last().unwrap();
    assert_eq!(record.tasks[0].agent, "default");
    assert_eq!(record.tasks[0].model, "fake/a");
    assert_eq!(record.tasks[0].output, "CHILD_DONE");
    assert_eq!(record.tasks[0].state, AgentState::Completed);
    assert_eq!(record.tasks[0].turns, 2);
    assert_eq!(record.tasks[0].usage.input, 6);
    let anchor = snapshot
        .entries()
        .iter()
        .find(
            |e| matches!(&e.kind, EntryKind::Message { message, .. } if message.role == Role::User),
        )
        .unwrap()
        .id
        .clone();
    let store = tiechui_module_rewind::store::Store::new(
        &root.path().join("rewind"),
        h._root.path(),
        &h.thread.0,
    )
    .unwrap();
    let plan = store.plan(&[anchor], &CancellationToken::new()).unwrap();
    let _lease = store.restore(&plan, &CancellationToken::new()).unwrap();
    store.finish(&plan.target).unwrap();
    assert!(!h._root.path().join("child.txt").exists());
    h.valid_transcript().await;
    h.stop().await;
}

#[tokio::test]
async fn actor_panic_is_a_task_diagnostic_and_parent_continues() {
    let root = tempfile::tempdir().unwrap();
    let mut h = harness(
        vec![
            invoke("subagent", json!({"task":"panic"})),
            Script {
                error: None,
                steps: VecDeque::from([Step::Panic]),
            },
            answer("PARENT_SURVIVED"),
        ],
        root.path(),
    )
    .await;
    h.input("parent", InputDelivery::FollowUp).await;
    let event = h.until(|e| matches!(e, EventKind::ItemCompleted { item: Item::Known(KnownItem::ToolResult { output, .. }), .. } if output["details"]["tasks"].is_array())).await;
    let EventKind::ItemCompleted {
        item: Item::Known(KnownItem::ToolResult { output, .. }),
        ..
    } = event.event
    else {
        unreachable!()
    };
    assert_eq!(output["details"]["tasks"][0]["state"], "Failed");
    assert!(
        output["details"]["tasks"][0]["diagnostic"]
            .as_str()
            .unwrap()
            .contains("panicked")
    );
    h.end(TurnStatus::Completed).await;
    h.stop().await;
}

#[tokio::test]
async fn real_parallel_cancel_settles_before_navigation_and_keeps_finished_result() {
    let root = tempfile::tempdir().unwrap();
    let (_release, wait) = oneshot::channel();
    let mut h = harness(
        vec![
            invoke(
                "subagent",
                json!({"tasks":[{"task":"quick"},{"task":"wait"}]}),
            ),
            answer("FINISHED_CHILD"),
            Script {
                error: None,
                steps: VecDeque::from([Step::Wait(wait), Step::Event(finish(StopReason::EndTurn))]),
            },
        ],
        root.path(),
    )
    .await;
    h.input("parent", InputDelivery::FollowUp).await;
    let turn = h.started_turn().await;
    h.until(|e| matches!(e, EventKind::ItemDelta { delta, .. } if delta.contains("FINISHED_CHILD") && delta.contains("Completed"))).await;
    h.send(Op::Interrupt { turn_id: turn }).await;
    h.end(TurnStatus::Interrupted).await;
    let saved = h.snapshot().await;
    let records = records(&saved);
    let record = records.last().unwrap();
    assert!(
        record
            .tasks
            .iter()
            .any(|t| t.state == AgentState::Completed && t.output == "FINISHED_CHILD")
    );
    assert!(
        record
            .tasks
            .iter()
            .any(|t| t.state == AgentState::Cancelled)
    );
    h.valid_transcript().await;
    h.stop().await;
}

#[tokio::test]
async fn goal_accounts_child_totals_once() {
    let root = tempfile::tempdir().unwrap();
    let mut h = harness(
        vec![
            invoke("create_goal", json!({"objective":"explicit fixture goal"})),
            invoke("subagent", json!({"task":"child"})),
            answer("CHILD"),
            invoke("update_goal", json!({"status":"complete"})),
            answer("DONE"),
        ],
        root.path(),
    )
    .await;
    h.input("create a goal and run a child", InputDelivery::FollowUp)
        .await;
    h.end(TurnStatus::Completed).await;
    let store = h.snapshot().await;
    let goal = store
        .entries()
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EntryKind::Custom { kind, data } if kind == "tiechui.goal.state" => Some(data),
            _ => None,
        })
        .unwrap();
    // Three parent messages after goal creation plus one child: 3 input + 4 cache write + 2 output each.
    assert_eq!(goal["goal"]["tokens_used"], 36);
    h.stop().await;
}

#[tokio::test]
async fn records_recover_after_actor_restart_and_follow_selected_branch() {
    let root = tempfile::tempdir().unwrap();
    let mut h = harness(
        vec![
            invoke("subagent", json!({"task":"plan"})),
            answer("SAVED_CHILD"),
            answer("DONE"),
        ],
        root.path(),
    )
    .await;
    h.input("parent", InputDelivery::FollowUp).await;
    h.end(TurnStatus::Completed).await;
    h.shutdown.cancel();
    (&mut h.handle.task).await.unwrap().unwrap();
    let store = SessionStore::open(&h.path, &CancellationToken::new()).unwrap();
    let root_id = store.header().id.clone();
    let last = store.current_leaf().unwrap().to_owned();
    let mut registry = Registry::default();
    tiechui_module_subagents::SubagentsModule {
        home: root.path().into(),
        trusted: true,
        confirmation: None,
        children: Arc::new(Children(root.path().join("rewind"))),
    }
    .register(&mut registry, &ModuleConfig::default())
    .unwrap();
    h.shutdown = CancellationToken::new();
    h.epoch.0 += 1;
    let (requests, _) = mpsc::unbounded_channel();
    h.handle = SessionActor::spawn(
        store,
        SessionConfig {
            compaction: Default::default(),
            model: origin("a"),
            thinking: None,
            providers: BTreeMap::from([(
                "fake".into(),
                Arc::new(FakeProvider {
                    scripts: Mutex::new(VecDeque::new()),
                    requests,
                }) as Arc<dyn ModelProvider>,
            )]),
            registry,
            retry: Arc::new(BoundedRetry::default()),
            interactions: None,
            epoch: h.epoch,
            short_cache: false,
        },
        h.shutdown.clone(),
    );
    let event = h.until(|e| matches!(e, EventKind::Extension { name, data } if name == SUBAGENTS_EVENT && data["records"][0]["tasks"][0]["output"] == "SAVED_CHILD")).await;
    assert_eq!(event.epoch, h.epoch);
    h.until(|e| matches!(e, EventKind::ResourcesUpdated { resources } if resources.iter().any(|r| r.kind == ResourceKind::Task && r.reference.contains("SAVED_CHILD")))).await;
    h.send(Op::Branch { entry_id: root_id }).await;
    h.until(|e| matches!(e, EventKind::Extension { name, data } if name == SUBAGENTS_EVENT && data["records"].as_array().is_some_and(Vec::is_empty))).await;
    h.until(|e| matches!(e, EventKind::ResourcesUpdated { resources } if resources.is_empty()))
        .await;
    h.epoch.0 += 1;
    h.send(Op::Branch { entry_id: last }).await;
    h.until(|e| matches!(e, EventKind::Extension { name, data } if name == SUBAGENTS_EVENT && data["records"][0]["tasks"][0]["output"] == "SAVED_CHILD")).await;
    h.until(|e| matches!(e, EventKind::ResourcesUpdated { resources } if resources.iter().any(|r| r.reference.contains("SAVED_CHILD")))).await;
    h.stop().await;
}

struct PanicWrite(ToolSpec);
#[async_trait]
impl Tool for PanicWrite {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn concurrency(&self, _: &serde_json::Value) -> Concurrency {
        Concurrency::Exclusive
    }
    async fn run(&self, _: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        std::fs::write(ctx.cwd.join("partial.txt"), "partial edit").unwrap();
        panic!("writer fixture");
    }
}
#[tokio::test]
async fn tool_panic_settles_child_parent_anchored_writer_lease() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().canonicalize().unwrap();
    std::fs::write(cwd.join("partial.txt"), "before").unwrap();
    let parent = ToolContext {
        invoker: None,
        cwd: cwd.clone(),
        branch: Arc::new(vec![BranchEntry {
            id: "parent-user".into(),
            parent_id: None,
            timestamp: 0,
            content: Some(vec![InputContent::Text {
                text: "edit".into(),
            }]),
            custom: None,
        }]),
        parent_session: None,
        cancel: CancellationToken::new(),
        updates: Arc::new(|_| {}),
        append_entry: Arc::new(|_| Ok(())),
        epoch: SessionEpoch(0),
        scope: ExecutionScope {
            thread_id: ThreadId("parent".into()),
            turn_id: TurnId("turn".into()),
        },
        origin: Default::default(),
    };
    let mut registry = Registry::default();
    registry
        .register_tool(Arc::new(PanicWrite(ToolSpec {
            name: "write".into(),
            description: String::new(),
            input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        })))
        .unwrap();
    let directory = cwd.join("rewind");
    tiechui_module_rewind::RewindModule {
        directory: directory.clone(),
    }
    .register_child(&mut registry, parent.clone());
    let mut child = parent;
    child.scope.thread_id = ThreadId("child".into());
    child.branch = Arc::new(vec![]);
    let output = interception::execute_call(
        &registry,
        &ToolScheduler::default(),
        ToolCall {
            id: "write".into(),
            name: "write".into(),
            arguments: json!({"path":"partial.txt"}),
            opaque: None,
        },
        child,
    )
    .await;
    assert!(output.is_error);
    assert_eq!(output.details.unwrap()["panic"], true);
    let store = tiechui_module_rewind::store::Store::new(&directory, &cwd, "parent").unwrap();
    let plan = store
        .plan(&["parent-user".into()], &CancellationToken::new())
        .unwrap();
    let _guard = store.restore(&plan, &CancellationToken::new()).unwrap();
    store.finish(&plan.target).unwrap();
    assert_eq!(
        std::fs::read_to_string(cwd.join("partial.txt")).unwrap(),
        "before"
    );
}

fn full_reasoning_levels() -> Vec<Option<ReasoningLevel>> {
    std::iter::once(None)
        .chain(ReasoningLevel::ALL.map(Some))
        .collect()
}

struct CatalogProvider {
    models: Vec<(String, u64, Vec<Option<ReasoningLevel>>)>,
    child_started: AtomicUsize,
    hold_window: Option<(usize, usize, Arc<tokio::sync::Barrier>)>,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    scripts: Mutex<VecDeque<Script>>,
    requests: mpsc::UnboundedSender<ModelRequest>,
}

impl CatalogProvider {
    fn plain(
        models: Vec<(String, u64, Vec<Option<ReasoningLevel>>)>,
        scripts: Vec<Script>,
        requests: mpsc::UnboundedSender<ModelRequest>,
    ) -> Self {
        Self {
            models,
            child_started: AtomicUsize::new(0),
            hold_window: None,
            in_flight: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            scripts: Mutex::new(scripts.into()),
            requests,
        }
    }
}

#[async_trait]
impl ModelProvider for CatalogProvider {
    fn models(&self) -> Vec<ModelId> {
        self.models
            .iter()
            .map(|(id, _, _)| ModelId(id.clone()))
            .collect()
    }

    fn capabilities(&self, model: &ModelId) -> ModelCapabilities {
        match self.models.iter().find(|(id, _, _)| id == &model.0) {
            Some((_, window, levels)) => ModelCapabilities {
                context_window: *window,
                max_output: u64::from(*window > 0) * 1000,
                reasoning: levels.iter().any(Option::is_some),
                images: true,
            },
            None => ModelCapabilities {
                context_window: 0,
                max_output: 0,
                reasoning: false,
                images: false,
            },
        }
    }

    fn thinking_levels(&self, model: &ModelId) -> Vec<Option<ReasoningLevel>> {
        self.models
            .iter()
            .find(|(id, _, _)| id == &model.0)
            .map_or_else(|| vec![None], |(_, _, levels)| levels.clone())
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let is_child = !request.tools.iter().any(|t| t.name == "subagent");
        self.requests.send(request).unwrap();
        let steps = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model request")
            .steps;
        let events: Vec<_> = steps
            .into_iter()
            .filter_map(|step| match step {
                Step::Event(event) => Some(Ok(event)),
                _ => None,
            })
            .collect();
        if !is_child {
            return Ok(Box::pin(futures::stream::iter(events)));
        }
        let order = self.child_started.fetch_add(1, Ordering::SeqCst) + 1;
        let hold = self
            .hold_window
            .as_ref()
            .filter(|(start, end, _)| (*start..=*end).contains(&order))
            .map(|(_, _, barrier)| barrier.clone());
        let in_flight = self.in_flight.clone();
        let peak = self.peak.clone();
        let active = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(active, Ordering::SeqCst);
        let stream = futures::stream::once(async move {
            if let Some(barrier) = hold {
                barrier.wait().await;
            }
            in_flight.fetch_sub(1, Ordering::SeqCst);
            futures::stream::iter(events)
        })
        .flatten();
        Ok(Box::pin(stream))
    }
}

#[derive(Default)]
struct ChildProbe {
    runner: Mutex<Option<Arc<dyn tiechui_plugin_api::ChildRunner>>>,
    spawned_events: AtomicUsize,
}

impl tiechui_plugin_api::ActorClient for ChildProbe {
    fn bind(&self, runner: Arc<dyn tiechui_plugin_api::ChildRunner>) {
        *self.runner.lock().unwrap() = Some(runner);
    }
}

#[async_trait]
impl tiechui_plugin_api::ContextProvider for ChildProbe {
    async fn fragments(
        &self,
        _: &TurnInput,
        _: CancellationToken,
    ) -> Result<Vec<String>, ToolError> {
        let Some(runner) = self.runner.lock().unwrap().clone() else {
            return Ok(vec![]);
        };
        let catalog = runner.model_catalog();
        let candidates = catalog
            .candidates()
            .map(|m| format!("{}:{:?}", m.model, m.thinking_levels))
            .collect::<Vec<_>>()
            .join(",");
        Ok(vec![format!(
            "active={};candidates={candidates}",
            catalog.active
        )])
    }
}

struct SpawnChildTool {
    probe: Arc<ChildProbe>,
    spec: ToolSpec,
}

impl SpawnChildTool {
    fn new(probe: Arc<ChildProbe>) -> Self {
        Self {
            probe,
            spec: ToolSpec {
                name: "spawn_child".into(),
                description: "Test child runner".into(),
                input_schema: json!({"type": "object"}),
            },
        }
    }
}

#[async_trait]
impl Tool for SpawnChildTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &serde_json::Value) -> Concurrency {
        Concurrency::Exclusive
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let runner = self
            .probe
            .runner
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| ToolError("unbound runner".into()))?;
        let model = call.arguments["model"]
            .as_str()
            .filter(|v| *v != "inherit")
            .map(str::to_owned);
        let thinking = match call.arguments["thinking"]
            .as_str()
            .map(tiechui_plugin_api::parse_child_thinking)
            .transpose()?
        {
            Some(tiechui_plugin_api::ChildThinking::Level(level)) => Some(level),
            None | Some(tiechui_plugin_api::ChildThinking::Inherit) => None,
        };
        let seen_model = Arc::new(Mutex::new(String::new()));
        let seen_text = Arc::new(Mutex::new(String::new()));
        let model_slot = seen_model.clone();
        let text_slot = seen_text.clone();
        let probe = self.probe.clone();
        runner
            .run(
                tiechui_plugin_api::ChildRequest {
                    cwd: ctx.cwd.clone(),
                    task: call.arguments["task"]
                        .as_str()
                        .unwrap_or("child")
                        .to_owned(),
                    system: "child-system".into(),
                    model,
                    thinking,
                    tools: None,
                    mode: tiechui_plugin_api::ChildMode::Standard,
                    store_path: None,
                    steer: None,
                },
                Registry::default(),
                ctx.cancel.child_token(),
                Arc::new(move |event| {
                    probe.spawned_events.fetch_add(1, Ordering::SeqCst);
                    match event.event {
                        EventKind::SessionUpdated { model, .. } => {
                            *model_slot.lock().unwrap() =
                                format!("{}/{}", model.provider, model.id);
                        }
                        EventKind::ItemCompleted {
                            item: Item::Known(KnownItem::AssistantMessage { text, .. }),
                            ..
                        } => {
                            *text_slot.lock().unwrap() = text;
                        }
                        _ => {}
                    }
                }),
            )
            .await?;
        Ok(ToolOutput {
            details: Some(json!({"model": *seen_model.lock().unwrap()})),
            ..ToolOutput::text(seen_text.lock().unwrap().clone())
        })
    }
}

struct ProbeSetup {
    alpha: (Arc<CatalogProvider>, mpsc::UnboundedReceiver<ModelRequest>),
    beta: (Arc<CatalogProvider>, mpsc::UnboundedReceiver<ModelRequest>),
    registry: Registry,
    probe: Arc<ChildProbe>,
}

struct MultiProviderFixture {
    _root: TempDir,
    store_path: PathBuf,
    handle: SessionHandle,
    thread: ThreadId,
    epoch: SessionEpoch,
    alpha_rx: mpsc::UnboundedReceiver<ModelRequest>,
    beta_rx: mpsc::UnboundedReceiver<ModelRequest>,
    probe: Arc<ChildProbe>,
    shutdown: CancellationToken,
    seen: Vec<Event>,
}

impl MultiProviderFixture {
    async fn spawn_store(
        root: TempDir,
        store: SessionStore,
        (alpha, alpha_rx): (Arc<CatalogProvider>, mpsc::UnboundedReceiver<ModelRequest>),
        (beta, beta_rx): (Arc<CatalogProvider>, mpsc::UnboundedReceiver<ModelRequest>),
        (registry, probe): (Registry, Arc<ChildProbe>),
        (thinking, epoch): (Option<ReasoningLevel>, SessionEpoch),
    ) -> Self {
        let store_path = store.path().to_path_buf();
        let thread = ThreadId(store.header().id.clone());
        let shutdown = CancellationToken::new();
        let handle = SessionActor::spawn(
            store,
            SessionConfig {
                compaction: tiechui_plugin_api::CompactionConfig {
                    enabled: false,
                    ..Default::default()
                },
                model: ModelRef {
                    provider: "alpha".into(),
                    model: ModelId("alpha/fast".into()),
                },
                thinking,
                providers: BTreeMap::from([
                    ("alpha".into(), alpha as Arc<dyn ModelProvider>),
                    ("beta".into(), beta as Arc<dyn ModelProvider>),
                ]),
                registry,
                retry: Arc::new(BoundedRetry {
                    max_retries: 0,
                    ..Default::default()
                }),
                interactions: None,
                epoch,
                short_cache: false,
            },
            shutdown.clone(),
        );
        let mut fixture = Self {
            _root: root,
            store_path,
            handle,
            thread,
            epoch,
            alpha_rx,
            beta_rx,
            probe,
            shutdown,
            seen: vec![],
        };
        fixture
            .until(|e| matches!(e, EventKind::ThreadStarted { .. }))
            .await;
        fixture
    }

    async fn spawn(
        root: TempDir,
        alpha: (Arc<CatalogProvider>, mpsc::UnboundedReceiver<ModelRequest>),
        beta: (Arc<CatalogProvider>, mpsc::UnboundedReceiver<ModelRequest>),
        registry: Registry,
        probe: Arc<ChildProbe>,
        thinking: Option<ReasoningLevel>,
    ) -> Self {
        let dir = root.path().to_path_buf();
        let store = tokio::task::spawn_blocking(move || {
            let token = CancellationToken::new();
            SessionStore::create(&dir.join("sessions"), &dir, &token).unwrap()
        })
        .await
        .unwrap();
        Self::spawn_store(
            root,
            store,
            alpha,
            beta,
            (registry, probe),
            (thinking, SessionEpoch(0)),
        )
        .await
    }

    fn probe_setup(alpha_scripts: Vec<Script>, beta_scripts: Vec<Script>) -> ProbeSetup {
        let (alpha_tx, alpha_rx) = mpsc::unbounded_channel();
        let (beta_tx, beta_rx) = mpsc::unbounded_channel();
        let full_levels = full_reasoning_levels();
        let alpha = Arc::new(CatalogProvider::plain(
            vec![("alpha/fast".into(), 16000, vec![None])],
            alpha_scripts,
            alpha_tx,
        ));
        let beta = Arc::new(CatalogProvider::plain(
            vec![
                ("beta/judge".into(), 32000, full_levels.clone()),
                ("beta/org/nested".into(), 32000, full_levels),
                (
                    "beta/limited".into(),
                    32000,
                    vec![None, Some(ReasoningLevel::Low), Some(ReasoningLevel::High)],
                ),
                ("beta/unready".into(), 0, vec![None]),
            ],
            beta_scripts,
            beta_tx,
        ));
        let probe = Arc::new(ChildProbe::default());
        let mut registry = Registry::default();
        registry.register_actor_client(probe.clone());
        registry.register_context_provider(probe.clone());
        registry
            .register_tool(Arc::new(SpawnChildTool::new(probe.clone())))
            .unwrap();
        ProbeSetup {
            alpha: (alpha, alpha_rx),
            beta: (beta, beta_rx),
            registry,
            probe,
        }
    }

    async fn new(
        alpha_scripts: Vec<Script>,
        beta_scripts: Vec<Script>,
        thinking: Option<ReasoningLevel>,
    ) -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let setup = Self::probe_setup(alpha_scripts, beta_scripts);
        Self::spawn(
            root,
            setup.alpha,
            setup.beta,
            setup.registry,
            setup.probe,
            thinking,
        )
        .await
    }

    async fn resume(
        self,
        alpha_scripts: Vec<Script>,
        beta_scripts: Vec<Script>,
        thinking: Option<ReasoningLevel>,
        epoch: SessionEpoch,
    ) -> Self {
        let store_path = self.store_path;
        let root = self._root;
        self.shutdown.cancel();
        self.handle.task.await.unwrap().unwrap();
        let store = tokio::task::spawn_blocking(move || {
            SessionStore::open(&store_path, &CancellationToken::new()).unwrap()
        })
        .await
        .unwrap();
        let setup = Self::probe_setup(alpha_scripts, beta_scripts);
        Self::spawn_store(
            root,
            store,
            setup.alpha,
            setup.beta,
            (setup.registry, setup.probe),
            (thinking, epoch),
        )
        .await
    }

    async fn send(&mut self, id: &str, op: Op) {
        self.handle
            .submissions
            .send(Submission {
                id: SubmissionId(id.into()),
                thread_id: self.thread.clone(),
                epoch: self.epoch,
                op,
            })
            .await
            .unwrap();
    }

    async fn until(&mut self, predicate: impl Fn(&EventKind) -> bool) -> Event {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), self.handle.events.recv())
                .await
                .unwrap()
                .unwrap();
            let matched = predicate(&event.event);
            self.seen.push(event.clone());
            if matched {
                return event;
            }
        }
    }

    async fn run_turn(&mut self, id: &str, text: &str) {
        self.send(
            id,
            Op::UserInput {
                content: vec![InputContent::Text { text: text.into() }],
                delivery: InputDelivery::FollowUp,
            },
        )
        .await;
        self.until(|e| {
            matches!(e, EventKind::TurnCompleted { turn } if turn.status == TurnStatus::Completed)
        })
        .await;
    }

    fn tool_results(&self) -> Vec<(serde_json::Value, bool)> {
        self.seen
            .iter()
            .filter_map(|e| match &e.event {
                EventKind::ItemCompleted {
                    item:
                        Item::Known(KnownItem::ToolResult {
                            output, is_error, ..
                        }),
                    ..
                } => Some((output.clone(), *is_error)),
                _ => None,
            })
            .collect()
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.handle.task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn cross_provider_child_executes_and_preflight_rejects_invalid_or_unavailable_models() {
    let mut f = MultiProviderFixture::new(
        vec![
            invoke(
                "spawn_child",
                json!({"task": "judge task", "model": "beta/judge"}),
            ),
            invoke(
                "spawn_child",
                json!({"task": "nested task", "model": "beta/org/nested"}),
            ),
            invoke(
                "spawn_child",
                json!({"task": "inherited task", "model": "inherit"}),
            ),
            answer("child-inherited"),
            invoke(
                "spawn_child",
                json!({"task": "missing provider", "model": "missing/nope"}),
            ),
            invoke(
                "spawn_child",
                json!({"task": "unknown model", "model": "alpha/nope"}),
            ),
            invoke(
                "spawn_child",
                json!({"task": "unready model", "model": "beta/unready"}),
            ),
            invoke(
                "spawn_child",
                json!({"task": "bare model", "model": "judge"}),
            ),
            answer("parent-done"),
        ],
        vec![answer("child-beta"), answer("child-nested")],
        None,
    )
    .await;

    f.run_turn("turn-1", "go").await;

    let beta_first = recv(&mut f.beta_rx).await;
    let beta_second = recv(&mut f.beta_rx).await;
    assert_eq!(beta_first.model.0, "beta/judge");
    assert_eq!(beta_second.model.0, "beta/org/nested");
    assert!(f.beta_rx.try_recv().is_err());
    assert!(beta_first.short_cache && beta_second.short_cache);

    let mut alpha_requests = Vec::new();
    while let Ok(req) = f.alpha_rx.try_recv() {
        alpha_requests.push(req);
    }
    assert_eq!(alpha_requests.len(), 9);
    assert!(alpha_requests.iter().all(|r| r.model.0 == "alpha/fast"));
    // Only the inherited-model child caps its cache; the parent keeps the configured retention.
    let (child, parent): (Vec<_>, Vec<_>) = alpha_requests.iter().partition(|r| r.short_cache);
    assert_eq!((child.len(), parent.len()), (1, 8));
    assert!(parent.iter().all(|r| r.cache_key == parent[0].cache_key));
    assert_ne!(child[0].cache_key, parent[0].cache_key);

    let tool_results = f.tool_results();
    assert_eq!(tool_results.len(), 7);
    assert!(!tool_results[0].1);
    assert_eq!(tool_results[0].0["details"]["model"], "beta/judge");
    assert!(!tool_results[1].1);
    assert_eq!(tool_results[1].0["details"]["model"], "beta/org/nested");
    assert!(!tool_results[2].1);
    assert_eq!(tool_results[2].0["details"]["model"], "alpha/fast");

    for (output, is_error) in &tool_results[3..] {
        assert!(*is_error, "expected preflight error: {output}");
        let message = output["content"][0]["text"].as_str().unwrap();
        let (_, available) = message
            .split_once("Available models: ")
            .unwrap_or_else(|| panic!("missing Available models list: {message}"));
        assert!(available.contains("alpha/fast"), "{message}");
        assert!(available.contains("beta/judge"), "{message}");
        assert!(available.contains("beta/org/nested"), "{message}");
        assert!(available.contains("beta/limited"), "{message}");
        assert!(!available.contains("beta/unready"), "{message}");
    }

    f.stop().await;
}

#[tokio::test]
async fn preflight_failure_spawns_zero_child_actor_events() {
    let mut f = MultiProviderFixture::new(
        vec![
            invoke(
                "spawn_child",
                json!({"task": "bad", "model": "missing/nope"}),
            ),
            invoke("spawn_child", json!({"task": "bare", "model": "fast"})),
            invoke("spawn_child", json!({"task": "empty-p", "model": "/fast"})),
            invoke(
                "spawn_child",
                json!({"task": "empty-id", "model": "alpha/"}),
            ),
            answer("done"),
        ],
        vec![],
        None,
    )
    .await;

    f.run_turn("turn", "run").await;
    assert_eq!(f.probe.spawned_events.load(Ordering::SeqCst), 0);
    assert!(f.beta_rx.try_recv().is_err());
    f.stop().await;
}

#[tokio::test]
async fn child_thinking_inherits_parent_requested_level_clamps_to_target_model_and_rejects_invalid()
{
    let mut f = MultiProviderFixture::new(
        vec![
            invoke(
                "spawn_child",
                json!({"task": "default-inherit", "model": "beta/judge"}),
            ),
            invoke(
                "spawn_child",
                json!({"task": "explicit-inherit", "model": "beta/judge", "thinking": "inherit"}),
            ),
            invoke(
                "spawn_child",
                json!({"task": "clamp-up", "model": "beta/limited", "thinking": "minimal"}),
            ),
            invoke(
                "spawn_child",
                json!({"task": "clamp-down", "model": "beta/limited", "thinking": "max"}),
            ),
            invoke(
                "spawn_child",
                json!({"task": "explicit-off", "model": "beta/judge", "thinking": "off"}),
            ),
            invoke(
                "spawn_child",
                json!({"task": "bad-thinking", "model": "beta/judge", "thinking": "turbo"}),
            ),
            answer("done"),
        ],
        vec![
            answer("r1"),
            answer("r2"),
            answer("r3"),
            answer("r4"),
            answer("r5"),
        ],
        Some(ReasoningLevel::Medium),
    )
    .await;

    f.run_turn("turn", "check thinking").await;

    let parent_first = recv(&mut f.alpha_rx).await;
    assert_eq!(parent_first.reasoning, None);

    for expected in [
        Some(ReasoningLevel::Medium),
        Some(ReasoningLevel::Medium),
        Some(ReasoningLevel::Low),
        Some(ReasoningLevel::High),
        None,
    ] {
        assert_eq!(recv(&mut f.beta_rx).await.reasoning, expected);
    }
    assert!(f.beta_rx.try_recv().is_err());

    let results = f.tool_results();
    let last_result = results.last().unwrap();
    assert!(last_result.1);
    assert!(
        last_result.0["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("turbo")
    );
    f.stop().await;
}

#[tokio::test]
async fn requested_thinking_on_non_reasoning_parent_survives_reload_model_switch_and_resume() {
    let mut f = MultiProviderFixture::new(
        vec![
            invoke(
                "spawn_child",
                json!({"task": "after-reload", "model": "beta/judge"}),
            ),
            answer("turn-1-done"),
            invoke(
                "spawn_child",
                json!({"task": "after-switch-reload", "model": "beta/judge"}),
            ),
            answer("turn-2-done"),
        ],
        vec![answer("beta-1"), answer("beta-2")],
        Some(ReasoningLevel::High),
    )
    .await;

    f.send("reload-1", Op::Reload).await;
    let reloaded = f
        .until(|e| matches!(e, EventKind::ThreadStarted { .. }))
        .await;
    f.epoch = reloaded.epoch;

    f.run_turn("t1", "first").await;
    assert_eq!(recv(&mut f.alpha_rx).await.reasoning, None);
    assert_eq!(recv(&mut f.alpha_rx).await.reasoning, None);
    assert_eq!(
        recv(&mut f.beta_rx).await.reasoning,
        Some(ReasoningLevel::High)
    );

    f.send(
        "to-beta",
        Op::SetModel {
            provider: "beta".into(),
            model: "beta/judge".into(),
        },
    )
    .await;
    f.until(|e| {
        matches!(
            e,
            EventKind::SessionUpdated {
                model,
                thinking: ThinkingLevel::High,
                ..
            } if model.provider == "beta" && model.id == "judge"
        )
    })
    .await;

    f.send(
        "to-alpha",
        Op::SetModel {
            provider: "alpha".into(),
            model: "alpha/fast".into(),
        },
    )
    .await;
    f.until(|e| {
        matches!(
            e,
            EventKind::SessionUpdated {
                model,
                thinking: ThinkingLevel::Off,
                ..
            } if model.provider == "alpha" && model.id == "fast"
        )
    })
    .await;

    f.send("reload-2", Op::Reload).await;
    let reloaded = f
        .until(|e| matches!(e, EventKind::ThreadStarted { .. }))
        .await;
    f.epoch = reloaded.epoch;

    f.run_turn("t2", "second").await;
    assert_eq!(recv(&mut f.alpha_rx).await.reasoning, None);
    assert_eq!(recv(&mut f.alpha_rx).await.reasoning, None);
    assert_eq!(
        recv(&mut f.beta_rx).await.reasoning,
        Some(ReasoningLevel::High)
    );

    let mut resumed = f
        .resume(
            vec![
                invoke(
                    "spawn_child",
                    json!({"task": "after-resume", "model": "beta/judge"}),
                ),
                answer("turn-3-done"),
            ],
            vec![answer("beta-3")],
            None,
            SessionEpoch(5),
        )
        .await;
    resumed.run_turn("t3", "third").await;
    assert_eq!(recv(&mut resumed.alpha_rx).await.reasoning, None);
    assert_eq!(
        recv(&mut resumed.beta_rx).await.reasoning,
        Some(ReasoningLevel::High)
    );
    resumed.stop().await;
}

#[tokio::test]
async fn session_model_catalog_tracks_active_model_and_excludes_unready() {
    let mut f = MultiProviderFixture::new(
        vec![
            invoke(
                "spawn_child",
                json!({"task": "inspect child catalog", "model": "beta/judge"}),
            ),
            answer("turn-1-done"),
        ],
        vec![answer("child-ok"), answer("turn-2-on-beta")],
        None,
    )
    .await;

    f.run_turn("turn-1", "first").await;

    let first_parent = recv(&mut f.alpha_rx).await;
    let system_text = match &first_parent.messages[0].content[0] {
        Content::Text { text, .. } => text.clone(),
        _ => panic!("expected system message"),
    };
    assert!(system_text.contains("active=alpha/fast"), "{system_text}");
    assert!(system_text.contains("beta/judge:"), "{system_text}");
    assert!(!system_text.contains("beta/unready"), "{system_text}");

    let child_req = recv(&mut f.beta_rx).await;
    let child_system = match &child_req.messages[0].content[0] {
        Content::Text { text, .. } => text.clone(),
        _ => panic!("expected child system message"),
    };
    assert!(!child_system.contains("active="), "{child_system}");

    f.send(
        "switch",
        Op::SetModel {
            provider: "beta".into(),
            model: "beta/judge".into(),
        },
    )
    .await;
    f.until(|e| matches!(e, EventKind::SessionUpdated { model, .. } if model.provider == "beta" && model.id == "judge"))
        .await;

    f.run_turn("turn-2", "second").await;

    let second_parent = recv(&mut f.beta_rx).await;
    let switched_system = match &second_parent.messages[0].content[0] {
        Content::Text { text, .. } => text.clone(),
        _ => panic!("expected system message"),
    };
    assert!(
        switched_system.contains("active=beta/judge"),
        "{switched_system}"
    );
    assert!(
        switched_system.contains("alpha/fast:[Off]"),
        "{switched_system}"
    );
    f.stop().await;
}

#[tokio::test]
async fn subagents_module_routes_cross_provider_exposes_catalog_and_queues_ten_calls() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let home = root.path().join("home");
    std::fs::create_dir_all(home.join("agents")).unwrap();
    std::fs::write(
        home.join("agents/pinned.md"),
        "---\nname: pinned\ndescription: custom\nmodel: alpha/fast\nthinking: low\n---\nPinned prompt",
    )
    .unwrap();

    let (alpha_tx, alpha_rx) = mpsc::unbounded_channel();
    let (beta_tx, beta_rx) = mpsc::unbounded_channel();
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let ten_calls = reply(
        (0..10)
            .map(|i| {
                Content::ToolCall(ToolCall {
                    id: format!("conc-{i}"),
                    name: "subagent".into(),
                    arguments: json!({"task": format!("work-{i}"), "model": "beta/judge"}),
                    opaque: None,
                })
            })
            .collect(),
    );

    let alpha = Arc::new(CatalogProvider {
        models: vec![("alpha/fast".into(), 16000, vec![None])],
        child_started: AtomicUsize::new(0),
        hold_window: None,
        in_flight: in_flight.clone(),
        peak: peak.clone(),
        requests: alpha_tx,
        scripts: Mutex::new(VecDeque::from([
            invoke(
                "subagent",
                json!({"tasks": [
                    {"task": "judge-inherit-thinking", "model": "beta/judge"},
                    {"agent": "pinned", "task": "override-inherit", "model": "inherit"},
                    {"task": "judge-low", "model": "beta/judge", "thinking": "low"}
                ]}),
            ),
            answer("ALPHA_INHERITED"),
            answer("TURN_ONE_DONE"),
            invoke("subagent", json!({"agent": "scout", "task": "removed"})),
            invoke("subagent", json!({"task": "bad", "model": "missing/nope"})),
            answer("TURN_TWO_DONE"),
            ten_calls,
            answer("TURN_THREE_DONE"),
        ])),
    });
    let mut beta_scripts = VecDeque::from([answer("BETA_HIGH"), answer("BETA_LOW")]);
    for i in 0..10 {
        beta_scripts.push_back(answer(&format!("DONE_{i}")));
    }
    let beta = Arc::new(CatalogProvider {
        models: vec![("beta/judge".into(), 32000, full_reasoning_levels())],
        child_started: AtomicUsize::new(0),
        hold_window: Some((3, 10, Arc::new(tokio::sync::Barrier::new(8)))),
        in_flight: in_flight.clone(),
        peak: peak.clone(),
        requests: beta_tx,
        scripts: Mutex::new(beta_scripts),
    });

    let mut registry = Registry::default();
    registry.register_context_provider(Arc::new(StaticContext(vec![
        "Base coding assistant.".into(),
    ])));
    tiechui_module_subagents::SubagentsModule {
        home: home.clone(),
        trusted: true,
        confirmation: None,
        children: Arc::new(Children(home.join("rewind"))),
    }
    .register(&mut registry, &ModuleConfig::default())
    .unwrap();

    let mut f = MultiProviderFixture::spawn(
        root,
        (alpha, alpha_rx),
        (beta, beta_rx),
        registry,
        Arc::new(ChildProbe::default()),
        Some(ReasoningLevel::High),
    )
    .await;

    f.run_turn("t1", "compare").await;
    f.run_turn("t2", "preflight").await;
    tokio::time::timeout(Duration::from_secs(5), f.run_turn("t3", "ten-concurrent"))
        .await
        .expect("ten concurrent subagent calls should complete without deadlock");

    let parent_first = recv(&mut f.alpha_rx).await;
    let parent_system = match &parent_first.messages[0].content[0] {
        Content::Text { text, .. } => text.clone(),
        _ => panic!("expected system prompt"),
    };
    assert!(parent_system.contains("alpha/fast"), "{parent_system}");
    assert!(parent_system.contains("beta/judge"), "{parent_system}");
    assert!(!parent_system.contains("providers.toml"), "{parent_system}");

    let beta_req_1 = recv(&mut f.beta_rx).await;
    let beta_req_2 = recv(&mut f.beta_rx).await;
    let mut reasonings = [beta_req_1.reasoning, beta_req_2.reasoning];
    reasonings.sort_by_key(|r| r.map(|l| l as u8));
    assert_eq!(
        reasonings,
        [Some(ReasoningLevel::Low), Some(ReasoningLevel::High)]
    );
    for child_req in [&beta_req_1, &beta_req_2] {
        let child_sys = serde_json::to_string(&child_req.messages[0]).unwrap();
        assert!(!child_sys.contains("Candidate models"), "{child_sys}");
    }

    let tool_results = f.tool_results();
    assert_eq!(tool_results.len(), 13);
    assert!(!tool_results[0].1);
    let summary_text = tool_results[0].0["content"][0]["text"].as_str().unwrap();
    assert!(
        summary_text.contains("### [ag1 · default · beta/judge] Completed"),
        "{summary_text}"
    );
    assert!(
        summary_text.contains("### [ag2 · pinned · alpha/fast] Completed"),
        "{summary_text}"
    );

    assert!(tool_results[1].1);
    assert!(
        tool_results[1].0["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unknown agent: scout")
    );
    assert!(tool_results[2].1);
    let bad_model_text = tool_results[2].0["content"][0]["text"].as_str().unwrap();
    assert!(
        bad_model_text.contains("alpha/fast") && bad_model_text.contains("beta/judge"),
        "{bad_model_text}"
    );

    for (output, is_error) in &tool_results[3..] {
        assert!(!*is_error, "expected concurrent call to succeed: {output}");
    }
    let observed_peak = peak.load(Ordering::SeqCst);
    assert_eq!(observed_peak, 8);

    let store_path = f.store_path.clone();
    let _root = f._root;
    f.shutdown.cancel();
    f.handle.task.await.unwrap().unwrap();
    let reopened = SessionStore::open(&store_path, &CancellationToken::new()).unwrap();
    let saved_records = records(&reopened);
    assert_eq!(saved_records[1].tasks[0].model, "beta/judge");
    assert_eq!(saved_records[1].tasks[1].model, "alpha/fast");
    drop(_root);
}

#[tokio::test]
async fn child_store_path_creates_private_file_and_resumes_prior_turns() {
    let setup = MultiProviderFixture::probe_setup(
        vec![answer("PARENT_UNUSED")],
        vec![answer("FIRST_CHILD_REPLY"), answer("SECOND_CHILD_REPLY")],
    );
    let mut f = MultiProviderFixture::spawn(
        tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap(),
        setup.alpha,
        setup.beta,
        setup.registry,
        setup.probe,
        Some(ReasoningLevel::Low),
    )
    .await;

    let runner = f.probe.runner.lock().unwrap().clone().unwrap();
    let child_store = f
        ._root
        .path()
        .join("profile/state/subagents/sessions/parent-1/ag1.jsonl");
    let read_spec = ToolSpec {
        name: "read".into(),
        description: "Read file".into(),
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
    };
    let make_registry = || {
        let mut reg = Registry::default();
        tiechui_module_editing::EditingModule
            .register_child(&mut reg, &ModuleConfig::default())
            .unwrap();
        reg
    };

    let cwd = f._root.path().to_path_buf();
    let run_standard = |task: &str, system: &str, thinking: ThinkingLevel| {
        let runner = runner.clone();
        let cwd = cwd.clone();
        let store_path = Some(child_store.clone());
        let reg = make_registry();
        let task = task.to_owned();
        let system = system.to_owned();
        async move {
            runner
                .run(
                    tiechui_plugin_api::ChildRequest {
                        cwd,
                        task,
                        system,
                        model: Some("beta/judge".into()),
                        thinking: Some(thinking),
                        tools: Some(vec!["read".into()]),
                        mode: tiechui_plugin_api::ChildMode::Standard,
                        store_path,
                        steer: None,
                    },
                    reg,
                    CancellationToken::new(),
                    Arc::new(|_| {}),
                )
                .await
                .unwrap();
        }
    };

    run_standard("first child task", "first-system", ThinkingLevel::Medium).await;

    assert!(child_store.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for dir in child_store.parent().unwrap().ancestors().take(4) {
            assert_eq!(
                std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(
            std::fs::metadata(&child_store)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let first_req = recv(&mut f.beta_rx).await;
    assert_eq!(users(&first_req), ["first child task"]);
    assert_eq!(first_req.tools.len(), 1);
    assert_eq!(first_req.tools[0].name, read_spec.name);

    run_standard("second child task", "resumed-system", ThinkingLevel::High).await;

    let second_req = recv(&mut f.beta_rx).await;
    assert_eq!(
        users(&second_req),
        ["first child task", "second child task"]
    );
    assert_eq!(second_req.reasoning, Some(ReasoningLevel::High));
    assert!(
        serde_json::to_string(&second_req)
            .unwrap()
            .contains("FIRST_CHILD_REPLY")
    );
    assert!(
        serde_json::to_string(&second_req.messages[0])
            .unwrap()
            .contains("resumed-system")
    );

    #[cfg(unix)]
    {
        let symlink_path = f
            ._root
            .path()
            .join("profile/state/subagents/sessions/parent-1/symlink.jsonl");
        std::os::unix::fs::symlink(&child_store, &symlink_path).unwrap();
        let err = runner
            .run(
                tiechui_plugin_api::ChildRequest {
                    cwd: f._root.path().to_path_buf(),
                    task: "reject symlink".into(),
                    system: String::new(),
                    model: None,
                    thinking: None,
                    tools: None,
                    mode: tiechui_plugin_api::ChildMode::Standard,
                    store_path: Some(symlink_path),
                    steer: None,
                },
                Registry::default(),
                CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await;
        assert!(err.is_err());
    }

    f.stop().await;
}

struct GateTool {
    spec: ToolSpec,
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
}

#[async_trait]
impl Tool for GateTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &serde_json::Value) -> Concurrency {
        Concurrency::Shared
    }
    async fn run(&self, _: ToolCall, _: ToolContext) -> Result<ToolOutput, ToolError> {
        if let Some(entered) = self.entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        let rx = self.release.lock().unwrap().take();
        if let Some(rx) = rx {
            let _ = rx.await;
        }
        Ok(ToolOutput::text("gate-released"))
    }
}

#[tokio::test]
async fn live_steer_injects_at_child_tool_boundary_and_drains_queued_steers_before_exit() {
    let (mid_entered_tx, mid_entered_rx) = oneshot::channel();
    let (mid_release_tx, mid_release_rx) = oneshot::channel();
    let (tail_release_tx, tail_release_rx) = oneshot::channel();

    let (req_tx, mut req_rx) = mpsc::unbounded_channel();
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::from([
            invoke("gate", json!({})),
            Script {
                error: None,
                steps: VecDeque::from([
                    Step::Wait(tail_release_rx),
                    Step::Event(StreamEvent::TextDelta {
                        index: 0,
                        delta: "AFTER_FIRST_STEER".into(),
                    }),
                    Step::Event(finish(StopReason::EndTurn)),
                ]),
            },
            answer("AFTER_QUEUED_STEER"),
        ])),
        requests: req_tx,
    });

    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let store = SessionStore::create(
        &root.path().join("sessions"),
        root.path(),
        &CancellationToken::new(),
    )
    .unwrap();
    let probe = Arc::new(ChildProbe::default());
    let mut parent_reg = Registry::default();
    parent_reg.register_actor_client(probe.clone());
    let shutdown = CancellationToken::new();
    let mut parent = SessionActor::spawn(
        store,
        SessionConfig {
            compaction: Default::default(),
            model: origin("a"),
            thinking: None,
            providers: BTreeMap::from([("fake".into(), provider as Arc<dyn ModelProvider>)]),
            registry: parent_reg,
            retry: Arc::new(BoundedRetry::default()),
            interactions: None,
            epoch: SessionEpoch(0),
            short_cache: false,
        },
        shutdown.clone(),
    );
    while !matches!(
        parent.events.recv().await.unwrap().event,
        EventKind::ThreadStarted { .. }
    ) {}

    let runner = probe.runner.lock().unwrap().clone().unwrap();
    let mut child_reg = Registry::default();
    child_reg
        .register_tool(Arc::new(GateTool {
            spec: ToolSpec {
                name: "gate".into(),
                description: "Gate".into(),
                input_schema: json!({"type":"object"}),
            },
            entered: Mutex::new(Some(mid_entered_tx)),
            release: Mutex::new(Some(mid_release_rx)),
        }))
        .unwrap();

    let (steer_tx, steer_rx) = mpsc::unbounded_channel();
    let cwd = root.path().to_path_buf();
    let seen_texts = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = seen_texts.clone();
    let child_task = tokio::spawn(async move {
        runner
            .run(
                tiechui_plugin_api::ChildRequest {
                    cwd,
                    task: "initial task".into(),
                    system: String::new(),
                    model: None,
                    thinking: None,
                    tools: None,
                    mode: tiechui_plugin_api::ChildMode::Standard,
                    store_path: None,
                    steer: Some(steer_rx),
                },
                child_reg,
                CancellationToken::new(),
                Arc::new(move |event| {
                    if let EventKind::ItemCompleted {
                        item: Item::Known(KnownItem::AssistantMessage { text, .. }),
                        ..
                    } = event.event
                    {
                        seen_clone.lock().unwrap().push(text);
                    }
                }),
            )
            .await
    });

    let req1 = recv(&mut req_rx).await;
    assert_eq!(users(&req1), ["initial task"]);

    tokio::time::timeout(Duration::from_secs(5), mid_entered_rx)
        .await
        .unwrap()
        .unwrap();
    steer_tx.send("steer at tool boundary".into()).unwrap();
    mid_release_tx.send(()).unwrap();

    let req2 = recv(&mut req_rx).await;
    assert_eq!(users(&req2), ["initial task", "steer at tool boundary"]);
    steer_tx.send("steer queued before finish".into()).unwrap();
    tail_release_tx.send(()).unwrap();

    tokio::time::timeout(Duration::from_secs(5), child_task)
        .await
        .expect("child should finish after processing steers")
        .unwrap()
        .unwrap();

    let req3 = recv(&mut req_rx).await;
    assert_eq!(
        users(&req3),
        [
            "initial task",
            "steer at tool boundary",
            "steer queued before finish"
        ]
    );
    assert_eq!(
        *seen_texts.lock().unwrap(),
        vec!["AFTER_FIRST_STEER", "AFTER_QUEUED_STEER"]
    );

    shutdown.cancel();
    parent.task.await.unwrap().unwrap();
}

struct ForkProbeTool {
    probe: Arc<ChildProbe>,
    stores: BTreeMap<String, PathBuf>,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for ForkProbeTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &serde_json::Value) -> Concurrency {
        Concurrency::Shared
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let runner = self
            .probe
            .runner
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| ToolError("unbound runner".into()))?;
        let slot = call.arguments["slot"].as_str().unwrap_or("f1");
        let task = call.arguments["task"].as_str().unwrap_or("fork").to_owned();
        let store_path = self.stores.get(slot).cloned();
        let mut child_registry = Registry::default();
        tiechui_module_editing::EditingModule
            .register_child(&mut child_registry, &ModuleConfig::default())
            .unwrap();
        let final_text = Arc::new(Mutex::new(String::new()));
        let slot_text = final_text.clone();
        runner
            .run(
                tiechui_plugin_api::ChildRequest {
                    cwd: ctx.cwd.clone(),
                    task,
                    system: "ignored-in-fork".into(),
                    model: None,
                    thinking: None,
                    tools: None,
                    mode: tiechui_plugin_api::ChildMode::Fork,
                    store_path,
                    steer: None,
                },
                child_registry,
                ctx.cancel.child_token(),
                Arc::new(move |event| {
                    if let EventKind::ItemCompleted {
                        item: Item::Known(KnownItem::AssistantMessage { text, .. }),
                        ..
                    } = event.event
                    {
                        *slot_text.lock().unwrap() = text;
                    }
                }),
            )
            .await?;
        Ok(ToolOutput::text(final_text.lock().unwrap().clone()))
    }
}

fn anthropic_text_sse(text: &str) -> String {
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"usage\":{{\"input_tokens\":10,\"output_tokens\":0,\"cache_read_input_tokens\":8,\"cache_creation_input_tokens\":4}}}}}}\n\nevent: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"input_tokens\":null,\"output_tokens\":3}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n",
        serde_json::to_string(text).unwrap()
    )
}

fn anthropic_tool_calls_sse(
    thinking: Option<(&str, &str)>,
    lead_text: Option<&str>,
    calls: &[(&str, &str, serde_json::Value)],
) -> String {
    let mut out = String::from(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":15,\"output_tokens\":0,\"cache_read_input_tokens\":10,\"cache_creation_input_tokens\":5}}}\n\n",
    );
    let mut idx = 0;
    if let Some((thought, sig)) = thinking {
        out.push_str(&format!(
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{idx},\"content_block\":{{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{idx},\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":{}}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{idx},\"delta\":{{\"type\":\"signature_delta\",\"signature\":{}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{idx}}}\n\n",
            serde_json::to_string(thought).unwrap(),
            serde_json::to_string(sig).unwrap()
        ));
        idx += 1;
    }
    if let Some(text) = lead_text {
        out.push_str(&format!(
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{idx},\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{idx},\"delta\":{{\"type\":\"text_delta\",\"text\":{}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{idx}}}\n\n",
            serde_json::to_string(text).unwrap()
        ));
        idx += 1;
    }
    for (id, name, input) in calls {
        let raw = serde_json::to_string(input).unwrap();
        out.push_str(&format!(
            "event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":{idx},\"content_block\":{{\"type\":\"tool_use\",\"id\":{},\"name\":{},\"input\":{{}}}}}}\n\nevent: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":{idx},\"delta\":{{\"type\":\"input_json_delta\",\"partial_json\":{}}}}}\n\nevent: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":{idx}}}\n\n",
            serde_json::to_string(id).unwrap(),
            serde_json::to_string(name).unwrap(),
            serde_json::to_string(&raw).unwrap()
        ));
        idx += 1;
    }
    out.push_str(
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":null,\"output_tokens\":12}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    );
    out
}

fn last_wire_message(body: &serde_json::Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|msgs| msgs.last())
        .map(ToString::to_string)
        .unwrap_or_default()
}

fn anthropic_long_cache_provider(uri: String) -> Arc<dyn ModelProvider> {
    let model = serde_json::from_value(json!({
        "id": "sonnet",
        "context_window": 200_000,
        "max_output": 8192,
        "reasoning": true,
        "thinking": "adaptive",
        "cache_retention": "long"
    }))
    .unwrap();
    Arc::from(
        tiechui_providers::testing::provider(
            "anth",
            &tiechui_config::ProviderConfig {
                wire: tiechui_config::Wire::AnthropicMessages,
                base_url: Some(uri),
                api_key: None,
                api_key_env: None,
                compat: Default::default(),
                models: vec![model],
            },
        )
        .unwrap(),
    )
}

async fn spawn_anthropic_parent(
    parent_store: SessionStore,
    provider: Arc<dyn ModelProvider>,
    registry: Registry,
    shutdown: CancellationToken,
) -> SessionHandle {
    let mut parent = SessionActor::spawn(
        parent_store,
        SessionConfig {
            compaction: tiechui_plugin_api::CompactionConfig {
                enabled: false,
                ..Default::default()
            },
            model: ModelRef {
                provider: "anth".into(),
                model: ModelId("anth/sonnet".into()),
            },
            thinking: Some(ReasoningLevel::High),
            providers: BTreeMap::from([("anth".into(), provider)]),
            registry,
            retry: Arc::new(BoundedRetry {
                max_retries: 0,
                ..Default::default()
            }),
            interactions: None,
            epoch: SessionEpoch(0),
            short_cache: false,
        },
        shutdown,
    );
    while !matches!(
        parent.events.recv().await.unwrap().event,
        EventKind::ThreadStarted { .. }
    ) {}
    parent
}

async fn wire_request_bodies(server: &wiremock::MockServer) -> Vec<serde_json::Value> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|req| serde_json::from_slice(&req.body).unwrap())
        .collect()
}

fn assert_fork_wire_parity(fork_body: &serde_json::Value, parent_initial: &serde_json::Value) {
    assert_eq!(
        serde_json::to_vec(&fork_body["system"]).unwrap(),
        serde_json::to_vec(&parent_initial["system"]).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&fork_body["tools"]).unwrap(),
        serde_json::to_vec(&parent_initial["tools"]).unwrap()
    );
    assert_eq!(
        fork_body["messages"][0]["content"][0]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"})
    );
    assert_eq!(
        serde_json::to_vec(&fork_body["messages"][0]).unwrap(),
        serde_json::to_vec(&parent_initial["messages"][0]).unwrap()
    );
}

struct ForkWireRouter;

impl wiremock::Respond for ForkWireRouter {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let parsed: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let last_msg = last_wire_message(&parsed);
        let sse = if last_msg.contains("call_rec_resume")
            && last_msg.contains("Forked subagents cannot spawn or manage subagents")
        {
            anthropic_text_sse("FORK_A_RESUMED_DONE")
        } else if last_msg.contains("Resume Fork A") {
            anthropic_tool_calls_sse(
                None,
                None,
                &[("call_rec_resume", "subagent_list", json!({}))],
            )
        } else if last_msg.contains("Forked subagents cannot spawn or manage subagents") {
            anthropic_text_sse("FORK_B_RECOVERED")
        } else if last_msg.contains("call_write_a") {
            anthropic_text_sse("FORK_A_DONE")
        } else if last_msg.contains("Task A: inspect parser") {
            anthropic_tool_calls_sse(
                None,
                None,
                &[(
                    "call_write_a",
                    "write",
                    json!({"path": "fork-a.txt", "content": "from fork a"}),
                )],
            )
        } else if last_msg.contains("Task B: attempt recursion") {
            anthropic_tool_calls_sse(
                None,
                None,
                &[
                    ("call_rec_1", "subagent", json!({"task": "illegal nested"})),
                    (
                        "call_rec_2",
                        "subagent_send",
                        json!({"to": "ag1", "message": "illegal send"}),
                    ),
                ],
            )
        } else if last_msg.contains("FORK_A_DONE") && last_msg.contains("FORK_B_RECOVERED") {
            anthropic_text_sse("PARENT_ALL_DONE")
        } else {
            anthropic_tool_calls_sse(
                Some(("Analyze parent context.", "sig-parent-fork-1")),
                Some("Spawning two forks."),
                &[
                    (
                        "call_fork_1",
                        "fork_probe",
                        json!({
                            "slot": "f1",
                            "task": "<fork_boilerplate>stay in scope</fork_boilerplate>\nTask A: inspect parser"
                        }),
                    ),
                    (
                        "call_fork_2",
                        "fork_probe",
                        json!({
                            "slot": "f2",
                            "task": "<fork_boilerplate>stay in scope</fork_boilerplate>\nTask B: attempt recursion"
                        }),
                    ),
                ],
            )
        };
        wiremock::ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream")
    }
}

struct StubTool(ToolSpec);

#[async_trait]
impl Tool for StubTool {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn concurrency(&self, _: &serde_json::Value) -> Concurrency {
        Concurrency::Shared
    }
    async fn run(&self, _: ToolCall, _: ToolContext) -> Result<ToolOutput, ToolError> {
        panic!("subagent management tools must be rejected before execution in fork children")
    }
}

#[tokio::test]
async fn fork_child_shares_byte_identical_wire_prefix_and_1h_long_cache_breakpoints_and_resumes_across_fresh_runner()
 {
    use wiremock::{Mock, MockServer, matchers::method};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ForkWireRouter)
        .mount(&server)
        .await;

    let make_provider = || anthropic_long_cache_provider(server.uri());

    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let f1_store = root
        .path()
        .join("profile/state/subagents/sessions/parent-fork/ag1.jsonl");
    let f2_store = root
        .path()
        .join("profile/state/subagents/sessions/parent-fork/ag2.jsonl");

    let probe = Arc::new(ChildProbe::default());
    let mut registry = Registry::default();
    registry.register_actor_client(probe.clone());
    registry.register_context_provider(Arc::new(StaticContext(vec![
        "Primary coding instructions.".into(),
        "<subagent_models>\nActive session model: anth/sonnet\n</subagent_models>".into(),
    ])));
    tiechui_module_editing::EditingModule
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    registry
        .register_tool(Arc::new(ForkProbeTool {
            probe: probe.clone(),
            stores: BTreeMap::from([
                ("f1".into(), f1_store.clone()),
                ("f2".into(), f2_store.clone()),
            ]),
            spec: ToolSpec {
                name: "fork_probe".into(),
                description: "Spawn fork child".into(),
                input_schema: json!({
                    "type": "object",
                    "required": ["slot", "task"],
                    "properties": {
                        "slot": {"type": "string"},
                        "task": {"type": "string"}
                    }
                }),
            },
        }))
        .unwrap();
    for name in [
        "subagent",
        "subagent_send",
        "subagent_stop",
        "subagent_list",
    ] {
        registry
            .register_tool(Arc::new(StubTool(ToolSpec {
                name: name.into(),
                description: format!("Parent {name} tool"),
                input_schema: json!({"type": "object"}),
            })))
            .unwrap();
    }

    let parent_store = SessionStore::create(
        &root.path().join("sessions"),
        root.path(),
        &CancellationToken::new(),
    )
    .unwrap();
    let parent_thread = ThreadId(parent_store.header().id.clone());
    let shutdown = CancellationToken::new();
    let mut parent =
        spawn_anthropic_parent(parent_store, make_provider(), registry, shutdown.clone()).await;

    parent
        .submissions
        .send(Submission {
            id: SubmissionId("turn-1".into()),
            thread_id: parent_thread,
            epoch: SessionEpoch(0),
            op: Op::UserInput {
                content: vec![InputContent::Text {
                    text: "Coordinate two parallel forks".into(),
                }],
                delivery: InputDelivery::FollowUp,
            },
        })
        .await
        .unwrap();

    loop {
        if let EventKind::TurnCompleted { turn } = parent.events.recv().await.unwrap().event {
            assert_eq!(turn.status, TurnStatus::Completed);
            break;
        }
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("fork-a.txt")).unwrap(),
        "from fork a"
    );

    shutdown.cancel();
    parent.task.await.unwrap().unwrap();

    let bodies = wire_request_bodies(&server).await;
    let parent_initial = &bodies[0];
    let fork_a_initial = bodies
        .iter()
        .find(|b| last_wire_message(b).contains("Task A: inspect parser"))
        .expect("missing Fork A initial request");
    let fork_b_initial = bodies
        .iter()
        .find(|b| last_wire_message(b).contains("Task B: attempt recursion"))
        .expect("missing Fork B initial request");
    let fork_b_after_rejection = bodies
        .iter()
        .find(|b| {
            last_wire_message(b).contains("Forked subagents cannot spawn or manage subagents")
        })
        .expect("missing Fork B post-rejection request");

    assert_eq!(
        parent_initial["system"][0]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"})
    );
    assert_eq!(
        parent_initial["messages"][0]["content"][0]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"})
    );

    for fork_body in [fork_a_initial, fork_b_initial] {
        assert_fork_wire_parity(fork_body, parent_initial);
        assert_eq!(fork_body["model"], parent_initial["model"]);
        assert_eq!(fork_body["thinking"], parent_initial["thinking"]);
        assert_eq!(fork_body["output_config"], parent_initial["output_config"]);
    }

    assert_eq!(
        serde_json::to_vec(&fork_a_initial["messages"][1]).unwrap(),
        serde_json::to_vec(&fork_b_initial["messages"][1]).unwrap()
    );
    assert_eq!(
        fork_a_initial["messages"][1]["content"][0],
        json!({
            "type": "thinking",
            "thinking": "Analyze parent context.",
            "signature": "sig-parent-fork-1"
        })
    );

    let fork_a_tools_and_task = fork_a_initial["messages"][2]["content"].as_array().unwrap();
    let fork_b_tools_and_task = fork_b_initial["messages"][2]["content"].as_array().unwrap();
    assert_eq!(fork_a_tools_and_task.len(), 3);
    assert_eq!(fork_b_tools_and_task.len(), 3);
    assert_eq!(
        serde_json::to_vec(&fork_a_tools_and_task[..2]).unwrap(),
        serde_json::to_vec(&fork_b_tools_and_task[..2]).unwrap()
    );
    for (idx, expected_id) in ["call_fork_1", "call_fork_2"].into_iter().enumerate() {
        assert_eq!(
            fork_a_tools_and_task[idx],
            json!({
                "type": "tool_result",
                "tool_use_id": expected_id,
                "content": [{"type": "text", "text": "Fork started — processing in background"}],
                "is_error": false
            })
        );
    }

    let rejected_blocks = fork_b_after_rejection["messages"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()["content"]
        .as_array()
        .unwrap();
    for expected_id in ["call_rec_1", "call_rec_2"] {
        let block = rejected_blocks
            .iter()
            .find(|b| b["tool_use_id"] == expected_id)
            .expect("missing rejected tool_result");
        assert_eq!(block["is_error"], true);
        assert_eq!(
            block["content"][0]["text"],
            "Forked subagents cannot spawn or manage subagents; complete your task directly using your tools."
        );
    }

    let fresh_probe = Arc::new(ChildProbe::default());
    let mut fresh_parent_reg = Registry::default();
    fresh_parent_reg.register_actor_client(fresh_probe.clone());
    let fresh_store = SessionStore::create(
        &root.path().join("fresh-sessions"),
        root.path(),
        &CancellationToken::new(),
    )
    .unwrap();
    let fresh_shutdown = CancellationToken::new();
    let mut fresh_parent = SessionActor::spawn(
        fresh_store,
        SessionConfig {
            compaction: Default::default(),
            model: ModelRef {
                provider: "anth".into(),
                model: ModelId("anth/sonnet".into()),
            },
            thinking: None,
            providers: BTreeMap::from([("anth".into(), make_provider())]),
            registry: fresh_parent_reg,
            retry: Arc::new(BoundedRetry {
                max_retries: 0,
                ..Default::default()
            }),
            interactions: None,
            epoch: SessionEpoch(0),
            short_cache: false,
        },
        fresh_shutdown.clone(),
    );
    while !matches!(
        fresh_parent.events.recv().await.unwrap().event,
        EventKind::ThreadStarted { .. }
    ) {}
    let fresh_runner = fresh_probe.runner.lock().unwrap().clone().unwrap();

    let mut resumed_child_reg = Registry::default();
    tiechui_module_editing::EditingModule
        .register_child(&mut resumed_child_reg, &ModuleConfig::default())
        .unwrap();
    resumed_child_reg
        .register_tool(Arc::new(StubTool(ToolSpec {
            name: "subagent_list".into(),
            description: "Should be intercepted".into(),
            input_schema: json!({"type": "object"}),
        })))
        .unwrap();

    let resumed_reply = Arc::new(Mutex::new(String::new()));
    let resumed_slot = resumed_reply.clone();
    fresh_runner
        .run(
            tiechui_plugin_api::ChildRequest {
                cwd: root.path().to_path_buf(),
                task: "Resume Fork A".into(),
                system: "must-not-override-persisted-fork-system".into(),
                model: None,
                thinking: None,
                tools: None,
                mode: tiechui_plugin_api::ChildMode::Fork,
                store_path: Some(f1_store.clone()),
                steer: None,
            },
            resumed_child_reg,
            CancellationToken::new(),
            Arc::new(move |event| {
                if let EventKind::ItemCompleted {
                    item: Item::Known(KnownItem::AssistantMessage { text, .. }),
                    ..
                } = event.event
                {
                    *resumed_slot.lock().unwrap() = text;
                }
            }),
        )
        .await
        .unwrap();
    assert_eq!(*resumed_reply.lock().unwrap(), "FORK_A_RESUMED_DONE");

    let all_received = server.received_requests().await.unwrap();
    let resumed_req: serde_json::Value = serde_json::from_slice(
        &all_received
            .iter()
            .find(|r| {
                let s = String::from_utf8_lossy(&r.body);
                s.contains("Resume Fork A") && !s.contains("call_rec_resume")
            })
            .expect("missing resumed fork request")
            .body,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_vec(&resumed_req["system"]).unwrap(),
        serde_json::to_vec(&parent_initial["system"]).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&resumed_req["tools"]).unwrap(),
        serde_json::to_vec(&parent_initial["tools"]).unwrap()
    );
    assert_eq!(resumed_req["thinking"], parent_initial["thinking"]);
    assert_eq!(
        resumed_req["output_config"],
        parent_initial["output_config"]
    );
    let placeholder_occurrences = resumed_req
        .to_string()
        .matches("Fork started — processing in background")
        .count();
    assert_eq!(
        placeholder_occurrences, 2,
        "resuming fork store must not re-seed parent prefix"
    );
    assert!(resumed_req.to_string().contains("FORK_A_DONE"));

    fresh_shutdown.cancel();
    fresh_parent.task.await.unwrap().unwrap();
}

struct SplitProvider {
    parent: FakeProvider,
    child: FakeProvider,
}

#[async_trait]
impl ModelProvider for SplitProvider {
    fn capabilities(&self, model: &ModelId) -> ModelCapabilities {
        self.parent.capabilities(model)
    }

    fn thinking_levels(&self, model: &ModelId) -> Vec<Option<ReasoningLevel>> {
        self.parent.thinking_levels(model)
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        if request.tools.iter().any(|t| t.name == "subagent") {
            self.parent.stream(request, cancel).await
        } else {
            self.child.stream(request, cancel).await
        }
    }
}

async fn respawn_split_actor(
    h: &mut Harness,
    home: &std::path::Path,
    children: Arc<dyn ChildRegistry>,
    include_goal: bool,
    with_interactions: bool,
    scripts: (Vec<Script>, Vec<Script>),
) -> mpsc::UnboundedReceiver<ModelRequest> {
    let store = SessionStore::open(&h.path, &CancellationToken::new()).unwrap();
    let (parent_scripts, child_scripts) = scripts;
    let (parent_tx, parent_rx) = mpsc::unbounded_channel();
    let (child_tx, child_rx) = mpsc::unbounded_channel();
    let provider = Arc::new(SplitProvider {
        parent: FakeProvider {
            scripts: Mutex::new(parent_scripts.into()),
            requests: parent_tx,
        },
        child: FakeProvider {
            scripts: Mutex::new(child_scripts.into()),
            requests: child_tx,
        },
    });
    let mut registry = Registry::default();
    tiechui_module_subagents::SubagentsModule {
        home: home.into(),
        trusted: true,
        confirmation: None,
        children,
    }
    .register(&mut registry, &ModuleConfig::default())
    .unwrap();
    if include_goal {
        tiechui_module_goal::GoalModule
            .register(&mut registry, &ModuleConfig::default())
            .unwrap();
    }
    let inbox = if with_interactions {
        let (interactions, inbox) = Interactions::new();
        h.interactions = interactions;
        Some(inbox)
    } else {
        None
    };
    h.shutdown = CancellationToken::new();
    h.epoch.0 += 1;
    h.requests = parent_rx;
    h.seen.clear();
    h.handle = SessionActor::spawn(
        store,
        SessionConfig {
            compaction: tiechui_plugin_api::CompactionConfig {
                enabled: false,
                ..Default::default()
            },
            model: origin("a"),
            thinking: None,
            providers: BTreeMap::from([("fake".into(), provider as Arc<dyn ModelProvider>)]),
            registry,
            retry: Arc::new(BoundedRetry {
                max_retries: 0,
                ..Default::default()
            }),
            interactions: inbox,
            epoch: h.epoch,
            short_cache: false,
        },
        h.shutdown.clone(),
    );
    h.until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    child_rx
}

async fn split_harness(
    parent_scripts: Vec<Script>,
    child_scripts: Vec<Script>,
    home: &std::path::Path,
) -> (Harness, mpsc::UnboundedReceiver<ModelRequest>) {
    let mut h = harness(vec![], home).await;
    h.shutdown.cancel();
    (&mut h.handle.task).await.unwrap().unwrap();
    let child_rx = respawn_split_actor(
        &mut h,
        home,
        Arc::new(Children(home.join("rewind"))),
        false,
        true,
        (parent_scripts, child_scripts),
    )
    .await;
    (h, child_rx)
}

#[tokio::test]
async fn background_subagent_releases_turn_survives_parent_esc_and_wakes_idle_continuation_once() {
    let root = tempfile::tempdir().unwrap();
    let (release_bg, wait_bg) = oneshot::channel();
    let (release_parent_slow, wait_parent_slow) = oneshot::channel();
    let (mut h, mut child_rx) = split_harness(
        vec![
            invoke(
                "subagent",
                json!({"task": "background audit", "name": "auditor", "background": true}),
            ),
            answer("LAUNCHED_ACK"),
            Script {
                error: None,
                steps: VecDeque::from([
                    Step::Wait(wait_parent_slow),
                    Step::Event(finish(StopReason::EndTurn)),
                ]),
            },
            answer("FINAL_SUMMARY_FROM_CONTINUATION"),
        ],
        vec![Script {
            error: None,
            steps: VecDeque::from([
                Step::Wait(wait_bg),
                Step::Event(StreamEvent::TextDelta {
                    index: 0,
                    delta: "BG_AUDIT_RESULT".into(),
                }),
                Step::Event(finish(StopReason::EndTurn)),
            ]),
        }],
        root.path(),
    )
    .await;

    h.input("start background audit", InputDelivery::FollowUp)
        .await;
    tokio::time::timeout(Duration::from_secs(2), h.end(TurnStatus::Completed))
        .await
        .expect("background subagent must release parent turn without waiting for child");

    let parent_req_1 = h.request().await;
    assert!(parent_req_1.tools.iter().any(|t| t.name == "subagent_stop"));
    assert!(parent_req_1.tools.iter().any(|t| t.name == "subagent_list"));
    let parent_req_2 = h.request().await;
    assert!(
        results(&parent_req_2)[0]
            .content
            .iter()
            .any(|c| matches!(c, Content::Text { text, .. } if text.contains("ag1") && text.contains("task-notification")))
    );
    let _child_req = tokio::time::timeout(Duration::from_secs(2), child_rx.recv())
        .await
        .unwrap()
        .unwrap();

    h.input("slow foreground question", InputDelivery::FollowUp)
        .await;
    let slow_turn = h.started_turn().await;
    let _slow_req = h.request().await;
    h.send(Op::Interrupt { turn_id: slow_turn }).await;
    h.end(TurnStatus::Interrupted).await;

    let _ = release_bg.send(());
    let continuation_req = tokio::time::timeout(Duration::from_secs(2), h.request())
        .await
        .expect("expected idle continuation request");
    let last_user = users(&continuation_req).pop().unwrap();
    assert!(last_user.contains("<task-notification>"), "{last_user}");
    assert!(last_user.contains("<task-id>ag1</task-id>"), "{last_user}");
    assert!(last_user.contains("BG_AUDIT_RESULT"), "{last_user}");
    h.end(TurnStatus::Completed).await;

    assert!(
        !h.seen.iter().any(|ev| matches!(
            &ev.event,
            EventKind::ItemCompleted {
                item: Item::Known(KnownItem::UserMessage { content, .. }),
                ..
            } if content.iter().any(|part| matches!(part, InputContent::Text { text } if text.contains("<task-notification>")))
        )),
        "hidden continuation must not emit a visible UserMessage bubble"
    );

    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        h.requests.try_recv().is_err(),
        "continuation must fire once"
    );
    let saved = h.snapshot().await;
    let saved_records = records(&saved);
    let latest = saved_records.last().unwrap();
    assert!(latest.background);
    assert_eq!(latest.tasks[0].id, "ag1");
    assert_eq!(latest.tasks[0].state, AgentState::Completed);
    assert_eq!(latest.tasks[0].output, "BG_AUDIT_RESULT");
    let _ = release_parent_slow;
    h.valid_transcript().await;
    h.stop().await;
}

#[tokio::test]
async fn foreground_subagent_promotes_on_background_all_and_subagent_stop_suppresses_wakeup() {
    let root = tempfile::tempdir().unwrap();
    let (release_promoted, wait_promoted) = oneshot::channel();
    let (_hold_stopped, wait_stopped) = oneshot::channel();
    let (mut h, mut child_rx) = split_harness(
        vec![
            invoke("subagent", json!({"task": "slow fg work", "name": "fg1"})),
            answer("PROMOTED_ACK"),
            invoke(
                "subagent",
                json!({"task": "stoppable bg", "name": "bg2", "background": true}),
            ),
            invoke("subagent_list", json!({})),
            invoke("subagent_stop", json!({"id": "bg2"})),
            answer("STOPPED_BG2_ACK"),
            answer("PROMOTED_CONTINUATION_SUMMARY"),
        ],
        vec![
            Script {
                error: None,
                steps: VecDeque::from([
                    Step::Wait(wait_promoted),
                    Step::Event(StreamEvent::TextDelta {
                        index: 0,
                        delta: "PROMOTED_CHILD_DONE".into(),
                    }),
                    Step::Event(finish(StopReason::EndTurn)),
                ]),
            },
            Script {
                error: None,
                steps: VecDeque::from([
                    Step::Wait(wait_stopped),
                    Step::Event(finish(StopReason::EndTurn)),
                ]),
            },
        ],
        root.path(),
    )
    .await;

    h.input("run foreground subagent", InputDelivery::FollowUp)
        .await;
    let _parent_req = h.request().await;
    let _child_fg_req = tokio::time::timeout(Duration::from_secs(2), child_rx.recv())
        .await
        .unwrap()
        .unwrap();

    h.send(Op::CommandInput {
        name: "subagents".into(),
        data: json!({"background_all": true}),
    })
    .await;
    tokio::time::timeout(Duration::from_secs(2), h.end(TurnStatus::Completed))
        .await
        .expect("background_all must promote foreground subagent and release parent turn");
    let promoted_tool_req = h.request().await;
    assert!(
        results(&promoted_tool_req)[0]
            .content
            .iter()
            .any(|c| matches!(c, Content::Text { text, .. } if text.contains("background") && text.contains("ag1")))
    );

    h.input("launch second and stop it", InputDelivery::FollowUp)
        .await;
    tokio::time::timeout(Duration::from_secs(2), h.end(TurnStatus::Completed))
        .await
        .expect("launch + list + stop turn must complete cleanly");
    let _launch_bg2_req = h.request().await;
    let _list_req = h.request().await;
    let stop_req = h.request().await;
    assert!(results(&stop_req).last().unwrap().content.iter().any(
        |c| matches!(c, Content::Text { text, .. } if text.contains("ag2") && text.contains("ag1"))
    ));
    let after_stop_req = h.request().await;
    assert!(
        results(&after_stop_req)
            .last()
            .unwrap()
            .content
            .iter()
            .any(|c| matches!(c, Content::Text { text, .. } if text.contains("ag2")))
    );

    tokio::time::sleep(Duration::from_millis(120)).await;
    assert!(
        h.requests.try_recv().is_err(),
        "stopped ag2 must not wake continuation"
    );

    let _ = release_promoted.send(());
    let cont_req = tokio::time::timeout(Duration::from_secs(2), h.request())
        .await
        .expect("promoted ag1 must wake continuation");
    let cont_text = users(&cont_req).pop().unwrap();
    assert!(cont_text.contains("<task-id>ag1</task-id>"), "{cont_text}");
    assert!(!cont_text.contains("<task-id>ag2</task-id>"), "{cont_text}");
    assert!(cont_text.contains("PROMOTED_CHILD_DONE"), "{cont_text}");
    h.end(TurnStatus::Completed).await;

    h.valid_transcript().await;
    h.stop().await;
}

struct SteerGateTool {
    spec: ToolSpec,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl Tool for SteerGateTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &serde_json::Value) -> Concurrency {
        Concurrency::Shared
    }
    async fn run(&self, _: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.entered.notify_one();
        tokio::select! {
            () = self.release.notified() => Ok(ToolOutput::text(format!("cwd={}", ctx.cwd.display()))),
            () = ctx.cancel.cancelled() => Err(ToolError("cancelled".into())),
        }
    }
}

struct SteerGateChildren {
    rewind: PathBuf,
    gate: Arc<SteerGateTool>,
}

impl ChildRegistry for SteerGateChildren {
    fn build(&self, parent: &ToolContext, _: &str) -> Result<Registry, ToolError> {
        self.build_default(parent)
    }

    fn build_default(&self, parent: &ToolContext) -> Result<Registry, ToolError> {
        let mut registry = Children(self.rewind.clone()).build_default(parent)?;
        registry.register_tool(self.gate.clone())?;
        Ok(registry)
    }
}

async fn submit_and_wait_gate(h: &mut Harness, gate: &SteerGateTool, prompt: &str) {
    h.input(prompt, InputDelivery::FollowUp).await;
    tokio::time::timeout(Duration::from_secs(5), h.end(TurnStatus::Completed))
        .await
        .unwrap();
    let _ = h.request().await;
    let _ = h.request().await;
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .expect("child must enter hold_gate");
}

#[tokio::test]
async fn subagent_send_steers_running_child_resumes_across_restart_and_avoids_double_counting_usage()
 {
    let root = tempfile::tempdir().unwrap();
    let gate = Arc::new(SteerGateTool {
        spec: ToolSpec {
            name: "hold_gate".into(),
            description: "Hold child mid-tool".into(),
            input_schema: json!({"type": "object"}),
        },
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut base = harness(vec![], root.path()).await;
    let sub_cwd = base._root.path().join("sub");
    std::fs::create_dir_all(&sub_cwd).unwrap();
    base.shutdown.cancel();
    (&mut base.handle.task).await.unwrap().unwrap();

    let parent_id = SessionStore::open_read_only(&base.path, &CancellationToken::new())
        .unwrap()
        .header()
        .id
        .clone();
    let mut child_rx = respawn_split_actor(
        &mut base,
        root.path(),
        Arc::new(SteerGateChildren {
            rewind: root.path().join("rewind"),
            gate: gate.clone(),
        }),
        true,
        false,
        (
            vec![
                invoke(
                    "subagent",
                    json!({"task": "initial child work", "name": "worker", "cwd": "sub", "background": true}),
                ),
                answer("BG_STARTED"),
                invoke(
                    "subagent_send",
                    json!({"to": "worker", "message": "steered mid-tool"}),
                ),
                answer("STEER_QUEUED"),
                answer("CONTINUATION_ACK_1"),
                invoke("create_goal", json!({"objective": "track resumed child tokens"})),
                invoke(
                    "subagent_send",
                    json!({"to": "ag1", "message": "foreground resume turn", "background": false}),
                ),
                invoke("update_goal", json!({"status": "complete"})),
                answer("FG_RESUME_ACK"),
            ],
            vec![
                invoke("hold_gate", json!({})),
                answer("CHILD_STEERED_DONE"),
                answer("CHILD_FG_RESUMED_DONE"),
            ],
        ),
    )
    .await;

    submit_and_wait_gate(&mut base, &gate, "launch worker").await;
    let _child_req_1 = tokio::time::timeout(Duration::from_secs(5), child_rx.recv())
        .await
        .unwrap()
        .unwrap();

    let child_file = root
        .path()
        .join("state/subagents/sessions")
        .join(&parent_id)
        .join("ag1.jsonl");
    assert!(child_file.is_file(), "expected {}", child_file.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&child_file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(child_file.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    let listed_top =
        tiechui_session::list(root.path(), base._root.path(), &CancellationToken::new()).unwrap();
    assert!(
        !listed_top.sessions.iter().any(|s| s.path == child_file),
        "child session must never appear in top-level session list"
    );

    base.input("steer worker while running", InputDelivery::FollowUp)
        .await;
    tokio::time::timeout(Duration::from_secs(5), base.end(TurnStatus::Completed))
        .await
        .unwrap();
    let _ = base.request().await;
    let steer_ack_req = base.request().await;
    let steer_results = results(&steer_ack_req);
    let steer_tool_result = steer_results.last().unwrap();
    assert!(!steer_tool_result.is_error);
    assert!(
        steer_tool_result
            .content
            .iter()
            .any(|c| matches!(c, Content::Text { text, .. } if text.contains("ag1") && text.contains("Queued"))),
        "{steer_tool_result:?}"
    );

    gate.release.notify_one();
    let child_req_2 = tokio::time::timeout(Duration::from_secs(5), child_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        users(&child_req_2),
        ["initial child work", "steered mid-tool"]
    );
    assert!(
        results(&child_req_2)[0]
            .content
            .iter()
            .any(|c| matches!(c, Content::Text { text, .. } if text.contains("sub"))),
        "child tool must run in custom sub cwd"
    );

    let cont_req = tokio::time::timeout(Duration::from_secs(5), base.request())
        .await
        .expect("steered background child must wake idle continuation");
    let cont_user = users(&cont_req).pop().unwrap();
    assert!(cont_user.contains("CHILD_STEERED_DONE"), "{cont_user}");
    base.end(TurnStatus::Completed).await;

    base.input("resume worker in foreground", InputDelivery::FollowUp)
        .await;
    tokio::time::timeout(Duration::from_secs(5), base.end(TurnStatus::Completed))
        .await
        .unwrap();
    let _ = base.request().await;
    let _ = base.request().await;
    let fg_resume_parent_req = base.request().await;
    let _ = base.request().await;
    assert!(
        results(&fg_resume_parent_req)
            .last()
            .unwrap()
            .content
            .iter()
            .any(|c| matches!(c, Content::Text { text, .. } if text.contains("CHILD_FG_RESUMED_DONE") && text.contains("[agent_id: ag1]")))
    );
    let child_req_3 = tokio::time::timeout(Duration::from_secs(5), child_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        users(&child_req_3),
        [
            "initial child work",
            "steered mid-tool",
            "foreground resume turn"
        ]
    );

    let snap_before_restart = base.snapshot().await;
    let child_usages: Vec<&serde_json::Value> = snap_before_restart
        .entries()
        .iter()
        .filter_map(|e| match &e.kind {
            EntryKind::Custom { kind, data } if kind == "runtime.usage" => Some(&data["usage"]),
            _ => None,
        })
        .collect();
    assert_eq!(child_usages.len(), 2);
    assert_eq!(child_usages[0]["input"], 6);
    assert_eq!(child_usages[1]["input"], 3);
    let goal_before = snap_before_restart
        .entries()
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EntryKind::Custom { kind, data } if kind == "tiechui.goal.state" => Some(data),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        goal_before["goal"]["tokens_used"], 36,
        "resumed child must report only incremental usage, never double-counting prior runs"
    );

    base.shutdown.cancel();
    (&mut base.handle.task).await.unwrap().unwrap();

    let mut c2_rx = respawn_split_actor(
        &mut base,
        root.path(),
        Arc::new(SteerGateChildren {
            rewind: root.path().join("rewind"),
            gate: gate.clone(),
        }),
        false,
        false,
        (
            vec![
                invoke(
                    "subagent_send",
                    json!({"to": "worker", "message": "after process restart", "background": true}),
                ),
                answer("RESTART_BG_ACK"),
                invoke("subagent_list", json!({})),
                answer("LISTED_AFTER_RESTART"),
                answer("RESTART_CONT_SUMMARY"),
            ],
            vec![
                invoke("hold_gate", json!({})),
                answer("AFTER_RESTART_CHILD_DONE"),
            ],
        ),
    )
    .await;

    submit_and_wait_gate(&mut base, &gate, "resume worker after restart").await;
    gate.release.notify_one();

    let c2_req_1 = tokio::time::timeout(Duration::from_secs(5), c2_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let c2_req_2 = tokio::time::timeout(Duration::from_secs(5), c2_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        users(&c2_req_1),
        [
            "initial child work",
            "steered mid-tool",
            "foreground resume turn",
            "after process restart"
        ]
    );
    assert!(
        results(&c2_req_2)[0]
            .content
            .iter()
            .any(|c| matches!(c, Content::Text { text, .. } if text.contains("sub"))),
        "resumed child after parent restart must retain custom sub cwd from child store header"
    );

    let restart_cont = tokio::time::timeout(Duration::from_secs(5), base.request())
        .await
        .expect("resumed background child after restart must wake continuation");
    assert!(
        users(&restart_cont)
            .pop()
            .unwrap()
            .contains("AFTER_RESTART_CHILD_DONE")
    );
    base.end(TurnStatus::Completed).await;

    base.input("check list deduplication", InputDelivery::FollowUp)
        .await;
    tokio::time::timeout(Duration::from_secs(5), base.end(TurnStatus::Completed))
        .await
        .unwrap();
    let _ = base.request().await;
    let list_after_req = base.request().await;
    let list_text = results(&list_after_req)
        .last()
        .unwrap()
        .content
        .iter()
        .find_map(|c| match c {
            Content::Text { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        list_text.lines().count(),
        1,
        "subagent_list must deduplicate resumed ag1 into a single line: {list_text}"
    );
    assert!(list_text.contains("ag1 (worker)"), "{list_text}");
    assert!(list_text.contains("5t"), "{list_text}");

    base.valid_transcript().await;
    base.stop().await;
}

struct ModuleForkWireRouter;

impl wiremock::Respond for ModuleForkWireRouter {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        let parsed: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
        let last_msg = last_wire_message(&parsed);
        let sse = if last_msg.contains("Follow up on Fork A") {
            anthropic_text_sse("MODULE_FORK_A_RESUMED")
        } else if last_msg.contains("MODULE_FORK_A_RESUMED") {
            anthropic_text_sse("PARENT_RESUMED_FORK_DONE")
        } else if last_msg.contains("Forked subagents cannot spawn or manage subagents") {
            anthropic_text_sse("MODULE_FORK_B_RECOVERED")
        } else if last_msg.contains("call_mod_write_a") {
            anthropic_text_sse("MODULE_FORK_A_DONE")
        } else if last_msg.contains("Directive: Task A: write module fork file") {
            anthropic_tool_calls_sse(
                None,
                None,
                &[(
                    "call_mod_write_a",
                    "write",
                    json!({"path": "mod-fork-a.txt", "content": "module fork a"}),
                )],
            )
        } else if last_msg.contains("Directive: Task B: attempt recursive subagent") {
            anthropic_tool_calls_sse(
                None,
                None,
                &[
                    ("call_mod_rec_1", "subagent", json!({"task": "nested"})),
                    (
                        "call_mod_rec_2",
                        "subagent_send",
                        json!({"to": "ag1", "message": "nested send"}),
                    ),
                ],
            )
        } else if last_msg.contains("<task-notification>") {
            anthropic_text_sse("PARENT_FORK_NOTIFIED_SUMMARY")
        } else if last_msg.contains("Subagent launched in the background") {
            anthropic_text_sse("PARENT_FORKS_LAUNCHED_ACK")
        } else if last_msg.contains("Resume Fork A synchronously") {
            anthropic_tool_calls_sse(
                None,
                None,
                &[(
                    "call_send_fork_a",
                    "subagent_send",
                    json!({
                        "to": "fa",
                        "message": "Follow up on Fork A",
                        "background": false
                    }),
                )],
            )
        } else {
            anthropic_tool_calls_sse(
                Some(("Parent deep reasoning.", "sig-mod-fork")),
                Some("Launching two parallel forked workers."),
                &[
                    (
                        "call_sub_fork_1",
                        "subagent",
                        json!({
                            "task": "Task A: write module fork file",
                            "name": "fa",
                            "fork": true
                        }),
                    ),
                    (
                        "call_sub_fork_2",
                        "subagent",
                        json!({
                            "task": "Task B: attempt recursive subagent",
                            "name": "fb",
                            "fork": true
                        }),
                    ),
                ],
            )
        };
        wiremock::ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream")
    }
}

#[tokio::test]
async fn subagent_module_fork_mode_shares_1h_cache_prefix_wraps_boilerplate_once_and_blocks_recursion()
 {
    use wiremock::{Mock, MockServer, matchers::method};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ModuleForkWireRouter)
        .mount(&server)
        .await;

    let provider = anthropic_long_cache_provider(server.uri());

    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let home = root.path().join("profile");
    let mut registry = Registry::default();
    registry.register_context_provider(Arc::new(StaticContext(vec![
        "Workspace coding instructions.".into(),
    ])));
    tiechui_module_editing::EditingModule
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    tiechui_module_subagents::SubagentsModule {
        home: home.clone(),
        trusted: true,
        confirmation: None,
        children: Arc::new(Children(home.join("rewind"))),
    }
    .register(&mut registry, &ModuleConfig::default())
    .unwrap();

    let parent_store = SessionStore::create(
        &home.join("sessions"),
        root.path(),
        &CancellationToken::new(),
    )
    .unwrap();
    let parent_id = parent_store.header().id.clone();
    let parent_thread = ThreadId(parent_id.clone());
    let shutdown = CancellationToken::new();
    let mut parent =
        spawn_anthropic_parent(parent_store, provider, registry, shutdown.clone()).await;

    parent
        .submissions
        .send(Submission {
            id: SubmissionId("fork-turn-1".into()),
            thread_id: parent_thread.clone(),
            epoch: SessionEpoch(0),
            op: Op::UserInput {
                content: vec![InputContent::Text {
                    text: "Run two parallel forks".into(),
                }],
                delivery: InputDelivery::FollowUp,
            },
        })
        .await
        .unwrap();

    let wait_summary = async {
        let mut both_settled = false;
        let mut pending = usize::MAX;
        let mut in_continuation_turn = false;
        loop {
            match parent.events.recv().await.unwrap().event {
                EventKind::ItemCompleted {
                    item: Item::Known(KnownItem::AssistantMessage { text, .. }),
                    ..
                } if text == "PARENT_FORK_NOTIFIED_SUMMARY" => {
                    in_continuation_turn = true;
                }
                EventKind::Extension { name, data } if name == SUBAGENTS_EVENT => {
                    let settled = data["records"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .flat_map(|r| r["tasks"].as_array().into_iter().flatten())
                        .filter(|t| t["state"] == "Completed")
                        .count();
                    both_settled = settled == 2;
                    pending = data["pending_notifications"].as_u64().unwrap_or(0) as usize;
                }
                EventKind::TurnCompleted { turn } => {
                    assert_eq!(turn.status, TurnStatus::Completed);
                    if in_continuation_turn && both_settled && pending == 0 {
                        break;
                    }
                    in_continuation_turn = false;
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), wait_summary)
        .await
        .expect("both background forks must complete and trigger continuation summary");

    assert_eq!(
        std::fs::read_to_string(root.path().join("mod-fork-a.txt")).unwrap(),
        "module fork a"
    );

    parent
        .submissions
        .send(Submission {
            id: SubmissionId("fork-turn-2".into()),
            thread_id: parent_thread,
            epoch: SessionEpoch(0),
            op: Op::UserInput {
                content: vec![InputContent::Text {
                    text: "Resume Fork A synchronously".into(),
                }],
                delivery: InputDelivery::FollowUp,
            },
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let EventKind::TurnCompleted { turn } = parent.events.recv().await.unwrap().event {
                assert_eq!(turn.status, TurnStatus::Completed);
                break;
            }
        }
    })
    .await
    .unwrap();

    shutdown.cancel();
    parent.task.await.unwrap().unwrap();

    for agent_id in ["ag1", "ag2"] {
        assert!(
            home.join("state/subagents/sessions")
                .join(&parent_id)
                .join(format!("{agent_id}.jsonl"))
                .is_file()
        );
    }

    let bodies = wire_request_bodies(&server).await;
    let parent_initial = &bodies[0];
    let fork_a = bodies
        .iter()
        .find(|b| last_wire_message(b).contains("Directive: Task A: write module fork file"))
        .expect("missing Fork A initial wire request");
    let fork_b = bodies
        .iter()
        .find(|b| last_wire_message(b).contains("Directive: Task B: attempt recursive subagent"))
        .expect("missing Fork B initial wire request");
    let resumed_fork_a = bodies
        .iter()
        .find(|b| {
            let msg = last_wire_message(b);
            msg.contains("Follow up on Fork A") && !msg.contains("call_send_fork_a")
        })
        .expect("missing Fork A resumed wire request");

    for fb in [fork_a, fork_b] {
        assert_fork_wire_parity(fb, parent_initial);
    }
    let a_msg2 = fork_a["messages"][2]["content"].as_array().unwrap();
    let b_msg2 = fork_b["messages"][2]["content"].as_array().unwrap();
    assert_eq!(
        serde_json::to_vec(&a_msg2[..2]).unwrap(),
        serde_json::to_vec(&b_msg2[..2]).unwrap()
    );
    let directive_a = a_msg2[2]["text"].as_str().unwrap();
    assert!(directive_a.contains("<fork_boilerplate>"), "{directive_a}");
    assert!(
        directive_a.contains("Directive: Task A: write module fork file"),
        "{directive_a}"
    );
    assert_eq!(
        resumed_fork_a
            .to_string()
            .matches("<fork_boilerplate>")
            .count(),
        1,
        "subagent_send resume on fork must not wrap <fork_boilerplate> a second time"
    );
}
