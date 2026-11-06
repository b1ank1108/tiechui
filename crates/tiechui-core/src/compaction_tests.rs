use super::*;
use tiechui_plugin_api::{
    CompactionConfig, CompactionContext, CompactionOutcome, CompactionResult, Compactor,
    RestoreContext, RestoredContext,
};
use tiechui_plugin_api::{Module, ModuleConfig};

async fn harness(
    scripts: Vec<Script>,
    enabled: bool,
    configure: impl FnOnce(&mut Registry),
) -> Harness {
    Harness::with_compaction(
        scripts,
        Arc::new(BoundedRetry {
            max_retries: 2,
            initial_delay: Duration::ZERO,
            max_delay: Duration::ZERO,
        }),
        json!({"type":"object"}),
        configure,
        CompactionConfig {
            enabled,
            reserve_tokens: 1000,
            keep_recent_tokens: 10,
            ..Default::default()
        },
    )
    .await
}

async fn conversation(h: &mut Harness) {
    for text in ["old history ".repeat(40), "recent input ".repeat(10)] {
        h.input(&text, InputDelivery::FollowUp).await;
        h.request().await;
        h.end(TurnStatus::Completed).await;
    }
}

fn text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn manual_compaction_updates_summary_resumes_and_invalidates_usage_and_recovery() {
    let mut h = harness(
        vec![
            answer("first"),
            answer("second"),
            answer("SUMMARY_ONE"),
            answer("third"),
            answer("SUMMARY_TWO"),
            answer("after resume"),
        ],
        false,
        |_| {},
    )
    .await;
    conversation(&mut h).await;
    h.send(Op::Compact {
        instructions: Some("preserve decisions exactly".into()),
    })
    .await;
    let request = h.request().await;
    assert!(request.tools.is_empty());
    assert!(request.disable_cache);
    assert_eq!(request.model, ModelId("a".into()));
    assert!(text(&request).contains("Additional focus: preserve decisions exactly"));
    assert!(text(&request).contains("old history"));
    assert!(!text(&request).contains("recent input"));
    h.end(TurnStatus::Completed).await;
    let store = h.snapshot().await;
    let compacted = store.build_context();
    assert!(format!("{compacted:?}").contains("SUMMARY_ONE"));
    assert!(!format!("{compacted:?}").contains("old history"));
    assert_eq!(
        estimate::input(&store, &HashSet::new(), &compacted, &[]).unwrap(),
        tiechui_providers::estimate::messages(&compacted)
    );
    assert!(
        h.seen
            .iter()
            .any(|event| matches!(event.event, EventKind::SessionUpdated { context: None, .. }))
    );
    h.input(&"third input ".repeat(10), InputDelivery::FollowUp)
        .await;
    let next = h.request().await;
    assert!(text(&next).contains("SUMMARY_ONE"));
    assert!(!text(&next).contains("old history"));
    h.end(TurnStatus::Completed).await;
    h.send(Op::Compact { instructions: None }).await;
    let update = h.request().await;
    assert!(text(&update).contains("<previous-summary>\nSUMMARY_ONE\n</previous-summary>"));
    assert!(text(&update).contains("PRESERVE all existing information"));
    h.end(TurnStatus::Completed).await;
    h.send(Op::Reload).await;
    let event = h
        .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    h.epoch = event.epoch;
    let EventKind::ThreadStarted { thread, .. } = event.event else {
        unreachable!()
    };
    assert!(thread.turns.iter().flat_map(|turn| &turn.items).any(|item| matches!(item, Item::Known(KnownItem::Compaction { summary, .. }) if summary == "SUMMARY_TWO")));
    h.input("after resume", InputDelivery::FollowUp).await;
    let request = h.request().await;
    assert!(text(&request).contains("SUMMARY_TWO"));
    assert!(!text(&request).contains("SUMMARY_ONE"));
    h.end(TurnStatus::Completed).await;
    h.stop().await;
}

