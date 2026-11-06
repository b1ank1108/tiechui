use super::*;
use tiechui_plugin_api::{Module, ModuleConfig};

struct ChangingPrompt(AtomicUsize);
#[async_trait]
impl tiechui_plugin_api::LifecycleInterceptor for ChangingPrompt {
    async fn on_event(
        &self,
        event: &LifecycleEvent,
        _: &LifecycleContext,
        _: CancellationToken,
    ) -> Result<LifecycleOutcome, ToolError> {
        Ok(if matches!(event, LifecycleEvent::BeforeRequest) {
            LifecycleOutcome {
                context: vec![format!(
                    "ephemeral-prefix-{}",
                    self.0.fetch_add(1, Ordering::SeqCst)
                )],
                ..Default::default()
            }
        } else {
            LifecycleOutcome::default()
        })
    }
}

#[tokio::test]
async fn request_time_prompt_survives_turn_cleanup_and_resets_cache_attribution() {
    let second = Script::events(vec![StreamEvent::Finish {
        reason: StopReason::EndTurn,
        usage: Usage {
            input: 200,
            ..Default::default()
        },
    }]);
    let mut harness = Harness::with_contributions(
        vec![answer("one"), second],
        Arc::new(BoundedRetry::default()),
        serde_json::json!({"type":"object"}),
        |registry| {
            tiechui_module_context::ContextModule
                .register(registry, &ModuleConfig::default())
                .unwrap();
            registry.register_lifecycle_interceptor(Arc::new(ChangingPrompt(AtomicUsize::new(0))));
        },
    )
    .await;
    for prompt in ["first", "second"] {
        harness.input(prompt, InputDelivery::FollowUp).await;
        harness.request().await;
        harness.end(TurnStatus::Completed).await;
    }
    let snapshot = open(&mut harness, "system").await;
    assert!(
        snapshot
            .system
            .iter()
            .any(|item| item.category == "Appended prompt" && item.text == "ephemeral-prefix-1")
    );
    assert_eq!(snapshot.cache.responses[1].fresh, 0);
    assert_eq!(snapshot.cache.responses[1].unattributed, 200);
    assert!(
        !std::fs::read_to_string(&harness.path)
            .unwrap()
            .contains("ephemeral-prefix")
    );
    harness
        .send(Op::SetModel {
            provider: "fake".into(),
            model: "plain".into(),
        })
        .await;
    harness
        .until(|event| matches!(event, EventKind::SessionUpdated { .. }))
        .await;
    assert!(
        open(&mut harness, "")
            .await
            .notices
            .iter()
            .any(|line| line.contains("No request captured"))
    );
    harness.stop().await;
}

pub(super) async fn open(harness: &mut Harness, arguments: &str) -> ContextSnapshot {
    let before = std::fs::read(&harness.path).unwrap();
    harness
        .send(Op::RunCommand {
            name: "context".into(),
            arguments: arguments.into(),
        })
        .await;
    let event = harness
        .until(|event| matches!(event, EventKind::CommandPanel { .. }))
        .await;
    assert_eq!(
        before,
        std::fs::read(&harness.path).unwrap(),
        "inspection wrote the session"
    );
    assert!(
        harness.requests.try_recv().is_err(),
        "inspection called the model"
    );
    let EventKind::CommandPanel { panel, .. } = event.event else {
        unreachable!()
    };
    *panel
}

#[tokio::test]
async fn ordinary_turn_has_no_request_or_projection_only_messages() {
    let mut harness = Harness::with_contributions(
        vec![answer("one")],
        Arc::new(BoundedRetry::default()),
        serde_json::json!({"type":"object"}),
        |registry| {
            tiechui_module_context::ContextModule
                .register(registry, &ModuleConfig::default())
                .unwrap();
        },
    )
    .await;
    harness.input("first", InputDelivery::FollowUp).await;
    harness.request().await;
    harness.end(TurnStatus::Completed).await;
    let snapshot = open(&mut harness, "injections").await;
    assert!(!snapshot.system.is_empty());
    assert!(
        snapshot.injections.iter().all(|item| !matches!(
            item.category.as_str(),
            "Request-only" | "Removed / replaced"
        )),
        "unexpected differences: {:?}",
        snapshot.injections
    );
    harness.stop().await;
}

struct MismatchedPrompt;
#[async_trait]
impl tiechui_plugin_api::ContextProvider for MismatchedPrompt {
    async fn fragments(
        &self,
        _: &TurnInput,
        _: CancellationToken,
    ) -> Result<Vec<String>, ToolError> {
        Ok(vec!["sent system text".into()])
    }

    fn describe(&self, _: &[String]) -> Vec<tiechui_plugin_api::PromptFragment> {
        vec![tiechui_plugin_api::PromptFragment {
            label: "System prompt".into(),
            category: "System prompt".into(),
            source: "Test contribution".into(),
            text: "described system text".into(),
            references: vec![],
        }]
    }
}

#[tokio::test]
async fn sent_system_differences_are_compared_with_the_system_fragments() {
    let mut harness = Harness::with_contributions(
        vec![answer("one")],
        Arc::new(BoundedRetry::default()),
        serde_json::json!({"type":"object"}),
        |registry| {
            tiechui_module_context::ContextModule
                .register(registry, &ModuleConfig::default())
                .unwrap();
            registry.register_context_provider(Arc::new(MismatchedPrompt));
        },
    )
    .await;
    harness.input("first", InputDelivery::FollowUp).await;
    harness.request().await;
    harness.end(TurnStatus::Completed).await;
    let snapshot = open(&mut harness, "injections").await;
    let added: Vec<_> = snapshot
        .injections
        .iter()
        .filter(|item| item.category == "Request-only")
        .collect();
    let removed: Vec<_> = snapshot
        .injections
        .iter()
        .filter(|item| item.category == "Removed / replaced")
        .collect();
    assert_eq!((added.len(), removed.len()), (1, 1));
    assert!(added[0].text.contains("sent system text"));
    assert!(removed[0].text.contains("described system text"));
    harness.stop().await;
}

