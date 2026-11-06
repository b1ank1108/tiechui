use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

struct Empty;
impl ChildRegistry for Empty {
    fn build(&self, _: &ToolContext, _: &str) -> Result<Registry, ToolError> {
        Ok(Registry::default())
    }
}
#[derive(Default)]
struct Runner {
    current: AtomicUsize,
    maximum: AtomicUsize,
    started: AtomicUsize,
    cleaned: AtomicUsize,
    tasks: Mutex<Vec<String>>,
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
    async fn run(
        &self,
        request: ChildRequest,
        _: Registry,
        cancel: CancellationToken,
        events: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<(), ToolError> {
        self.tasks.lock().unwrap().push(request.task.clone());
        if request.task == "panic" {
            panic!("isolated fixture");
        }
        self.started.fetch_add(1, Ordering::SeqCst);
        let active = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
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
    runner: Arc<Runner>,
    ctx: ToolContext,
    entries: Arc<Mutex<Vec<tools::CustomSessionEntry>>>,
}
impl Harness {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let runner = Arc::new(Runner::default());
        let service = Arc::new(Service {
            home: root.path().join("home"),
            trusted: false,
            confirmation: None,
            children: Arc::new(Empty),
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
            runner,
            ctx,
            entries,
        }
    }
    async fn run(&self, value: Value) -> Result<AgentDispatch, ToolError> {
        let result = self
            .service
            .dispatch(
                ToolCall {
                    id: "dispatch".into(),
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
#[tokio::test]
async fn modes_limits_chain_substitution_and_real_failure_skip() {
    let h = Harness::new();
    let single = h
        .run(json!({"agent":"worker","task":"literal $& {x}"}))
        .await
        .unwrap();
    assert_eq!(single.tasks[0].output, "literal $& {x}");
    assert_eq!(single.tasks[0].usage.input, 7);
    let chain = h.run(json!({"chain":[{"agent":"worker","task":"first"},{"agent":"worker","task":"{previous} + {previous}"},{"agent":"worker","task":"fail"},{"agent":"worker","task":"never"}]})).await.unwrap();
    assert_eq!(chain.tasks[1].output, "first + first");
    assert_eq!(chain.tasks[2].state, AgentState::Failed);
    assert_eq!(chain.tasks[3].state, AgentState::Skipped);
    assert!(!h.runner.tasks.lock().unwrap().iter().any(|t| t == "never"));
    for value in [
        json!({}),
        json!({"tasks":[]}),
        json!({"agent":"worker","task":"a","chain":[]}),
        json!({"tasks":vec![json!({"agent":"worker","task":"x"});9]}),
    ] {
        assert!(h.run(value).await.is_err());
    }
    let parallel = h.run(json!({"tasks":(0..8).map(|i| json!({"agent":"worker","task":i.to_string()})).collect::<Vec<_>>()})).await.unwrap();
    assert_eq!(h.runner.maximum.load(Ordering::SeqCst), 4);
    for (i, t) in parallel.tasks.iter().enumerate() {
        assert_eq!(t.output, i.to_string());
        assert_eq!(t.state, AgentState::Completed);
    }
}
#[tokio::test]
async fn cancellation_waits_for_cleanup_starts_nothing_new_and_retains_finished() {
    let h = Harness::new();
    let cancel = h.ctx.cancel.clone();
    let runner = h.runner.clone();
    let result = h.run(json!({"tasks":std::iter::once(json!({"agent":"worker","task":"done"})).chain((0..7).map(|_| json!({"agent":"worker","task":"wait"}))).collect::<Vec<_>>()}));
    let stop = async move {
        while runner.cleaned.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        cancel.cancel();
    };
    let (result, ()) = tokio::join!(result, stop);
    let result = result.unwrap();
    assert_eq!(result.tasks[0].state, AgentState::Completed);
    assert!(
        result.tasks[1..]
            .iter()
            .all(|t| t.state == AgentState::Cancelled)
    );
    assert!(h.runner.started.load(Ordering::SeqCst) <= 5);
    assert_eq!(h.runner.current.load(Ordering::SeqCst), 0);
    assert_eq!(
        h.runner.started.load(Ordering::SeqCst),
        h.runner.cleaned.load(Ordering::SeqCst)
    );
}
#[tokio::test]
async fn panic_scope_and_stale_completion_are_isolated() {
    let h = Harness::new();
    let result = h.run(json!({"tasks":[{"agent":"worker","task":"panic"},{"agent":"worker","task":"still works"}]})).await.unwrap();
    assert_eq!(result.tasks[0].state, AgentState::Failed);
    assert!(
        result.tasks[0]
            .diagnostic
            .as_ref()
            .unwrap()
            .contains("panicked")
    );
    assert_eq!(result.tasks[1].state, AgentState::Completed);
    let result = h.run(json!({"tasks":[{"agent":"worker","task":"fail"},{"agent":"worker","task":"survives sibling error"}]})).await.unwrap();
    assert_eq!(result.tasks[0].state, AgentState::Failed);
    assert_eq!(result.tasks[1].state, AgentState::Completed);
    let outside = tempfile::tempdir().unwrap();
    let result = h
        .run(json!({"agent":"worker","task":"escape","cwd":outside.path()}))
        .await
        .unwrap();
    assert!(
        result.tasks[0]
            .diagnostic
            .as_ref()
            .unwrap()
            .contains("parent workspace")
    );
    let run = h.run(json!({"agent":"worker","task":"wait"}));
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
    let result = h.run(json!({"chain":[{"agent":"worker","task":text},{"agent":"worker","task":"{previous}"}]})).await.unwrap();
    assert_eq!(result.tasks[0].output, text);
    let mut parallel = result.clone();
    parallel.mode = "parallel".into();
    let summary = super::result(&parallel);
    let Content::Text { text: summary, .. } = &summary.content[0] else {
        unreachable!()
    };
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
        .run(json!({"agent":"worker","task":"x".repeat(8 * 1024 * 1024 + 1)}))
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