#[tokio::test]
async fn overflow_compacts_and_retries_only_once_and_preserves_pending_input() {
    let mut h = harness(
        vec![
            answer("first"),
            answer("second"),
            Script::error(ErrorKind::ContextOverflow),
            answer("OVERFLOW_SUMMARY"),
            Script::error(ErrorKind::ContextOverflow),
        ],
        true,
        |_| {},
    )
    .await;
    conversation(&mut h).await;
    h.input(&"pending request ".repeat(10), InputDelivery::FollowUp)
        .await;
    h.request().await;
    let summary = h.request().await;
    assert!(summary.tools.is_empty());
    let retry = h.request().await;
    assert!(text(&retry).contains("OVERFLOW_SUMMARY"));
    assert!(text(&retry).contains("pending request"));
    assert!(!text(&retry).contains("old history"));
    h.end(TurnStatus::Failed).await;
    assert_eq!(
        h.snapshot()
            .await
            .entries()
            .iter()
            .filter(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
            .count(),
        1
    );
    assert!(h.requests.try_recv().is_err());
    h.stop().await;
}

#[tokio::test]
async fn automatic_threshold_uses_usage_and_drains_queued_input_after_summary() {
    let (release, gate) = oneshot::channel();
    let high = Script::events(vec![
        StreamEvent::ContentComplete {
            index: 0,
            content: text_content("second".into()),
        },
        StreamEvent::Finish {
            reason: StopReason::EndTurn,
            usage: Usage {
                input: 9500,
                ..Default::default()
            },
        },
    ]);
    let summary = Script {
        error: None,
        steps: VecDeque::from([
            Step::Wait(gate),
            Step::Event(StreamEvent::ContentComplete {
                index: 0,
                content: text_content("AUTO_SUMMARY".into()),
            }),
            Step::Event(finish(StopReason::EndTurn)),
        ]),
    };
    let mut h = harness(
        vec![answer("first"), high, summary, answer("queued answer")],
        true,
        |_| {},
    )
    .await;
    h.input(&"old history ".repeat(40), InputDelivery::FollowUp)
        .await;
    h.request().await;
    h.end(TurnStatus::Completed).await;
    h.input(&"recent input ".repeat(10), InputDelivery::FollowUp)
        .await;
    h.request().await;
    h.request().await;
    h.until(|event| {
        matches!(
            event,
            EventKind::CompactionStarted {
                reason: CompactionReason::Threshold
            }
        )
    })
    .await;
    h.input("queued while compacting", InputDelivery::FollowUp)
        .await;
    release.send(()).unwrap();
    h.end(TurnStatus::Completed).await;
    let queued = h.request().await;
    assert!(text(&queued).contains("AUTO_SUMMARY"));
    assert!(text(&queued).contains("queued while compacting"));
    assert!(!text(&queued).contains("old history"));
    h.end(TurnStatus::Completed).await;
    h.stop().await;
}

#[tokio::test]
async fn cancellation_and_incomplete_or_tool_summaries_leave_session_unchanged() {
    for failure in ["cancel", "length", "tool", "error"] {
        let (_release, gate) = oneshot::channel();
        let script = match failure {
            "cancel" => Script {
                error: None,
                steps: VecDeque::from([Step::Wait(gate)]),
            },
            "length" => Script::events(vec![
                StreamEvent::TextDelta {
                    index: 0,
                    delta: "partial".into(),
                },
                finish(StopReason::Length),
            ]),
            "tool" => reply(vec![call("unexpected")]),
            _ => Script::error(ErrorKind::Auth),
        };
        let mut h = harness(
            vec![answer("first"), answer("second"), script],
            false,
            |_| {},
        )
        .await;
        conversation(&mut h).await;
        let before = std::fs::read(&h.path).unwrap();
        h.send(Op::Compact { instructions: None }).await;
        let turn = h.started_turn().await;
        h.request().await;
        if failure == "cancel" {
            h.send(Op::Interrupt { turn_id: turn }).await;
        }
        h.end(if failure == "cancel" {
            TurnStatus::Interrupted
        } else {
            TurnStatus::Failed
        })
        .await;
        assert_eq!(std::fs::read(&h.path).unwrap(), before, "{failure}");
        assert_eq!(h.tool.runs.load(Ordering::SeqCst), 0);
        assert!(h.seen.iter().any(|event| matches!(
            &event.event,
            EventKind::CompactionEnded { aborted: true, .. }
                | EventKind::CompactionEnded { error: Some(_), .. }
        )));
        h.stop().await;
    }
}

struct Contributor(bool);
#[async_trait]
impl Compactor for Contributor {
    async fn before_compact(
        &self,
        ctx: &CompactionContext,
        _: CancellationToken,
    ) -> Result<CompactionOutcome, ToolError> {
        assert_eq!(ctx.reason, CompactionReason::Manual);
        Ok(if self.0 {
            CompactionOutcome::Cancel
        } else {
            CompactionOutcome::Summary(CompactionResult {
                summary: "MODULE_SUMMARY".into(),
                first_kept_entry_id: ctx.preparation.first_kept_entry_id.clone(),
                tokens_before: ctx.preparation.tokens_before,
                usage: None,
                details: json!({"module": true}),
            })
        })
    }
}

#[tokio::test]
async fn contributors_cancel_or_supply_without_model_calls() {
    for cancel in [true, false] {
        let mut h = harness(vec![answer("first"), answer("second")], false, |registry| {
            registry.register_compactor(Arc::new(Contributor(cancel)))
        })
        .await;
        conversation(&mut h).await;
        let before = std::fs::read(&h.path).unwrap();
        h.send(Op::Compact { instructions: None }).await;
        h.end(if cancel {
            TurnStatus::Failed
        } else {
            TurnStatus::Completed
        })
        .await;
        if cancel {
            assert_eq!(std::fs::read(&h.path).unwrap(), before);
        } else {
            assert!(h.snapshot().await.entries().iter().any(|entry| matches!(&entry.kind, EntryKind::Compaction { summary, from_extension: true, .. } if summary == "MODULE_SUMMARY")));
        }
        assert!(h.requests.try_recv().is_err());
        h.stop().await;
    }
}

#[tokio::test]
async fn summary_retries_transient_failure_without_persisting_partial_attempt() {
    let mut h = harness(
        vec![
            answer("first"),
            answer("second"),
            Script::error(ErrorKind::TransientNetwork),
            answer("RETRIED_SUMMARY"),
        ],
        false,
        |_| {},
    )
    .await;
    conversation(&mut h).await;
    let before = h.snapshot().await.entries().len();
    h.send(Op::Compact { instructions: None }).await;
    let initial = h.request().await;
    assert_eq!(h.request().await, initial);
    h.end(TurnStatus::Completed).await;
    assert_eq!(h.snapshot().await.entries().len(), before + 1);
    h.stop().await;
}

#[tokio::test]
async fn split_turn_generates_separate_checkpoints_with_the_same_custom_focus() {
    let mut h = Harness::with_compaction(
        vec![
            answer("first"),
            answer("second"),
            answer("HISTORY_SUMMARY"),
            answer("TURN_SUMMARY"),
        ],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |_| {},
        CompactionConfig {
            enabled: false,
            keep_recent_tokens: 0,
            ..Default::default()
        },
    )
    .await;
    conversation(&mut h).await;
    h.send(Op::Compact {
        instructions: Some("keep decisions".into()),
    })
    .await;
    let history = h.request().await;
    let prefix = h.request().await;
    assert!(text(&history).contains("old history"));
    assert!(!text(&history).contains("recent input"));
    assert!(text(&prefix).contains("# Instructions\nThe messages above are earlier context"));
    assert!(text(&prefix).contains("recent input"));
    for request in [history, prefix] {
        assert!(text(&request).contains("Additional focus: keep decisions"));
    }
    h.end(TurnStatus::Completed).await;
    let store = h.snapshot().await;
    assert!(store.entries().iter().any(|entry| matches!(&entry.kind, EntryKind::Compaction { summary, .. } if summary == "HISTORY_SUMMARY\n\n---\n\n**Turn Context (split turn):**\n\nTURN_SUMMARY")));
    h.stop().await;
}

#[tokio::test]
async fn goal_continuation_waits_for_compaction() {
    let (release, gate) = oneshot::channel();
    let summary = Script {
        error: None,
        steps: VecDeque::from([
            Step::Wait(gate),
            Step::Event(StreamEvent::ContentComplete {
                index: 0,
                content: text_content("GOAL_CHECKPOINT".into()),
            }),
            Step::Event(finish(StopReason::EndTurn)),
        ]),
    };
    let mut h = harness(
        vec![
            answer("first"),
            answer("second"),
            summary,
            answer("goal continued"),
        ],
        false,
        |registry| {
            tiechui_module_goal::GoalModule
                .register(registry, &ModuleConfig::default())
                .unwrap()
        },
    )
    .await;
    conversation(&mut h).await;
    h.send(Op::InputState { blocked: true }).await;
    h.send(Op::RunCommand {
        name: "goal".into(),
        arguments: "finish the implementation".into(),
    })
    .await;
    h.until(|event| matches!(event, EventKind::CommandOutput { name, .. } if name == "goal"))
        .await;
    h.send(Op::Compact { instructions: None }).await;
    h.request().await;
    h.send(Op::InputState { blocked: false }).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), h.requests.recv())
            .await
            .is_err()
    );
    release.send(()).unwrap();
    h.end(TurnStatus::Completed).await;
    let next = h.request().await;
    assert!(text(&next).contains("GOAL_CHECKPOINT"));
    assert!(text(&next).contains("<active_goal"));
    h.end(TurnStatus::Completed).await;
    h.stop().await;
}

