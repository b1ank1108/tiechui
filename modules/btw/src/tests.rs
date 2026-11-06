use super::*;
use tokio::sync::mpsc;

struct Host {
    context: Mutex<ModelContext>,
    calls: Mutex<Vec<ModelRequest>>,
    streams: Mutex<Vec<mpsc::UnboundedSender<Result<StreamEvent, ToolError>>>>,
    events: Mutex<Vec<BtwEvent>>,
    shared: Mutex<Vec<SessionEffect>>,
}
fn model(id: &str) -> ModelInfo {
    ModelInfo {
        provider: "fake".into(),
        id: id.into(),
        context_window: 10000,
        reasoning: true,
        thinking_levels: vec![ThinkingLevel::Off, ThinkingLevel::High],
    }
}
impl Host {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            context: Mutex::new(ModelContext {
                model: model("a"),
                thinking: ThinkingLevel::High,
                system: "FROZEN<&>".into(),
                messages: vec![Message {
                    role: Role::User,
                    content: vec![text("MAIN".into())],
                    opaque: None,
                }],
                models: vec![model("a"), model("b")],
            }),
            calls: Mutex::new(vec![]),
            streams: Mutex::new(vec![]),
            events: Mutex::new(vec![]),
            shared: Mutex::new(vec![]),
        })
    }
    fn session(self: &Arc<Self>) -> CommandSession {
        CommandSession {
            branch: BranchView {
                session_id: "main".into(),
                leaf: "leaf".into(),
                cwd: "/fixture".into(),
                parent_session: None,
                entries: vec![],
            },
            host: self.clone(),
        }
    }
    fn event(&self, event: StreamEvent) {
        self.streams
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .send(Ok(event))
            .unwrap();
    }
}
#[async_trait]
impl CommandHost for Host {
    fn model_context(&self) -> Result<ModelContext, ToolError> {
        Ok(self.context.lock().unwrap().clone())
    }
    async fn model_stream(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<ModelStream, ToolError> {
        self.calls.lock().unwrap().push(request);
        let (tx, rx) = mpsc::unbounded_channel();
        self.streams.lock().unwrap().push(tx);
        Ok(futures::stream::unfold(rx, |mut rx| async {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }
    async fn ask(
        &self,
        _: ApprovalRequest,
        _: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError> {
        panic!("No main interaction")
    }
    async fn update(&self, _: ApprovalRequest) -> Result<(), ToolError> {
        panic!("No main interaction")
    }
    async fn apply(&self, effect: SessionEffect) -> Result<(), ToolError> {
        if let SessionEffect::Emit { data, .. } = effect {
            self.events
                .lock()
                .unwrap()
                .push(serde_json::from_value(data).unwrap());
        } else {
            self.shared.lock().unwrap().push(effect);
        }
        Ok(())
    }
}
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}
async fn command(btw: &Btw, host: &Arc<Host>, args: &str) -> Result<String, ToolError> {
    btw.session_run(args, host.session(), CancellationToken::new())
        .await
}
async fn action(btw: &Btw, host: &Arc<Host>, action: BtwAction) {
    btw.input(
        serde_json::to_value(action).unwrap(),
        host.session(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    settle().await;
}
fn finish(host: &Host, text: &str) {
    host.event(StreamEvent::ContentComplete {
        index: 0,
        content: Content::Thinking {
            text: "PRIVATE_THINKING".into(),
            redacted: false,
            opaque: Some(OpaqueData("SECRET_SIGNATURE".into())),
        },
    });
    host.event(StreamEvent::ContentComplete {
        index: 1,
        content: Content::Text {
            text: text.into(),
            opaque: Some(OpaqueData("TEXT_SIGNATURE".into())),
        },
    });
    host.event(StreamEvent::Finish {
        reason: StopReason::EndTurn,
        usage: Usage {
            input: 12,
            output: 4,
            ..Usage::default()
        },
    });
}
#[test]
fn snapshot_keeps_newest_utf16_text_and_placeholders_without_metadata() {
    let messages = vec![
        Message {
            role: Role::System,
            content: vec![text("EXCLUDE_SYSTEM".into())],
            opaque: None,
        },
        Message {
            role: Role::User,
            content: vec![
                text("🦀".repeat(40000)),
                Content::Image {
                    mime_type: "image/png".into(),
                    data: "SECRET_IMAGE".into(),
                },
            ],
            opaque: None,
        },
        Message {
            role: Role::Tool,
            content: vec![Content::ToolResult(ToolResult {
                call_id: "SECRET_ID".into(),
                content: vec![text("LATEST".into())],
                is_error: false,
            })],
            opaque: None,
        },
    ];
    let result = snapshot(&messages);
    assert!(result.starts_with("[Older main conversation omitted from this snapshot.]"));
    assert!(result.encode_utf16().count() < 64100);
    assert!(result.contains("[image: image/png; data omitted]"));
    assert!(result.ends_with("LATEST"));
    assert!(!result.contains("SECRET") && !result.contains("EXCLUDE_SYSTEM"));
}
#[tokio::test]
async fn frozen_context_switch_busy_model_rebuild_and_explicit_share() {
    let btw = Btw::default();
    let host = Host::new();
    command(&btw, &host, "").await.unwrap();
    host.context.lock().unwrap().system = "LATER".into();
    command(&btw, &host, "question").await.unwrap();
    settle().await;
    command(&btw, &host, "").await.unwrap();
    command(&btw, &host, "second while busy").await.unwrap();
    assert!(
        command(&btw, &host, "model fake/b")
            .await
            .unwrap_err()
            .0
            .contains("Stop the side reply")
    );
    assert_eq!(host.calls.lock().unwrap().len(), 1);
    assert_eq!(
        btw.state()
            .unwrap()
            .as_ref()
            .unwrap()
            .view
            .notice
            .as_deref(),
        Some(WAIT)
    );
    finish(&host, "ANSWER");
    settle().await;
    assert!(host.shared.lock().unwrap().is_empty());
    action(&btw, &host, BtwAction::Share).await;
    assert_eq!(host.calls.lock().unwrap().len(), 1);
    assert!(
        matches!(&host.shared.lock().unwrap()[0], SessionEffect::Message { text, .. } if text.ends_with("ANSWER") && !text.contains("THINKING"))
    );
    host.context.lock().unwrap().models.push(ModelInfo {
        thinking_levels: vec![ThinkingLevel::Off],
        reasoning: false,
        ..model("plain")
    });
    command(&btw, &host, "model fake/plain").await.unwrap();
    assert_eq!(
        btw.state().unwrap().as_ref().unwrap().view.thinking,
        ThinkingLevel::Off
    );
    command(&btw, &host, "model fake/b").await.unwrap();
    command(&btw, &host, "followup").await.unwrap();
    settle().await;
    {
        let calls = host.calls.lock().unwrap();
        assert_eq!(calls[1].model.0, "fake/b");
        assert_eq!(calls[1].reasoning, Some(ReasoningLevel::High));
        assert!(calls.iter().all(|call| call.tools.is_empty()));
        assert_eq!(calls[0].messages[0], calls[1].messages[0]);
        let serialized = serde_json::to_string(&calls[1]).unwrap();
        assert!(serialized.contains("FROZEN&lt;&amp;&gt;"));
        assert!(
            !serialized.contains("LATER")
                && !serialized.contains("SIGNATURE")
                && !serialized.contains("PRIVATE_THINKING")
        );
        assert_eq!(calls[1].messages.len(), 4);
    }
    action(&btw, &host, BtwAction::Close).await;
}
#[tokio::test]
async fn stopping_preserves_partial_thinking_and_discard_invalidates_late_results() {
    let btw = Btw::default();
    let host = Host::new();
    command(&btw, &host, "question").await.unwrap();
    settle().await;
    host.event(StreamEvent::ThinkingDelta {
        index: 0,
        delta: "THINK".into(),
    });
    host.event(StreamEvent::TextDelta {
        index: 1,
        delta: "PARTIAL".into(),
    });
    settle().await;
    action(&btw, &host, BtwAction::Stop).await;
    let view = btw.state().unwrap().as_ref().unwrap().view.clone();
    assert!(!view.busy);
    assert!(view.messages[1].interrupted);
    assert_eq!(view.messages[1].text, "PARTIAL");
    assert_eq!(view.messages[1].thinking, "THINK");
    assert!(view.messages[1].error.is_none());
    action(&btw, &host, BtwAction::Share).await;
    assert!(host.shared.lock().unwrap().is_empty());
    for event in [
        SessionEvent::Started {
            thread_id: ThreadId("main".into()),
            epoch: SessionEpoch(1),
        },
        SessionEvent::Ended {
            thread_id: ThreadId("main".into()),
            epoch: SessionEpoch(1),
        },
    ] {
        command(&btw, &host, "question").await.unwrap();
        settle().await;
        SessionObserver::observe(&btw, &event);
        settle().await;
        assert!(btw.state().unwrap().is_none());
    }
    command(&btw, &host, "").await.unwrap();
    assert!(
        btw.state()
            .unwrap()
            .as_ref()
            .unwrap()
            .view
            .messages
            .is_empty()
    );
}
#[tokio::test]
async fn thinking_only_and_provider_errors_never_synthesize_answer_text() {
    let btw = Btw::default();
    let host = Host::new();
    command(&btw, &host, "thinking only").await.unwrap();
    settle().await;
    finish(&host, "");
    settle().await;
    let row = btw.state().unwrap().as_ref().unwrap().view.messages[1].clone();
    assert!(row.text.is_empty() && row.complete);
    assert_eq!(row.thinking, "PRIVATE_THINKING");
    command(&btw, &host, "error").await.unwrap();
    settle().await;
    host.streams
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .send(Err(ToolError("ORIGINAL_FAILURE".into())))
        .unwrap();
    settle().await;
    let row = btw.state().unwrap().as_ref().unwrap().view.messages[3].clone();
    assert!(row.text.is_empty());
    assert_eq!(row.error.as_deref(), Some("ORIGINAL_FAILURE"));
}
