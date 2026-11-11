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
        if catalog.is_child {
            return Ok(vec![]);
        }
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
        let thinking = call.arguments["thinking"]
            .as_str()
            .map(tiechui_plugin_api::parse_child_thinking)
            .transpose()?;
        let mut child_registry = Registry::default();
        let nested_probe = if call.arguments["probe_child_catalog"] == true {
            let nested = Arc::new(ChildProbe::default());
            child_registry.register_actor_client(nested.clone());
            child_registry.register_context_provider(nested.clone());
            Some(nested)
        } else {
            None
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
                    model: call.arguments["model"].as_str().map(str::to_owned),
                    thinking,
                    tools: None,
                },
                child_registry,
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
        let child_catalog_is_child = nested_probe.and_then(|nested| {
            let bound = nested.runner.lock().unwrap().clone()?;
            Some(bound.model_catalog().is_child)
        });
        Ok(ToolOutput {
            details: Some(json!({
                "model": *seen_model.lock().unwrap(),
                "child_catalog_is_child": child_catalog_is_child,
            })),
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

    let mut alpha_requests = Vec::new();
    while let Ok(req) = f.alpha_rx.try_recv() {
        alpha_requests.push(req);
    }
    assert_eq!(alpha_requests.len(), 9);
    assert!(alpha_requests.iter().all(|r| r.model.0 == "alpha/fast"));

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
async fn session_model_catalog_tracks_active_model_excludes_unready_and_marks_child_sessions() {
    let mut f = MultiProviderFixture::new(
        vec![
            invoke(
                "spawn_child",
                json!({"task": "inspect child catalog", "model": "beta/judge", "probe_child_catalog": true}),
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

    let tool_output = &f.tool_results()[0].0;
    assert_eq!(tool_output["details"]["child_catalog_is_child"], true);

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
        summary_text.contains("### [default · beta/judge] Completed"),
        "{summary_text}"
    );
    assert!(
        summary_text.contains("### [pinned · alpha/fast] Completed"),
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