#[tokio::test]
async fn precompact_hook_is_dispatched_by_the_contribution() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let capture = root.path().join("precompact.json");
    let command = format!("cat > '{}'", capture.display());
    std::fs::write(
        home.join("hooks.json"),
        json!({"hooks":{"PreCompact":[{"hooks":[{"type":"command","command":command}]}]}})
            .to_string(),
    )
    .unwrap();
    let hooks = tiechui_module_hooks::HooksModule::load(
        root.path(),
        &home,
        false,
        Default::default(),
        &CancellationToken::new(),
    )
    .unwrap();
    let mut h = harness(
        vec![answer("first"), answer("second"), answer("HOOK_CHECKPOINT")],
        false,
        |registry| hooks.register(registry, &ModuleConfig::default()).unwrap(),
    )
    .await;
    conversation(&mut h).await;
    h.send(Op::Compact { instructions: None }).await;
    h.request().await;
    h.end(TurnStatus::Completed).await;
    let input: serde_json::Value =
        serde_json::from_slice(&std::fs::read(capture).unwrap()).unwrap();
    assert_eq!(input["hook_event_name"], "PreCompact");
    assert_eq!(input["trigger"], "manual");
    assert_eq!(input["session_id"], h.thread.0);
    assert!(input.get("permission_mode").is_none());
    h.stop().await;
}

