use super::*;
use tiechui_plugin_api::{ChildRegistry, Module, ModuleConfig};

struct Children(PathBuf);
impl ChildRegistry for Children {
    fn build(&self, parent: &ToolContext, _: &str) -> Result<Registry, ToolError> {
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
            invoke("subagent", json!({"agent":"worker","task":"child task"})),
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
    assert!(
        !serde_json::to_string(&child)
            .unwrap()
            .contains("parent task")
    );
    assert_eq!(
        std::fs::read_to_string(h._root.path().join("child.txt")).unwrap(),
        "child edit"
    );
    let snapshot = h.snapshot().await;
    let records = records(&snapshot);
    let record = records.last().unwrap();
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
            invoke("subagent", json!({"agent":"worker","task":"panic"})),
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
    let mut h = harness(vec![invoke("subagent", json!({"tasks":[{"agent":"worker","task":"quick"},{"agent":"worker","task":"wait"}]})), answer("FINISHED_CHILD"), Script { error: None, steps: VecDeque::from([Step::Wait(wait), Step::Event(finish(StopReason::EndTurn))]) }], root.path()).await;
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
            invoke("subagent", json!({"agent":"worker","task":"child"})),
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
            invoke("subagent", json!({"agent":"planner","task":"plan"})),
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
