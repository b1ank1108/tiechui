use super::*;
use tiechui_plugin_api::{Module, ModuleConfig};
async fn harness(scripts: Vec<Script>) -> Harness {
    Harness::with_contributions(
        scripts,
        Arc::new(BoundedRetry {
            max_retries: 0,
            ..Default::default()
        }),
        json!({"type":"object"}),
        |r| {
            tiechui_module_goal::GoalModule
                .register(r, &ModuleConfig::default())
                .unwrap();
        },
    )
    .await
}
async fn command(h: &mut Harness, args: &str) {
    h.send(Op::RunCommand {
        name: "goal".into(),
        arguments: args.into(),
    })
    .await;
    h.until(|e| matches!(e, EventKind::CommandOutput { name, .. } if name == "goal"))
        .await;
}
fn tool(name: &str, args: serde_json::Value) -> Script {
    reply(vec![Content::ToolCall(ToolCall {
        id: format!("call-{name}"),
        name: name.into(),
        arguments: args,
        opaque: None,
    })])
}
async fn state(h: &Harness) -> serde_json::Value {
    h.snapshot()
        .await
        .entries()
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EntryKind::Custom { kind, data } if kind == "tiechui.goal.state" => Some(data.clone()),
            _ => None,
        })
        .unwrap_or(serde_json::Value::Null)
}
async fn quiet(h: &mut Harness) {
    assert!(
        tokio::time::timeout(Duration::from_millis(90), h.requests.recv())
            .await
            .is_err()
    );
}
#[tokio::test]
async fn hidden_continuations_settle_then_complete_and_never_become_user_history() {
    let mut h = harness(vec![
        tool("get_goal", json!({})),
        answer("first done"),
        tool("get_goal", json!({})),
        answer("second done"),
        tool("update_goal", json!({"status":"complete"})),
        answer("verified complete"),
    ])
    .await;
    command(&mut h, "verify local artifacts").await;
    for _ in 0..3 {
        h.end(TurnStatus::Completed).await;
    }
    for _ in 0..6 {
        h.request().await;
    }
    command(&mut h, "").await;
    quiet(&mut h).await;
    let saved = h.snapshot().await;
    assert!(!saved.entries().iter().any(
        |e| matches!(&e.kind, EntryKind::Message { message, .. } if message.role == Role::User)
    ));
    let current = state(&h).await;
    assert_eq!(current["goal"]["status"], "complete");
    assert_eq!(current["goal"]["tokens_used"], 54);
    h.stop().await;
}
#[tokio::test]
async fn draft_pause_and_clear_win_before_continuation() {
    let mut h = harness(vec![answer("no tools"), answer("ordinary input")]).await;
    h.send(Op::InputState { blocked: true }).await;
    command(&mut h, "task").await;
    quiet(&mut h).await;
    command(&mut h, "pause").await;
    h.send(Op::InputState { blocked: false }).await;
    quiet(&mut h).await;
    command(&mut h, "resume").await;
    let request = h.request().await;
    assert!(users(&request).iter().any(|s| s.contains("<active_goal")));
    assert_eq!(request.messages[0].role, Role::System);
    assert!(
        !serde_json::to_string(&request.messages[0])
            .unwrap()
            .contains("task")
    );
    h.end(TurnStatus::Completed).await;
    quiet(&mut h).await;
    assert_eq!(state(&h).await["deferred"], true);
    h.input("new input", InputDelivery::FollowUp).await;
    h.request().await;
    h.end(TurnStatus::Completed).await;
    command(&mut h, "clear").await;
    quiet(&mut h).await;
    assert!(state(&h).await["goal"].is_null());
    h.stop().await;
}
#[tokio::test]
async fn tools_restrict_creation_updates_and_retry_uses_current_controls() {
    let mut h = harness(vec![
        tool("create_goal", json!({"objective":"explicit"})),
        tool("update_goal", json!({"status":"paused"})),
        answer("done"),
        tool("create_goal", json!({"objective":"automatic forbidden"})),
        tool("update_goal", json!({"status":"complete"})),
        answer("complete"),
    ])
    .await;
    h.input("create a goal explicitly", InputDelivery::FollowUp)
        .await;
    for _ in 0..2 {
        h.end(TurnStatus::Completed).await;
    }
    let saved = h.snapshot().await;
    let text = serde_json::to_string(saved.entries()).unwrap();
    assert!(
        text.contains("Model updates accept only complete or blocked")
            || text.contains("Validation failed for tool")
    );
    assert!(text.contains("Automatic continuation cannot create a new goal"));
    assert_eq!(state(&h).await["goal"]["status"], "complete");
    h.stop().await;
}
#[tokio::test]
async fn explicit_pause_during_stream_stops_clock_and_esc_pauses_continuation() {
    let (release, wait) = oneshot::channel();
    let mut h = harness(vec![Script {
        error: None,
        steps: VecDeque::from([Step::Wait(wait), Step::Event(finish(StopReason::EndTurn))]),
    }])
    .await;
    command(&mut h, "task").await;
    h.request().await;
    let turn = h.started_turn().await;
    h.send(Op::Interrupt { turn_id: turn }).await;
    h.end(TurnStatus::Interrupted).await;
    quiet(&mut h).await;
    assert_eq!(state(&h).await["goal"]["status"], "paused");
    drop(release);
    h.stop().await;
}
#[tokio::test]
async fn quota_error_blocks_and_retry_reads_latest_projection() {
    let failure = Script {
        error: Some(provider_error(
            ErrorKind::InvalidRequest,
            "insufficient_quota",
        )),
        steps: VecDeque::new(),
    };
    let mut h = harness(vec![failure, answer("retry")]).await;
    command(&mut h, "task").await;
    h.request().await;
    h.end(TurnStatus::Failed).await;
    command(&mut h, "").await;
    assert_eq!(state(&h).await["goal"]["status"], "usage_limited");
    command(&mut h, "clear").await;
    h.send(Op::Retry).await;
    let request = h.request().await;
    assert!(users(&request).iter().any(|s| s.contains("status: none")));
    h.end(TurnStatus::Completed).await;
    quiet(&mut h).await;
    h.stop().await;
}