#[tokio::test]
async fn cache_inspection_invalidates_old_usage_and_navigation_restores_across_compaction() {
    let mut h = harness(
        vec![
            answer("first"),
            answer("second"),
            answer("CHECKPOINT"),
            answer("after"),
        ],
        false,
        |registry| {
            tiechui_module_context::ContextModule
                .register(registry, &ModuleConfig::default())
                .unwrap()
        },
    )
    .await;
    conversation(&mut h).await;
    let before = h.snapshot().await;
    let leaf = before.current_leaf().unwrap().to_owned();
    h.send(Op::Compact { instructions: None }).await;
    h.request().await;
    h.end(TurnStatus::Completed).await;
    h.send(Op::RunCommand {
        name: "context".into(),
        arguments: "cache".into(),
    })
    .await;
    let event = h
        .until(|event| matches!(event, EventKind::CommandPanel { .. }))
        .await;
    let EventKind::CommandPanel { panel, .. } = event.event else {
        unreachable!()
    };
    assert!(panel.provider_usage.is_none());
    h.send(Op::Branch { entry_id: leaf }).await;
    let event = h
        .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    h.epoch = event.epoch;
    assert_eq!(h.snapshot().await.build_context(), before.build_context());
    h.input("after navigation", InputDelivery::FollowUp).await;
    let next = h.request().await;
    assert!(text(&next).contains("old history"));
    assert!(!text(&next).contains("CHECKPOINT"));
    h.end(TurnStatus::Completed).await;
    h.stop().await;
}

#[tokio::test]
async fn range_module_keeps_native_capacity_fallback_available() {
    let module = tiechui_module_compaction::CompactionModule;
    let mut h = harness(
        vec![
            answer("first"),
            answer("second"),
            answer("NATIVE_FALLBACK"),
            answer("after"),
        ],
        false,
        |registry| {
            module.register(registry, &ModuleConfig::default()).unwrap();
            tiechui_module_editing::EditingModule
                .register(registry, &ModuleConfig::default())
                .unwrap();
        },
    )
    .await;
    conversation(&mut h).await;
    h.send(Op::Compact { instructions: None }).await;
    let request = h.request().await;
    assert!(request.tools.is_empty());
    assert!(request.disable_cache);
    h.end(TurnStatus::Completed).await;
    h.input("continue", InputDelivery::FollowUp).await;
    let request = h.request().await;
    assert!(text(&request).contains("NATIVE_FALLBACK"));
    assert!(
        request
            .tools
            .iter()
            .any(|tool| tool.name == "compact_history")
    );
    h.end(TurnStatus::Completed).await;
    h.stop().await;
}

