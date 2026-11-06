use super::*;
use tiechui_plugin_api::{Module, ModuleConfig};

async fn harness(scripts: Vec<Script>) -> Harness {
    Harness::with_contributions(
        scripts,
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| {
            tiechui_module_btw::BtwModule
                .register(registry, &ModuleConfig::default())
                .unwrap();
        },
    )
    .await
}
async fn command(h: &mut Harness, arguments: &str) {
    h.send(Op::RunCommand {
        name: "btw".into(),
        arguments: arguments.into(),
    })
    .await;
}
async fn action(h: &mut Harness, action: BtwAction) {
    h.send(Op::CommandInput {
        name: "btw".into(),
        data: serde_json::to_value(action).unwrap(),
    })
    .await;
}
async fn side(h: &mut Harness, predicate: impl Fn(&BtwView) -> bool) -> BtwView {
    let event = h.until(|event| match event {
        EventKind::Extension { name, data } if name == BTW_EVENT => matches!(serde_json::from_value::<BtwEvent>(data.clone()).unwrap(), BtwEvent::Update { view, .. } if predicate(&view)),
        _ => false,
    }).await;
    let EventKind::Extension { data, .. } = event.event else {
        unreachable!()
    };
    let BtwEvent::Update { view, .. } = serde_json::from_value(data).unwrap() else {
        unreachable!()
    };
    view
}
#[tokio::test]
async fn side_runs_during_main_stream_and_share_is_one_custom_message_for_next_request() {
    let (release, wait) = oneshot::channel();
    let main = Script {
        error: None,
        steps: VecDeque::from([Step::Wait(wait), Step::Event(finish(StopReason::EndTurn))]),
    };
    let mut h = harness(vec![main, answer("SIDE_ANSWER"), answer("NEXT_MAIN")]).await;
    h.input("MAIN_CONTEXT", InputDelivery::FollowUp).await;
    let main_turn = h.started_turn().await;
    let main_request = h.request().await;
    assert!(!main_request.tools.is_empty());
    command(&mut h, "SIDE_QUESTION").await;
    let request = h.request().await;
    assert!(!request.tools.is_empty());
    assert_eq!(request.model.0, "fake/a");
    let system = serde_json::to_string(&request.messages[0]).unwrap();
    assert!(
        system.contains("MAIN_CONTEXT")
            && system.contains("base")
            && system.contains("Tools are available")
    );
    let view = side(&mut h, |view| {
        !view.busy && view.messages.last().is_some_and(|m| m.complete)
    })
    .await;
    assert_eq!(view.messages[1].text, "SIDE_ANSWER");
    assert_eq!(view.total.cost_nano_usd, Some(5000));
    let before = h.snapshot().await.entries().len();
    action(&mut h, BtwAction::Share).await;
    side(&mut h, |view| {
        view.notice
            .as_ref()
            .is_some_and(|s| s.contains("Shared with main"))
    })
    .await;
    assert!(h.requests.try_recv().is_err());
    assert!(!h.seen.iter().any(
        |event| matches!(&event.event, EventKind::TurnCompleted { turn } if turn.id == main_turn)
    ));
    let saved = h.snapshot().await;
    assert_eq!(saved.entries().len(), before + 1);
    assert!(
        matches!(&saved.entries().last().unwrap().kind, EntryKind::CustomMessage { text, .. } if text.ends_with("SIDE_ANSWER"))
    );
    assert!(
        !serde_json::to_string(saved.entries())
            .unwrap()
            .contains("SIDE_QUESTION")
    );
    release.send(()).unwrap();
    h.end(TurnStatus::Completed).await;
    action(&mut h, BtwAction::Close).await;
    h.input("NEXT_QUESTION", InputDelivery::FollowUp).await;
    let next = h.request().await;
    assert!(users(&next).iter().any(|text| text.contains("SIDE_ANSWER")));
    assert!(
        !serde_json::to_string(&main_request)
            .unwrap()
            .contains("SIDE_ANSWER")
    );
    h.end(TurnStatus::Completed).await;
    h.stop().await;
}
#[tokio::test]
async fn stopping_and_epoch_navigation_discard_only_side_state() {
    let (release, wait) = oneshot::channel();
    let (_side_release, side_wait) = oneshot::channel();
    let main = Script {
        error: None,
        steps: VecDeque::from([Step::Wait(wait), Step::Event(finish(StopReason::EndTurn))]),
    };
    let held = Script {
        error: None,
        steps: VecDeque::from([
            Step::Event(StreamEvent::TextDelta {
                index: 0,
                delta: "SIDE_PARTIAL".into(),
            }),
            Step::Wait(side_wait),
        ]),
    };
    let mut h = harness(vec![main, held]).await;
    h.input("MAIN", InputDelivery::FollowUp).await;
    let turn = h.started_turn().await;
    h.request().await;
    command(&mut h, "question").await;
    h.request().await;
    side(&mut h, |v| {
        v.messages.last().is_some_and(|m| m.text == "SIDE_PARTIAL")
    })
    .await;
    action(&mut h, BtwAction::Stop).await;
    side(&mut h, |v| {
        !v.busy && v.messages.last().is_some_and(|m| m.interrupted)
    })
    .await;
    assert!(
        !h.seen
            .iter()
            .any(|e| matches!(&e.event, EventKind::TurnCompleted { turn: t } if t.id == turn))
    );
    release.send(()).unwrap();
    h.end(TurnStatus::Completed).await;
    let leaf = h.snapshot().await.current_leaf().unwrap().to_owned();
    for op in [Op::Reload, Op::Branch { entry_id: leaf }] {
        h.send(op).await;
        let event = h
            .until(|e| matches!(e, EventKind::ThreadStarted { .. }))
            .await;
        h.epoch = event.epoch;
        command(&mut h, "").await;
        let view = side(&mut h, |v| v.messages.is_empty()).await;
        assert!(!view.busy);
    }
    h.stop().await;
}