#[tokio::test]
async fn queued_user_input_precedes_continuation_after_a_fully_settled_turn() {
    let (release, wait) = oneshot::channel();
    let mut h = harness(vec![
        tool("get_goal", json!({})),
        Script {
            error: None,
            steps: VecDeque::from([Step::Wait(wait), Step::Event(finish(StopReason::EndTurn))]),
        },
        answer("queued input"),
        answer("defer"),
    ])
    .await;
    command(&mut h, "task").await;
    h.request().await;
    h.request().await;
    h.input("priority user input", InputDelivery::FollowUp)
        .await;
    h.until(|e| matches!(e, EventKind::QueueUpdated { follow_up, .. } if !follow_up.is_empty()))
        .await;
    release.send(()).unwrap();
    h.end(TurnStatus::Completed).await;
    let request = h.request().await;
    assert!(users(&request).iter().any(|s| s == "priority user input"));
    assert_eq!(
        users(&request).iter().rev().nth(1).unwrap(),
        "priority user input"
    );
    h.end(TurnStatus::Completed).await;
    h.request().await;
    h.end(TurnStatus::Completed).await;
    quiet(&mut h).await;
    h.stop().await;
}

#[tokio::test]
async fn reload_defers_and_branch_navigation_pauses_old_branch_and_reads_target_state() {
    let mut h = harness(vec![]).await;
    h.send(Op::InputState { blocked: true }).await;
    command(&mut h, "persistent").await;
    let saved = h.snapshot().await;
    let leaf = saved.current_leaf().unwrap().to_owned();
    let old = state(&h).await;
    h.send(Op::Reload).await;
    h.epoch = h
        .until(|e| matches!(e, EventKind::ThreadStarted { .. }))
        .await
        .epoch;
    command(&mut h, "").await;
    assert_eq!(state(&h).await["goal"]["id"], old["goal"]["id"]);
    assert_eq!(state(&h).await["deferred"], true);
    h.send(Op::InputState { blocked: false }).await;
    quiet(&mut h).await;
    h.send(Op::Branch {
        entry_id: saved.header().id.clone(),
    })
    .await;
    h.epoch = h
        .until(|e| matches!(e, EventKind::ThreadStarted { .. }))
        .await
        .epoch;
    let view = h.send(Op::RunCommand {
        name: "goal".into(),
        arguments: String::new(),
    });
    view.await;
    let output = h
        .until(|e| matches!(e, EventKind::CommandOutput { name, .. } if name == "goal"))
        .await;
    assert!(
        matches!(output.event, EventKind::CommandOutput { text, .. } if text.starts_with("No goal set"))
    );
    h.send(Op::Branch { entry_id: leaf }).await;
    h.epoch = h
        .until(|e| matches!(e, EventKind::ThreadStarted { .. }))
        .await
        .epoch;
    command(&mut h, "").await;
    assert_eq!(state(&h).await["goal"]["status"], "paused");
    quiet(&mut h).await;
    h.stop().await;
}