struct Projector(Mutex<Vec<bool>>);

#[async_trait::async_trait]
impl tiechui_plugin_api::RequestProjection for Projector {
    fn fold(&self, _: &str, _: &serde_json::Value) -> Option<tiechui_plugin_api::ContextFold> {
        None
    }

    async fn project(
        &self,
        context: tiechui_plugin_api::ProjectionContext,
        _: CancellationToken,
    ) -> Result<Option<tiechui_plugin_api::ProjectedRequest>, tiechui_plugin_api::ToolError> {
        self.0.lock().unwrap().push(context.cache_cold);
        let mut messages: Vec<_> = context
            .messages
            .into_iter()
            .enumerate()
            .map(|(index, message)| tiechui_plugin_api::ProjectedMessage {
                source: Some(index),
                message,
            })
            .collect();
        messages.push(tiechui_plugin_api::ProjectedMessage {
            source: None,
            message: Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: "PROJECTED".into(),
                    opaque: None,
                }],
                opaque: None,
            },
        });
        Ok(Some(tiechui_plugin_api::ProjectedRequest {
            messages,
            entries: vec![("test.projection".into(), json!({"point": true}))],
        }))
    }
}

#[tokio::test]
async fn projection_entries_are_persisted_before_the_view_is_sent() {
    let projector = Arc::new(Projector(Mutex::new(vec![])));
    let registered = projector.clone();
    let mut h = harness(vec![answer("one"), answer("two")], false, move |registry| {
        registry.register_projection(registered)
    })
    .await;
    for input in ["first", "second"] {
        h.input(input, InputDelivery::FollowUp).await;
        let request = h.request().await;
        assert!(text(&request).ends_with("PROJECTED"));
        h.end(TurnStatus::Completed).await;
        let store = h.snapshot().await;
        let kinds: Vec<_> = store
            .entries()
            .iter()
            .rev()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Message { message, .. } => Some(format!("{:?}", message.role)),
                EntryKind::Custom { kind, .. } => Some(kind.clone()),
                _ => None,
            })
            .take(3)
            .collect();
        assert_eq!(kinds, ["Assistant", "test.projection", "User"]);
    }
    // The fake provider reports cache usage but declares no retention, so the cache is never cold.
    assert_eq!(*projector.0.lock().unwrap(), [false, false]);
    h.stop().await;
}

/// An opening request, three tool rounds and an answer, then a short recent turn.
fn tool_round_scripts() -> Vec<Script> {
    let mut scripts: Vec<_> = ["CALL_A", "CALL_B", "CALL_C"]
        .map(|id| {
            reply(vec![
                text_content(format!("{id}_NOTES ").repeat(50)),
                call(id),
            ])
        })
        .into();
    scripts.extend([answer("DONE_ONE"), answer("second")]);
    scripts
}

async fn tool_rounds(h: &mut Harness) {
    h.input(&"FIRST_REQUEST ".repeat(20), InputDelivery::FollowUp)
        .await;
    for _ in 0..4 {
        h.request().await;
    }
    h.end(TurnStatus::Completed).await;
    h.input(&"recent input ".repeat(10), InputDelivery::FollowUp)
        .await;
    h.request().await;
    h.end(TurnStatus::Completed).await;
}

/// Every tool call in a summary input appears together with its result.
fn paired(text: &str) -> bool {
    ["CALL_A", "CALL_B", "CALL_C"].iter().all(|id| {
        text.contains(&format!("probe(value=\"{id}\")"))
            == text.contains(&format!("[Tool result]: {id}"))
    })
}

fn progress(seen: &[Event]) -> Vec<(CompactionPhase, u32, u32)> {
    seen.iter()
        .filter_map(|event| match event.event {
            EventKind::CompactionProgress {
                phase,
                retries,
                shrinks,
                ..
            } => Some((phase, retries, shrinks)),
            _ => None,
        })
        .collect()
}

fn usage_answer(text: &str, input: u64) -> Script {
    Script::events(vec![
        StreamEvent::ContentComplete {
            index: 0,
            content: text_content(text.into()),
        },
        StreamEvent::Finish {
            reason: StopReason::EndTurn,
            usage: Usage {
                input,
                ..Default::default()
            },
        },
    ])
}