#[tokio::test]
async fn side_tools_edit_and_read_shared_files_without_adding_to_main_history() {
    let (release, wait) = oneshot::channel();
    let main = Script {
        error: None,
        steps: VecDeque::from([Step::Wait(wait), Step::Event(finish(StopReason::EndTurn))]),
    };
    let tool = |id: &str, name: &str, arguments| {
        Content::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
            opaque: None,
        })
    };
    let mut h = Harness::with_contributions(
        vec![
            main,
            reply(vec![
                tool(
                    "write",
                    "write",
                    json!({"path":"side.txt","content":"side contents"}),
                ),
                tool("read", "read", json!({"path":"side.txt"})),
            ]),
            answer("SIDE_DONE"),
            answer("FOLLOWUP"),
        ],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| {
            tiechui_module_btw::BtwModule
                .register(registry, &ModuleConfig::default())
                .unwrap();
            tiechui_module_editing::EditingModule
                .register(registry, &ModuleConfig::default())
                .unwrap();
        },
    )
    .await;
    h.input("MAIN", InputDelivery::FollowUp).await;
    let main_turn = h.started_turn().await;
    h.request().await;
    let before = h.snapshot().await.entries().len();
    command(
        &mut h,
        "Create side.txt with side contents and read it back.",
    )
    .await;
    let first = h.request().await;
    assert!(first.tools.iter().any(|tool| tool.name == "write"));
    let continuation = h.request().await;
    let results: Vec<_> = continuation
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            Content::ToolResult(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| !result.is_error));
    assert!(
        serde_json::to_string(results[1])
            .unwrap()
            .contains("side contents")
    );
    let view = side(&mut h, |view| !view.busy).await;
    assert!(view.messages.last().unwrap().text.contains("SIDE_DONE"));
    assert_eq!(view.total.input, 6);
    assert_eq!(
        std::fs::read_to_string(h._root.path().join("side.txt")).unwrap(),
        "side contents"
    );
    assert_eq!(h.snapshot().await.entries().len(), before);
    assert!(!h.seen.iter().any(
        |event| matches!(&event.event, EventKind::TurnCompleted { turn } if turn.id == main_turn)
    ));
    command(&mut h, "What did you read?").await;
    let followup = h.request().await;
    assert!(
        serde_json::to_string(&followup.messages)
            .unwrap()
            .contains("side contents")
    );
    side(&mut h, |view| !view.busy).await;
    release.send(()).unwrap();
    h.end(TurnStatus::Completed).await;
    h.stop().await;
}

#[tokio::test]
async fn side_calls_obey_the_same_permission_denials() {
    use tiechui_module_permissions::{HeadlessPolicy, PermissionsModule};
    let root = tempfile::tempdir().unwrap();
    let mut config = ModuleConfig::default();
    config.permissions.rules = Some(
        serde_json::from_value(json!([{"surface":"probe","pattern":"*","action":"deny"}])).unwrap(),
    );
    let permissions = PermissionsModule::headless(
        root.path().into(),
        root.path().into(),
        HeadlessPolicy::Unavailable,
    )
    .with_mode(tiechui_plugin_api::PermissionMode::Yolo);
    let mut h = Harness::with_contributions(
        vec![reply(vec![call("denied")]), answer("DENIED")],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| {
            tiechui_module_btw::BtwModule
                .register(registry, &config)
                .unwrap();
            permissions.register(registry, &config).unwrap();
        },
    )
    .await;
    command(&mut h, "Try the probe.").await;
    h.request().await;
    let next = h.request().await;
    assert!(
        next.messages
            .iter()
            .flat_map(|message| &message.content)
            .any(|part| matches!(part, Content::ToolResult(result) if result.is_error))
    );
    assert_eq!(h.tool.runs.load(Ordering::SeqCst), 0);
    side(&mut h, |view| !view.busy).await;
    h.stop().await;
}

#[tokio::test]
async fn stopping_a_side_tool_settles_it_and_keeps_tool_pairs_for_followup() {
    let mut h = harness(vec![reply(vec![call("held")]), answer("RECOVERED")]).await;
    command(&mut h, "").await;
    let view = side(&mut h, |view| view.messages.is_empty()).await;
    let (_release, wait) = oneshot::channel();
    h.tool
        .gates
        .lock()
        .unwrap()
        .insert(format!("command-{}-held", view.id), wait);
    command(&mut h, "Run the probe.").await;
    h.request().await;
    h.started.recv().await.unwrap();
    action(&mut h, BtwAction::Stop).await;
    side(&mut h, |view| {
        !view.busy
            && view
                .messages
                .last()
                .is_some_and(|message| message.interrupted)
    })
    .await;
    assert!(h.requests.try_recv().is_err());
    command(&mut h, "Continue answering without the probe.").await;
    let next = h.request().await;
    assert!(next.messages.iter().flat_map(|message| &message.content).any(|part| matches!(part, Content::ToolResult(result) if result.call_id == "held" && result.is_error)));
    side(&mut h, |view| !view.busy).await;
    h.stop().await;
}