#[tokio::test]
async fn fork_before_goal_creation_inherits_current_parent_and_defers() {
    let mut h = harness(vec![]).await;
    h.send(Op::InputState { blocked: true }).await;
    command(&mut h, "parent current goal").await;
    let parent = h.snapshot().await;
    let fork = parent
        .fork(&parent.header().id, &CancellationToken::new())
        .unwrap();
    let path = fork.path().to_owned();
    let shutdown = CancellationToken::new();
    let mut registry = Registry::default();
    tiechui_module_goal::GoalModule
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let (tx, mut requests) = mpsc::unbounded_channel();
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::new()),
        requests: tx,
    });
    let mut handle = SessionActor::spawn(
        fork,
        SessionConfig {
            compaction: Default::default(),
            model: origin("a"),
            thinking: None,
            providers: BTreeMap::from([("fake".into(), provider as Arc<dyn ModelProvider>)]),
            registry,
            retry: Arc::new(BoundedRetry::default()),
            interactions: None,
            epoch: SessionEpoch(0),
            short_cache: false,
        },
        shutdown.clone(),
    );
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), handle.events.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(event.event, EventKind::Extension { name, data } if name == "tiechui.goal.status" && data.as_str().is_some_and(|s| s.starts_with("Goal ready")))
        {
            break;
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(90), requests.recv())
            .await
            .is_err()
    );
    shutdown.cancel();
    handle.task.await.unwrap().unwrap();
    let child = SessionStore::open(&path, &CancellationToken::new()).unwrap();
    let latest = child
        .entries()
        .iter()
        .rev()
        .find_map(|e| match &e.kind {
            EntryKind::Custom { kind, data } if kind == "tiechui.goal.state" => Some(data),
            _ => None,
        })
        .unwrap();
    assert_eq!(latest["goal"]["id"], state(&h).await["goal"]["id"]);
    assert_eq!(latest["deferred"], true);
    h.stop().await;
}

#[tokio::test]
async fn main_questions_block_idle_work_until_answered() {
    let mut h = harness(vec![answer("defer")]).await;
    h.send(Op::InputState { blocked: true }).await;
    command(&mut h, "task").await;
    let interactions = h.interactions.clone();
    let ask = tokio::spawn(async move {
        interactions
            .ask(
                ApprovalRequest::Questions {
                    questions: vec![Question {
                        id: "choice".into(),
                        header: "Choice".into(),
                        question: "Which?".into(),
                        options: None,
                    }],
                },
                CancellationToken::new(),
            )
            .await
    });
    let prompt = h
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested { approval_id, .. } = prompt.event else {
        unreachable!()
    };
    h.send(Op::InputState { blocked: false }).await;
    quiet(&mut h).await;
    h.send(Op::ApprovalResponse {
        approval_id,
        decision: ApprovalDecision::Cancel,
    })
    .await;
    ask.await.unwrap();
    h.request().await;
    h.end(TurnStatus::Completed).await;
    assert_eq!(state(&h).await["goal"]["status"], "active");
    h.stop().await;
}