fn paused(seen: &[Event]) -> Vec<String> {
    seen.iter()
        .filter_map(|event| match &event.event {
            EventKind::CompactionEnded {
                paused: Some(paused),
                ..
            } => Some(paused.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn degraded_summary_omits_whole_rounds_runs_precompact_once_and_keeps_raw_history() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let capture = root.path().join("precompact.jsonl");
    let command = format!("cat >> '{}'", capture.display());
    std::fs::write(
        home.join("hooks.json"),
        json!({"hooks":{"PreCompact":[{"hooks":[{"type":"command","command":command}]}]}})
            .to_string(),
    )
    .unwrap();
    let hooks = tiechui_module_hooks::HooksModule::load(
        root.path(),
        &home,
        false,
        Default::default(),
        &CancellationToken::new(),
    )
    .unwrap();
    let mut scripts = tool_round_scripts();
    scripts.extend([
        Script::error(ErrorKind::TransientNetwork),
        Script::error(ErrorKind::ContextOverflow),
        answer("SHRUNK_SUMMARY"),
        answer("after"),
    ]);
    let mut h = harness(scripts, false, |registry| {
        hooks.register(registry, &ModuleConfig::default()).unwrap()
    })
    .await;
    tool_rounds(&mut h).await;
    let before = std::fs::read(&h.path).unwrap();
    h.send(Op::Compact { instructions: None }).await;
    let full = h.request().await;
    assert_eq!(h.request().await, full);
    let shrunk = text(&h.request().await);
    h.end(TurnStatus::Completed).await;
    let full = text(&full);
    assert!(full.contains("CALL_A_NOTES") && paired(&full));
    assert!(!shrunk.contains("CALL_A_NOTES") && !shrunk.contains("CALL_B_NOTES"));
    for kept in ["FIRST_REQUEST", "CALL_C_NOTES", "DONE_ONE"] {
        assert!(shrunk.contains(kept), "{kept}");
    }
    assert!(shrunk.contains("[4 earlier messages omitted to fit this request"));
    assert!(paired(&shrunk));
    assert_eq!(
        progress(&h.seen),
        [
            (CompactionPhase::Prepare, 0, 0),
            (CompactionPhase::Summarize, 0, 0),
            (CompactionPhase::Summarize, 1, 0),
            (CompactionPhase::Summarize, 2, 1),
            (CompactionPhase::Restore, 2, 1),
            (CompactionPhase::Commit, 2, 1),
        ]
    );
    assert!(h.seen.iter().any(|event| matches!(
        &event.event,
        EventKind::ItemCompleted {
            item: Item::Known(KnownItem::Compaction { degraded: true, .. }),
            ..
        }
    )));
    assert_eq!(
        std::fs::read_to_string(&capture)
            .unwrap()
            .matches("PreCompact")
            .count(),
        1
    );
    let after = std::fs::read(&h.path).unwrap();
    assert!(after.starts_with(&before));
    let store = h.snapshot().await;
    let first = store
        .entries()
        .iter()
        .find(|entry| format!("{:?}", entry.kind).contains("CALL_A_NOTES"))
        .unwrap();
    let Some(EntryKind::Compaction {
        summary, details, ..
    }) = store.entries().last().map(|entry| &entry.kind)
    else {
        panic!("missing checkpoint");
    };
    assert!(summary.starts_with("Note: 4 earlier messages did not fit the summary request"));
    assert!(summary.contains("they remain in the session history"));
    assert_eq!(details["degraded"], true);
    assert_eq!(details["omitted"][0]["from"], first.id.as_str());
    assert_eq!(details["omitted"][0]["messages"], 4);
    h.input("continue", InputDelivery::FollowUp).await;
    assert!(text(&h.request().await).contains("did not fit the summary request"));
    h.end(TurnStatus::Completed).await;
    h.stop().await;
}

#[tokio::test]
async fn shrink_retries_share_the_request_budget_stop_at_two_and_cancel_cleanly() {
    for case in ["cap", "shared", "cancel"] {
        let (_release, gate) = oneshot::channel();
        let overflow = || Script::error(ErrorKind::ContextOverflow);
        let mut scripts = tool_round_scripts();
        scripts.extend(match case {
            "cap" => vec![overflow(), overflow(), overflow()],
            "shared" => vec![
                Script::error(ErrorKind::TransientNetwork),
                overflow(),
                overflow(),
            ],
            _ => vec![
                overflow(),
                Script {
                    error: None,
                    steps: VecDeque::from([Step::Wait(gate)]),
                },
            ],
        });
        let mut h = harness(scripts, false, |_| {}).await;
        tool_rounds(&mut h).await;
        let before = std::fs::read(&h.path).unwrap();
        h.send(Op::Compact { instructions: None }).await;
        let turn = h.started_turn().await;
        let mut requests = vec![h.request().await, h.request().await];
        if case == "cancel" {
            h.send(Op::Interrupt { turn_id: turn }).await;
        } else {
            requests.push(h.request().await);
        }
        h.end(if case == "cancel" {
            TurnStatus::Interrupted
        } else {
            TurnStatus::Failed
        })
        .await;
        assert_eq!(std::fs::read(&h.path).unwrap(), before, "{case}");
        assert!(h.requests.try_recv().is_err(), "{case}");
        let texts: Vec<_> = requests.iter().map(text).collect();
        assert!(texts.iter().all(|text| paired(text)), "{case}");
        let sizes: Vec<_> = requests
            .iter()
            .map(|request| tiechui_providers::estimate::messages(&request.messages))
            .collect();
        match case {
            "cap" => {
                assert!(sizes[0] > sizes[1] && sizes[1] > sizes[2]);
                assert!(!texts[2].contains("CALL_C_NOTES") && texts[2].contains("DONE_ONE"));
                assert_eq!(
                    progress(&h.seen).last(),
                    Some(&(CompactionPhase::Summarize, 2, 2))
                );
            }
            "shared" => {
                assert_eq!(requests[0], requests[1]);
                assert!(sizes[1] > sizes[2]);
                assert_eq!(
                    progress(&h.seen).last(),
                    Some(&(CompactionPhase::Summarize, 2, 1))
                );
            }
            _ => assert!(sizes[0] > sizes[1]),
        }
        assert!(h.seen.iter().any(|event| match &event.event {
            EventKind::CompactionEnded { aborted, error, .. } => {
                if case == "cancel" {
                    *aborted
                } else {
                    error
                        .as_deref()
                        .is_some_and(|error| error.contains("ContextOverflow"))
                }
            }
            _ => false,
        }));
        h.stop().await;
    }
}

#[tokio::test]
async fn three_automatic_failures_pause_until_the_user_compacts() {
    let auth = || Script::error(ErrorKind::Auth);
    let mut h = harness(
        vec![
            answer("first"),
            answer("second"),
            usage_answer("near the threshold", 8990),
            auth(),
            auth(),
            auth(),
            auth(),
            auth(),
            answer("MANUAL_RESET"),
        ],
        true,
        |_| {},
    )
    .await;
    conversation(&mut h).await;
    h.input("third", InputDelivery::FollowUp).await;
    h.request().await;
    h.end(TurnStatus::Completed).await;
    h.input(&"pressure ".repeat(50), InputDelivery::FollowUp)
        .await;
    assert!(h.request().await.tools.is_empty());
    assert!(!h.request().await.tools.is_empty());
    assert!(h.request().await.tools.is_empty());
    h.end(TurnStatus::Failed).await;
    assert!(paused(&h.seen).is_empty());
    h.send(Op::Retry).await;
    assert!(h.request().await.tools.is_empty());
    assert!(!h.request().await.tools.is_empty());
    h.end(TurnStatus::Failed).await;
    assert!(h.requests.try_recv().is_err());
    assert_eq!(
        paused(&h.seen),
        [
            "Automatic compaction paused after 3 consecutive failures on the same history; it resumes after new input or a model response, or run /compact."
        ]
    );
    let started = |seen: &[Event]| {
        seen.iter()
            .filter(|event| matches!(event.event, EventKind::CompactionStarted { .. }))
            .count()
    };
    assert_eq!(started(&h.seen), 3);
    h.send(Op::Compact { instructions: None }).await;
    assert!(h.request().await.tools.is_empty());
    h.end(TurnStatus::Completed).await;
    assert_eq!(started(&h.seen), 4);
    h.stop().await;
}

#[tokio::test]
async fn checkpoint_without_progress_pauses_until_new_input() {
    let mut h = harness(
        vec![
            answer("first"),
            answer("second"),
            answer("STALLED_SUMMARY"),
            Script::error(ErrorKind::Auth),
            answer("after"),
        ],
        true,
        |_| {},
    )
    .await;
    conversation(&mut h).await;
    h.input(&"huge ".repeat(6400), InputDelivery::FollowUp)
        .await;
    assert!(h.request().await.tools.is_empty());
    assert!(!h.request().await.tools.is_empty());
    h.end(TurnStatus::Failed).await;
    assert!(h.requests.try_recv().is_err());
    let notices = paused(&h.seen);
    assert_eq!(notices.len(), 1);
    assert!(notices[0].contains("after the checkpoint, above the 9000 token threshold"));
    let ended = |seen: &[Event]| {
        seen.iter()
            .filter(|event| matches!(event.event, EventKind::CompactionEnded { .. }))
            .count()
    };
    assert_eq!(ended(&h.seen), 1);
    h.input(&"next input ".repeat(10), InputDelivery::FollowUp)
        .await;
    assert!(!h.request().await.tools.is_empty());
    h.end(TurnStatus::Completed).await;
    assert_eq!(ended(&h.seen), 2);
    assert!(h.seen.iter().any(|event| matches!(
        &event.event,
        EventKind::CompactionEnded { error: Some(error), .. }
            if error.contains("no earlier round can be omitted")
    )));
    h.stop().await;
}

type RestoreCall = (String, String, Vec<RestoredContext>, u64, u64);

#[derive(Default)]
struct Restorer(Mutex<Vec<RestoreCall>>);

#[async_trait]
impl Compactor for Restorer {
    async fn restore(
        &self,
        ctx: &RestoreContext,
        _: CancellationToken,
    ) -> Result<Vec<RestoredContext>, ToolError> {
        let mut calls = self.0.lock().unwrap();
        calls.push((
            format!("{:?}", ctx.summarized),
            format!("{:?}", ctx.retained),
            ctx.previous.clone(),
            ctx.item_tokens,
            ctx.total_tokens,
        ));
        let item = |key: &str, text: String| RestoredContext {
            source: "test".into(),
            key: key.into(),
            version: "1".into(),
            text,
        };
        Ok(if calls.len() == 1 {
            vec![
                item("alpha", "RESTORED_ALPHA".into()),
                item("alpha", "DUPLICATE_ALPHA".into()),
                item("big", "OVERSIZED ".repeat(400)),
                item("beta", "RESTORED_BETA".into()),
            ]
        } else {
            vec![]
        })
    }
}

#[tokio::test]
async fn restored_context_is_budgeted_deduplicated_persisted_and_replayed_once() {
    let restorer = Arc::new(Restorer::default());
    let contribution = restorer.clone();
    let mut h = harness(
        vec![
            answer("first"),
            answer("second"),
            answer("CHECKPOINT_ONE"),
            answer("after reload"),
            answer("CHECKPOINT_TWO"),
            answer("final"),
        ],
        false,
        |registry| registry.register_compactor(contribution),
    )
    .await;
    conversation(&mut h).await;
    h.send(Op::Compact { instructions: None }).await;
    h.request().await;
    h.end(TurnStatus::Completed).await;
    h.send(Op::Reload).await;
    let event = h
        .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    h.epoch = event.epoch;
    h.input("after reload", InputDelivery::FollowUp).await;
    let resumed = text(&h.request().await);
    h.end(TurnStatus::Completed).await;
    assert!(resumed.contains(
        "Context restored after the summary, as of the checkpoint:\n\nRESTORED_ALPHA\n\nRESTORED_BETA"
    ));
    assert!(!resumed.contains("DUPLICATE_ALPHA") && !resumed.contains("OVERSIZED"));
    h.send(Op::Compact { instructions: None }).await;
    assert!(!text(&h.request().await).contains("RESTORED_ALPHA"));
    h.end(TurnStatus::Completed).await;
    h.input("final", InputDelivery::FollowUp).await;
    assert!(!text(&h.request().await).contains("RESTORED_ALPHA"));
    h.end(TurnStatus::Completed).await;
    h.stop().await;
    let calls = restorer.0.lock().unwrap();
    let (summarized, retained, previous, item, total) = &calls[0];
    assert!(summarized.contains("old history") && !summarized.contains("recent input"));
    assert!(retained.contains("recent input"));
    assert!(previous.is_empty());
    assert_eq!((*item, *total), (250, 1000));
    assert_eq!(
        calls[1]
            .2
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        ["alpha", "beta"]
    );
}