#[tokio::test]
async fn inspection_is_read_only_frozen_and_keeps_request_bytes_identical() {
    let mut requests = vec![];
    for inspect in [false, true] {
        let mut harness = Harness::with_contributions(
            vec![answer("one"), answer("two")],
            Arc::new(BoundedRetry::default()),
            serde_json::json!({"type":"object"}),
            |registry| {
                tiechui_module_context::ContextModule
                    .register(registry, &ModuleConfig::default())
                    .unwrap();
            },
        )
        .await;
        if inspect {
            let first = open(&mut harness, "").await;
            assert!(
                first
                    .notices
                    .iter()
                    .any(|line| line.contains("No request captured"))
            );
            assert!(!first.system.is_empty());
            assert!(first.provider_usage.is_none());
        }
        harness.input("first", InputDelivery::FollowUp).await;
        let first = harness.request().await;
        harness.end(TurnStatus::Completed).await;
        if inspect {
            let frozen = open(&mut harness, "messages").await;
            assert_eq!(frozen.input_tokens, 10, "shared successful usage anchor");
            assert_eq!(frozen.provider_usage.unwrap().input, 3);
            assert_eq!(frozen.messages.len(), 2);
            for view in ["system", "tools", "injections", "cache"] {
                assert_eq!(open(&mut harness, view).await, frozen);
            }
            assert_eq!(frozen.cache.responses.len(), 1);
        }
        harness.input("second", InputDelivery::FollowUp).await;
        let second = harness.request().await;
        harness.end(TurnStatus::Completed).await;
        if inspect {
            let snapshot = open(&mut harness, "").await;
            assert_eq!(snapshot.messages.len(), 4);
            assert_eq!(snapshot.cache.responses.len(), 2);
            harness.send(Op::Reload).await;
            let event = harness
                .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
                .await;
            harness.epoch = event.epoch;
            let reloaded = open(&mut harness, "").await;
            assert!(
                reloaded
                    .notices
                    .iter()
                    .any(|line| line.contains("No request captured"))
            );
            assert_eq!(reloaded.cache, snapshot.cache);
            let store = harness.snapshot().await;
            let first_reply = store
                .entries()
                .iter()
                .find(|entry| {
                    matches!(&entry.kind,
                EntryKind::Message { message, .. } if message.role == Role::Assistant)
                })
                .unwrap()
                .id
                .clone();
            harness
                .send(Op::Branch {
                    entry_id: first_reply,
                })
                .await;
            let event = harness
                .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
                .await;
            harness.epoch = event.epoch;
            let branch = open(&mut harness, "cache").await;
            assert_eq!(branch.cache.responses.len(), 1);
            assert_eq!(branch.messages.len(), 2);
        }
        requests.push((
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap(),
        ));
        harness.stop().await;
    }
    assert_eq!(requests[0], requests[1]);
}

#[test]
fn request_diffs_are_bounded_preserve_duplicates_and_hide_replay_payloads() {
    let plain = Message {
        role: Role::User,
        content: vec![text_content("same".into())],
        opaque: None,
    };
    let mut private = Message {
        role: Role::Assistant,
        content: vec![
            Content::Image {
                mime_type: "image/png".into(),
                data: "PRIVATE_IMAGE".repeat(100_000),
            },
            Content::Thinking {
                text: "reason".into(),
                redacted: false,
                opaque: Some(tiechui_model::OpaqueData("PRIVATE_SIGNATURE".into())),
            },
            text_content("\x1b[31mvisible\x1b[0m\x1b]52;c;PRIVATE_OSC\x07".into()),
            text_content("界".repeat(100_000)),
        ],
        opaque: Some(tiechui_model::OpaqueData("PRIVATE_TRANSPORT".into())),
    };
    let observed = inspection::observe(
        &[plain.clone(), plain.clone()],
        &[plain.clone(), private.clone()],
    );
    assert_eq!(observed.removed.len(), 1);
    assert_eq!(observed.added.len(), 1);
    let text = &observed.added[0].item.text;
    assert!(text.contains("[image: image/png; data omitted]"));
    assert!(text.contains("thinking signature omitted"));
    assert!(text.contains("visible"));
    assert!(!text.contains("PRIVATE_"));
    assert!(!text.contains('\x1b'));
    assert!(text.len() < 66000);
    assert!(text.contains("Diagnostic preview clipped"));
    let original = private.clone();
    private.opaque = Some(tiechui_model::OpaqueData("CHANGED".into()));
    let diff = inspection::observe(&[original], &[private]);
    assert_eq!((diff.added.len(), diff.removed.len()), (1, 1));
    let many = inspection::observe(
        &[],
        &vec![
            Message {
                role: Role::User,
                content: vec![text_content("x".repeat(100_000))],
                opaque: None
            };
            30
        ],
    );
    assert!(
        many.added
            .iter()
            .map(|message| message.item.text.len())
            .sum::<usize>()
            < 1024 * 1024 + 30 * 100
    );
}