#[tokio::test]
async fn blocker_reports_on_third_turn_stop_and_resume_restarts_the_audit() {
    let mut scripts = vec![];
    for _ in 0..2 {
        scripts.extend([
            tool("get_goal", json!({})),
            answer("blocker persists; investigating"),
            tool("get_goal", json!({})),
            answer("same blocker; investigating"),
            tool("update_goal", json!({"status":"blocked"})),
            answer("same blocker on third run; external input required"),
        ]);
    }
    let mut h = harness(scripts).await;
    command(&mut h, "resolve the blocked fixture").await;
    for audit in 0..2 {
        if audit == 1 {
            command(&mut h, "resume").await;
        }
        for _ in 0..3 {
            h.end(TurnStatus::Completed).await;
        }
        for _ in 0..6 {
            let request = h.request().await;
            let current = users(&request).last().unwrap().clone();
            if current.contains("status: active") {
                assert!(
                    current.contains("same blocker")
                        && current.contains("three consecutive goal runs")
                        && current.contains("resume starts a fresh blocked audit")
                );
            }
            assert!(
                request
                    .tools
                    .iter()
                    .find(|t| t.name == "update_goal")
                    .unwrap()
                    .description
                    .contains("three consecutive goal runs")
            );
        }
        command(&mut h, "").await;
        assert_eq!(state(&h).await["goal"]["status"], "blocked");
        quiet(&mut h).await;
    }
    h.stop().await;
}

#[tokio::test]
async fn pause_command_during_execution_settles_time_without_cancelling_the_current_request() {
    let (release, wait) = oneshot::channel();
    let mut h = harness(vec![Script {
        error: None,
        steps: VecDeque::from([Step::Wait(wait), Step::Event(finish(StopReason::EndTurn))]),
    }])
    .await;
    command(&mut h, "task").await;
    h.request().await;
    command(&mut h, "pause").await;
    let used = state(&h).await["goal"]["time_used_seconds"].clone();
    release.send(()).unwrap();
    h.end(TurnStatus::Completed).await;
    command(&mut h, "").await;
    quiet(&mut h).await;
    assert_eq!(state(&h).await["goal"]["time_used_seconds"], used);
    assert_eq!(state(&h).await["goal"]["status"], "paused");
    h.stop().await;
}

#[tokio::test]
async fn rewind_preview_freezes_without_mutation_and_confirmation_pauses_old_goal() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let mut h = Harness::with_contributions(
        vec![],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| {
            tiechui_module_goal::GoalModule
                .register(registry, &ModuleConfig::default())
                .unwrap();
            tiechui_module_rewind::RewindModule {
                directory: root.path().join("rewind"),
            }
            .register(registry, &ModuleConfig::default())
            .unwrap();
        },
    )
    .await;
    h.send(Op::InputState { blocked: true }).await;
    command(&mut h, "goal-only checkpoint").await;
    h.send(Op::RunCommand {
        name: "rewind".into(),
        arguments: String::new(),
    })
    .await;
    let (id, _) = rewind_panel(&mut h, RewindPage::Messages).await;
    h.send(Op::InputState { blocked: false }).await;
    quiet(&mut h).await;
    assert_eq!(state(&h).await["goal"]["status"], "active");
    h.send(Op::InputState { blocked: true }).await;
    h.send(Op::ApprovalResponse {
        approval_id: id,
        decision: ApprovalDecision::Cancel,
    })
    .await;
    h.until(|e| matches!(e, EventKind::CommandOutput { name, .. } if name == "rewind"))
        .await;
    assert_eq!(state(&h).await["goal"]["status"], "active");
    h.send(Op::RunCommand {
        name: "rewind".into(),
        arguments: String::new(),
    })
    .await;
    let (id, panel) = rewind_panel(&mut h, RewindPage::Messages).await;
    assert_eq!(panel.rows.len(), 1);
    assert_eq!(panel.rows[0].text, "/goal goal-only checkpoint");
    let row = &panel.rows[0];
    h.send(Op::ApprovalResponse {
        approval_id: id,
        decision: ApprovalDecision::Accept {
            content: Some(
                json!({"action": "preview","session_id":row.session_id,"entry_id":row.entry_id}),
            ),
        },
    })
    .await;
    let (approval_id, _) = rewind_panel(&mut h, RewindPage::Actions).await;
    h.send(Op::ApprovalResponse { approval_id, decision: ApprovalDecision::Accept { content: Some(json!({"action":"conversation","session_id":row.session_id,"entry_id":row.entry_id})) } }).await;
    h.epoch = h
        .until(|e| matches!(e, EventKind::CommandOutput { name, .. } if name == "rewind"))
        .await
        .epoch;
    h.send(Op::RunCommand {
        name: "goal".into(),
        arguments: String::new(),
    })
    .await;
    let view = h
        .until(|e| matches!(e, EventKind::CommandOutput { name, .. } if name == "goal"))
        .await;
    assert!(
        matches!(view.event, EventKind::CommandOutput { text, .. } if text.starts_with("No goal set"))
    );
    let saved = h.snapshot().await;
    assert!(saved.entries().iter().any(|e| matches!(&e.kind, EntryKind::Custom { kind, data } if kind == "tiechui.goal.state" && data["goal"]["status"] == "paused")));
    h.send(Op::InputState { blocked: false }).await;
    quiet(&mut h).await;
    h.stop().await;
}
async fn rewind_panel(h: &mut Harness, page: RewindPage) -> (ApprovalId, RewindPanel) {
    let event = h.until(|e| matches!(e, EventKind::ApprovalRequested { request: ApprovalRequest::Rewind { panel }, .. } if panel.page == page)).await;
    let EventKind::ApprovalRequested {
        approval_id,
        request: ApprovalRequest::Rewind { panel },
        ..
    } = event.event
    else {
        unreachable!()
    };
    (approval_id, panel)
}

#[tokio::test]
async fn failed_empty_responses_count_reported_usage_once() {
    let mut h = harness(vec![Script::events(vec![
        StreamEvent::Usage {
            usage: Usage {
                input: 100,
                output: 20,
                cache_write: 5,
                cache_read: 1000,
                reasoning: 10,
                ..Default::default()
            },
        },
        StreamEvent::Finish {
            reason: StopReason::Error,
            usage: Usage {
                input: 100,
                output: 20,
                cache_write: 5,
                cache_read: 1000,
                reasoning: 10,
                ..Default::default()
            },
        },
    ])])
    .await;
    command(&mut h, "task").await;
    h.request().await;
    h.end(TurnStatus::Failed).await;
    command(&mut h, "").await;
    let state = state(&h).await;
    assert_eq!(state["goal"]["tokens_used"], 125);
    assert_eq!(state["goal"]["usage_complete"], true);
    assert_eq!(state["goal"]["status"], "blocked");
    quiet(&mut h).await;
    h.stop().await;
}

struct Scheduled(Mutex<Option<u64>>);
impl tiechui_plugin_api::RuntimeExtension for Scheduled {
    fn event(
        &self,
        event: &tiechui_plugin_api::RuntimeEvent,
        _: &tiechui_plugin_api::BranchView,
    ) -> Result<tiechui_plugin_api::RuntimeOutput, ToolError> {
        if let tiechui_plugin_api::RuntimeEvent::Input {
            continuation: Some(c),
            ..
        } = event
            && c.kind == "test.scheduled"
        {
            *self.0.lock().unwrap() = None;
        }
        Ok(Default::default())
    }
    fn continuation(&self) -> Option<tiechui_plugin_api::Continuation> {
        self.0
            .lock()
            .unwrap()
            .map(|generation| tiechui_plugin_api::Continuation {
                kind: "test.scheduled".into(),
                generation,
                identity: "job".into(),
                text: "check $probe-skill now".into(),
                visible: true,
            })
    }
    fn accepts(&self, next: &tiechui_plugin_api::Continuation) -> bool {
        self.continuation()
            .is_some_and(|c| c.generation == next.generation)
    }
}

struct Inputs(Mutex<Vec<Vec<InputContent>>>);
#[async_trait]
impl tiechui_plugin_api::ContextProvider for Inputs {
    async fn fragments(
        &self,
        input: &tiechui_plugin_api::TurnInput,
        _: CancellationToken,
    ) -> Result<Vec<String>, ToolError> {
        self.0.lock().unwrap().push(input.content.clone());
        Ok(vec![])
    }
}

#[tokio::test]
async fn other_automatic_turns_take_turns_show_their_input_and_leave_the_goal_alone() {
    let scheduled = Arc::new(Scheduled(Mutex::new(None)));
    let inputs = Arc::new(Inputs(Mutex::new(vec![])));
    let (extension, provider) = (scheduled.clone(), inputs.clone());
    let mut h = Harness::with_contributions(
        vec![
            tool("get_goal", json!({})),
            answer("goal step"),
            tool("create_goal", json!({"objective":"from schedule"})),
            answer("checked"),
            answer("goal again"),
        ],
        Arc::new(BoundedRetry {
            max_retries: 0,
            ..Default::default()
        }),
        json!({"type":"object"}),
        |r| {
            tiechui_module_goal::GoalModule
                .register(r, &ModuleConfig::default())
                .unwrap();
            r.register_runtime(extension);
            r.register_context_provider(provider);
        },
    )
    .await;
    command(&mut h, "task").await;
    h.request().await;
    *scheduled.0.lock().unwrap() = Some(1);
    h.request().await;
    h.end(TurnStatus::Completed).await;
    let request = h.request().await;
    assert_eq!(
        users(&request).iter().rev().nth(1).unwrap(),
        "check $probe-skill now"
    );
    let status = users(&request)
        .into_iter()
        .find(|s| s.starts_with("Current persistent goal status: active"))
        .unwrap();
    assert!(!status.contains("<active_goal"));
    h.until(|e| {
        matches!(e, EventKind::ItemCompleted { item: Item::Known(KnownItem::UserMessage { content, .. }), .. }
            if content == &vec![InputContent::Text { text: "check $probe-skill now".into() }])
    })
    .await;
    h.request().await;
    h.end(TurnStatus::Completed).await;
    // The goal continues after the scheduled turn, which a tool-free goal turn would have deferred.
    h.request().await;
    h.end(TurnStatus::Completed).await;
    quiet(&mut h).await;
    assert!(inputs.lock_contains("check $probe-skill now"));
    let saved = h.snapshot().await;
    assert!(saved.entries().iter().any(|e| matches!(&e.kind,
        EntryKind::CustomMessage { hidden: false, kind, .. } if kind == "test.scheduled")));
    let text = serde_json::to_string(saved.entries()).unwrap();
    assert!(text.contains("Automatic continuation cannot create a new goal"));
    let current = state(&h).await;
    assert_eq!(current["goal"]["objective"], "task");
    assert_eq!(current["goal"]["tokens_used"], 27);
    h.stop().await;
}

impl Inputs {
    fn lock_contains(&self, text: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .iter()
            .any(|content| content == &vec![InputContent::Text { text: text.into() }])
    }
}
