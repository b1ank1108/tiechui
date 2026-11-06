use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::json;
use tempfile::TempDir;
use tiechui_model::{ModelCapabilities, ModelId, OpaqueData, ToolSpec};
use tiechui_plugin_api::StaticContext;
use tiechui_tools::{Concurrency, Tool, ToolError, ToolRegistry};
use tokio::sync::oneshot;

use super::*;

enum Step {
    Panic,
    Event(StreamEvent),
    Error(ProviderError),
    Wait(oneshot::Receiver<()>),
}

struct Script {
    error: Option<ProviderError>,
    steps: VecDeque<Step>,
}

impl Script {
    fn events(events: Vec<StreamEvent>) -> Self {
        Self {
            error: None,
            steps: events.into_iter().map(Step::Event).collect(),
        }
    }

    fn error(kind: ErrorKind) -> Self {
        Self {
            error: Some(provider_error(kind, "scripted failure")),
            steps: VecDeque::new(),
        }
    }
}

struct FakeProvider {
    scripts: Mutex<VecDeque<Script>>,
    requests: mpsc::UnboundedSender<ModelRequest>,
}

#[async_trait]
impl ModelProvider for FakeProvider {
    fn cost_nano_usd(&self, model: &ModelId, usage: Usage) -> Option<u64> {
        (model.0 == "fake/a").then_some((usage.input + usage.output) * 1000)
    }

    fn capabilities(&self, _: &ModelId) -> ModelCapabilities {
        ModelCapabilities {
            context_window: 10000,
            max_output: 1000,
            reasoning: true,
            images: true,
        }
    }

    fn thinking_levels(&self, model: &ModelId) -> Vec<Option<ReasoningLevel>> {
        if model.0 == "plain" {
            vec![None]
        } else {
            std::iter::once(None)
                .chain(ReasoningLevel::ALL.map(Some))
                .collect()
        }
    }

    async fn stream(
        &self,
        request: ModelRequest,
        _: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        self.requests.send(request).unwrap();
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model request");
        if let Some(error) = script.error {
            return Err(error);
        }
        Ok(Box::pin(futures::stream::unfold(
            script.steps,
            |mut steps| async {
                loop {
                    match steps.pop_front()? {
                        Step::Panic => panic!("isolated provider fixture"),
                        Step::Wait(wait) => {
                            let _ = wait.await;
                        }
                        Step::Event(event) => return Some((Ok(event), steps)),
                        Step::Error(error) => return Some((Err(error), steps)),
                    }
                }
            },
        )))
    }
}

struct TestTool {
    spec: ToolSpec,
    gates: Mutex<HashMap<String, oneshot::Receiver<()>>>,
    started: mpsc::UnboundedSender<String>,
    finished: mpsc::UnboundedSender<String>,
    runs: AtomicUsize,
    arguments: Mutex<Vec<serde_json::Value>>,
    late: Mutex<Option<oneshot::Receiver<()>>>,
    late_sent: mpsc::UnboundedSender<()>,
}

#[async_trait]
impl Tool for TestTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &serde_json::Value) -> Concurrency {
        Concurrency::Shared
    }

    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        assert!(!ctx.scope.thread_id.0.is_empty());
        assert!(!ctx.scope.turn_id.0.is_empty());
        assert!(ctx.cwd.is_absolute());
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.arguments.lock().unwrap().push(call.arguments.clone());
        self.started.send(call.id.clone()).unwrap();
        let late = self.late.lock().unwrap().take();
        if let Some(wait) = late {
            let updates = ctx.updates.clone();
            let call_id = call.id.clone();
            let epoch = ctx.epoch;
            let sent = self.late_sent.clone();
            tokio::spawn(async move {
                let _ = wait.await;
                updates(ToolUpdate {
                    call_id,
                    epoch,
                    output: ToolOutput::text("stale update"),
                });
                let _ = sent.send(());
            });
        }
        let gate = self.gates.lock().unwrap().remove(&call.id);
        if let Some(gate) = gate {
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled() => return Err(ToolError("Operation aborted".into())),
                _ = gate => {}
            }
        }
        (ctx.updates)(ToolUpdate {
            call_id: call.id.clone(),
            epoch: ctx.epoch,
            output: ToolOutput::text("current update"),
        });
        self.finished.send(call.id.clone()).unwrap();
        if call.id == "fail" {
            Err(ToolError("tool failure".into()))
        } else {
            Ok(ToolOutput::text(call.id))
        }
    }
}

struct Harness {
    _root: TempDir,
    handle: SessionHandle,
    requests: mpsc::UnboundedReceiver<ModelRequest>,
    tool: Arc<TestTool>,
    started: mpsc::UnboundedReceiver<String>,
    finished: mpsc::UnboundedReceiver<String>,
    late_sent: mpsc::UnboundedReceiver<()>,
    thread: ThreadId,
    epoch: SessionEpoch,
    path: PathBuf,
    seen: Vec<Event>,
    shutdown: CancellationToken,
    next: u64,
    interactions: Interactions,
}

impl Harness {
    async fn new(scripts: Vec<Script>) -> Self {
        Self::with_retry(
            scripts,
            Arc::new(BoundedRetry {
                max_retries: 2,
                initial_delay: Duration::ZERO,
                max_delay: Duration::ZERO,
            }),
        )
        .await
    }

    async fn with_retry(scripts: Vec<Script>, retry: Arc<dyn RetryPolicy>) -> Self {
        Self::with_schema(
            scripts,
            retry,
            json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}),
        )
        .await
    }

    async fn with_schema(
        scripts: Vec<Script>,
        retry: Arc<dyn RetryPolicy>,
        schema: serde_json::Value,
    ) -> Self {
        Self::with_contributions(scripts, retry, schema, |_| {}).await
    }

    async fn with_contributions(
        scripts: Vec<Script>,
        retry: Arc<dyn RetryPolicy>,
        schema: serde_json::Value,
        configure: impl FnOnce(&mut Registry),
    ) -> Self {
        Self::with_compaction(
            scripts,
            retry,
            schema,
            configure,
            tiechui_plugin_api::CompactionConfig {
                enabled: false,
                ..Default::default()
            },
        )
        .await
    }

    async fn with_compaction(
        scripts: Vec<Script>,
        retry: Arc<dyn RetryPolicy>,
        schema: serde_json::Value,
        configure: impl FnOnce(&mut Registry),
        compaction: tiechui_plugin_api::CompactionConfig,
    ) -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let home = root.path().join("home");
        let cwd = root.path().to_path_buf();
        let store = tokio::task::spawn_blocking(move || {
            SessionStore::create(&home, &cwd, &CancellationToken::new()).unwrap()
        })
        .await
        .unwrap();
        let path = store.path().to_owned();
        let thread = ThreadId(store.header().id.clone());
        let (requests_tx, requests) = mpsc::unbounded_channel();
        let provider = Arc::new(FakeProvider {
            scripts: Mutex::new(scripts.into()),
            requests: requests_tx,
        });
        let (started_tx, started) = mpsc::unbounded_channel();
        let (finished_tx, finished) = mpsc::unbounded_channel();
        let (late_tx, late_sent) = mpsc::unbounded_channel();
        let tool = Arc::new(TestTool {
            spec: ToolSpec {
                name: "probe".into(),
                description: "Offline test tool".into(),
                input_schema: schema,
            },
            gates: Mutex::new(HashMap::new()),
            started: started_tx,
            finished: finished_tx,
            runs: AtomicUsize::new(0),
            arguments: Mutex::new(vec![]),
            late: Mutex::new(None),
            late_sent: late_tx,
        });
        let mut registry = Registry::default();
        registry.register_tool(tool.clone()).unwrap();
        registry.register_context_provider(Arc::new(StaticContext(vec![
            "base\n".into(),
            "tool guidance".into(),
        ])));
        configure(&mut registry);
        let (interactions, inbox) = Interactions::new();
        let config = SessionConfig {
            compaction,
            model: origin("a"),
            thinking: None,
            providers: BTreeMap::from([("fake".into(), provider as Arc<dyn ModelProvider>)]),
            registry,
            retry,
            interactions: Some(inbox),
            epoch: SessionEpoch(0),
        };
        let shutdown = CancellationToken::new();
        let handle = SessionActor::spawn(store, config, shutdown.clone());
        let mut harness = Self {
            _root: root,
            handle,
            requests,
            tool,
            started,
            finished,
            late_sent,
            thread,
            epoch: SessionEpoch(0),
            path,
            seen: vec![],
            shutdown,
            next: 0,
            interactions,
        };
        harness
            .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
            .await;
        harness
    }

    async fn send(&mut self, op: Op) {
        self.next += 1;
        self.handle
            .submissions
            .send(Submission {
                id: SubmissionId(self.next.to_string()),
                thread_id: self.thread.clone(),
                epoch: self.epoch,
                op,
            })
            .await
            .unwrap();
    }

    async fn input(&mut self, text: &str, delivery: InputDelivery) {
        self.send(Op::UserInput {
            content: vec![InputContent::Text { text: text.into() }],
            delivery,
        })
        .await;
    }

    async fn until(&mut self, predicate: impl Fn(&EventKind) -> bool) -> Event {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), self.handle.events.recv())
                .await
                .expect("actor event timeout")
                .unwrap();
            self.seen.push(event.clone());
            if predicate(&event.event) {
                return event;
            }
        }
    }

    async fn started_turn(&mut self) -> TurnId {
        let event = self
            .until(|event| matches!(event, EventKind::TurnStarted { .. }))
            .await;
        let EventKind::TurnStarted { turn } = event.event else {
            unreachable!()
        };
        turn.id
    }

    async fn end(&mut self, status: TurnStatus) {
        let event = self
            .until(|event| matches!(event, EventKind::TurnCompleted { .. }))
            .await;
        let EventKind::TurnCompleted { turn } = event.event else {
            unreachable!()
        };
        assert_eq!(turn.status, status);
    }

    async fn request(&mut self) -> ModelRequest {
        recv(&mut self.requests).await
    }

    fn gate_tool(&self, id: &str) -> oneshot::Sender<()> {
        let (send, receive) = oneshot::channel();
        self.tool.gates.lock().unwrap().insert(id.into(), receive);
        send
    }

    async fn snapshot(&self) -> SessionStore {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            SessionStore::open_read_only(&path, &CancellationToken::new()).unwrap()
        })
        .await
        .unwrap()
    }

    async fn valid_transcript(&self) {
        let store = self.snapshot().await;
        let mut pending = HashSet::new();
        for entry in store.entries() {
            if let EntryKind::Message { message, model, .. } = &entry.kind {
                if message.role == Role::Assistant {
                    assert!(model.is_some());
                }
                for content in &message.content {
                    match content {
                        Content::ToolCall(call) => assert!(
                            pending.insert(call.id.clone()),
                            "duplicate call {}",
                            call.id
                        ),
                        Content::ToolResult(result) => assert!(
                            pending.remove(&result.call_id),
                            "unpaired result {}",
                            result.call_id
                        ),
                        _ => {}
                    }
                }
            }
        }
        assert!(pending.is_empty(), "dangling calls: {pending:?}");
    }

    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.handle.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

async fn recv<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}
fn origin(model: &str) -> ModelRef {
    ModelRef {
        provider: "fake".into(),
        model: ModelId(model.into()),
    }
}
fn call(id: &str) -> Content {
    Content::ToolCall(ToolCall {
        id: id.into(),
        name: "probe".into(),
        arguments: json!({"value":id}),
        opaque: None,
    })
}
fn finish(reason: StopReason) -> StreamEvent {
    StreamEvent::Finish {
        reason,
        usage: Usage {
            input: 3,
            output: 2,
            cache_read: 1,
            cache_write: 4,
            ..Default::default()
        },
    }
}
fn reply(content: Vec<Content>) -> Script {
    let reason = if content
        .iter()
        .any(|content| matches!(content, Content::ToolCall(_)))
    {
        StopReason::ToolUse
    } else {
        StopReason::EndTurn
    };
    let mut events: Vec<_> = content
        .into_iter()
        .enumerate()
        .map(|(index, content)| StreamEvent::ContentComplete {
            index: index as u32,
            content,
        })
        .collect();
    events.push(finish(reason));
    Script::events(events)
}
fn answer(text: &str) -> Script {
    reply(vec![text_content(text.into())])
}
fn users(request: &ModelRequest) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .map(|message| match &message.content[0] {
            Content::Text { text, .. } => text.clone(),
            _ => "image".into(),
        })
        .collect()
}
fn results(request: &ModelRequest) -> Vec<ToolResult> {
    request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| {
            if let Content::ToolResult(result) = content {
                Some(result.clone())
            } else {
                None
            }
        })
        .collect()
}

#[tokio::test]
async fn multi_step_loop_persists_origin_and_publishes_cumulative_usage() {
    let mut h = Harness::new(vec![
        reply(vec![call("a")]),
        reply(vec![call("b")]),
        answer("done"),
    ])
    .await;
    h.input("task", InputDelivery::Steer).await;
    let first = h.request().await;
    let second = h.request().await;
    let third = h.request().await;
    h.end(TurnStatus::Completed).await;
    assert_eq!(results(&second)[0].call_id, "a");
    assert_eq!(
        results(&third)
            .iter()
            .map(|result| &result.call_id)
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert_eq!(h.tool.runs.load(Ordering::SeqCst), 2);
    assert_eq!(first.messages[0], second.messages[0]);
    assert_eq!(second.messages[0], third.messages[0]);
    let usage: Vec<_> = h
        .seen
        .iter()
        .filter_map(|event| {
            if let EventKind::TokenUsageUpdated { total, .. } = event.event {
                Some(total)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        usage.last().unwrap(),
        &TokenUsage {
            input: 9,
            output: 6,
            cache_read: 3,
            cache_write: 12,
            ..Default::default()
        }
    );
    assert_event_items(&h.seen);
    h.valid_transcript().await;
    h.stop().await;
}

#[tokio::test]
async fn parallel_calls_complete_out_of_order_but_results_keep_model_order() {
    let mut h = Harness::new(vec![reply(vec![call("a"), call("b")]), answer("done")]).await;
    let a = h.gate_tool("a");
    let b = h.gate_tool("b");
    h.input("parallel", InputDelivery::Steer).await;
    let mut started = vec![recv(&mut h.started).await, recv(&mut h.started).await];
    started.sort();
    assert_eq!(started, ["a", "b"]);
    b.send(()).unwrap();
    assert_eq!(recv(&mut h.finished).await, "b");
    a.send(()).unwrap();
    assert_eq!(recv(&mut h.finished).await, "a");
    h.request().await;
    let request = h.request().await;
    h.end(TurnStatus::Completed).await;
    assert_eq!(
        results(&request)
            .iter()
            .map(|result| result.call_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    h.valid_transcript().await;
    h.stop().await;
}

#[tokio::test]
async fn interrupt_stream_persists_partial_and_pairs_unfinished_calls() {
    let (_release, gate) = oneshot::channel();
    let script = Script {
        error: None,
        steps: vec![
            Step::Event(StreamEvent::TextDelta {
                index: 0,
                delta: "partial".into(),
            }),
            Step::Event(StreamEvent::ToolCallStart {
                index: 1,
                id: "unfinished".into(),
                name: "probe".into(),
            }),
            Step::Event(StreamEvent::ToolCallArgumentsDelta {
                index: 1,
                delta: "{\"value\":".into(),
            }),
            Step::Wait(gate),
        ]
        .into(),
    };
    let mut h = Harness::new(vec![script, answer("next")]).await;
    h.send(Op::SetThinking {
        level: ThinkingLevel::Low,
    })
    .await;
    h.input("task", InputDelivery::Steer).await;
    let turn = h.started_turn().await;
    h.until(|event| matches!(event, EventKind::ItemDelta { delta, .. } if delta == "{\"value\":"))
        .await;
    h.send(Op::Interrupt { turn_id: turn }).await;
    h.end(TurnStatus::Interrupted).await;
    h.valid_transcript().await;
    let store = h.snapshot().await;
    assert!(store.entries().iter().any(|entry| matches!(
        entry.kind,
        EntryKind::Message {
            stop_reason: Some(StopReason::Cancelled),
            reasoning: Some(ReasoningLevel::Low),
            ..
        }
    )));
    assert!(
        store
            .build_context()
            .iter()
            .flat_map(|message| &message.content)
            .any(|content| matches!(content, Content::Text { text, .. } if text == "partial"))
    );
    assert!(store.build_context().iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::ToolResult(result) if result.content == vec![text_content("Operation aborted".into())])));
    h.input("next", InputDelivery::Steer).await;
    h.request().await;
    let request = h.request().await;
    h.end(TurnStatus::Completed).await;
    assert_eq!(users(&request), ["task", "next"]);
    assert!(
        request
            .messages
            .iter()
            .all(|message| message.role != Role::Assistant && message.role != Role::Tool)
    );
    assert_eq!(h.tool.runs.load(Ordering::SeqCst), 0);
    h.stop().await;
}

#[tokio::test]
async fn interrupt_tools_preserves_completed_results_and_closes_cancelled_calls() {
    let mut h = Harness::new(vec![
        reply(vec![call("fast"), call("slow")]),
        answer("next"),
    ])
    .await;
    let _slow = h.gate_tool("slow");
    h.input("task", InputDelivery::Steer).await;
    let turn = h.started_turn().await;
    assert_eq!(recv(&mut h.finished).await, "fast");
    recv(&mut h.started).await;
    recv(&mut h.started).await;
    h.send(Op::Interrupt { turn_id: turn }).await;
    h.end(TurnStatus::Interrupted).await;
    h.valid_transcript().await;
    h.input("next", InputDelivery::Steer).await;
    h.request().await;
    let request = h.request().await;
    h.end(TurnStatus::Completed).await;
    let results = results(&request);
    assert_eq!(results[0].content, [text_content("fast".into())]);
    assert!(!results[0].is_error);
    assert_eq!(
        results[1].content,
        [text_content("Operation aborted".into())]
    );
    assert!(results[1].is_error);
    h.stop().await;
}

#[tokio::test]
async fn steering_reaches_next_boundary_and_follow_ups_wait_fifo_with_images() {
    let mut h = Harness::new(vec![
        reply(vec![call("wait")]),
        answer("steered1"),
        answer("steered2"),
        answer("follow1"),
        answer("follow2"),
    ])
    .await;
    let release = h.gate_tool("wait");
    h.input("task", InputDelivery::Steer).await;
    recv(&mut h.started).await;
    h.input("s1", InputDelivery::Steer).await;
    h.input("s2", InputDelivery::Steer).await;
    h.send(Op::UserInput {
        content: vec![
            InputContent::Text { text: "f1".into() },
            InputContent::Image {
                mime_type: "image/png".into(),
                data: "AA==".into(),
            },
        ],
        delivery: InputDelivery::FollowUp,
    })
    .await;
    h.input("f2", InputDelivery::FollowUp).await;
    release.send(()).unwrap();
    let mut requests = vec![];
    for _ in 0..5 {
        requests.push(h.request().await);
    }
    h.end(TurnStatus::Completed).await;
    h.end(TurnStatus::Completed).await;
    h.end(TurnStatus::Completed).await;
    assert_eq!(users(&requests[1]), ["task", "s1"]);
    assert_eq!(users(&requests[2]), ["task", "s1", "s2"]);
    assert_eq!(users(&requests[3]), ["task", "s1", "s2", "f1"]);
    assert_eq!(users(&requests[4]), ["task", "s1", "s2", "f1", "f2"]);
    let image = requests[3].messages.last().unwrap().content.last().unwrap();
    assert_eq!(
        image,
        &Content::Image {
            mime_type: "image/png".into(),
            data: "AA==".into()
        }
    );
    h.stop().await;
}

#[tokio::test]
async fn queue_updates_are_published_and_retract_removes_only_unconsumed_inputs() {
    let mut h = Harness::new(vec![
        reply(vec![call("wait")]),
        answer("done"),
        answer("follow"),
    ])
    .await;
    let release = h.gate_tool("wait");
    h.input("task", InputDelivery::Steer).await;
    recv(&mut h.started).await;
    h.input("f1", InputDelivery::FollowUp).await;
    h.input("f2", InputDelivery::FollowUp).await;
    let event = h
        .until(|event| matches!(event, EventKind::QueueUpdated { follow_up, .. } if follow_up.len() == 2))
        .await;
    let EventKind::QueueUpdated {
        steering,
        follow_up,
    } = event.event
    else {
        unreachable!()
    };
    assert!(steering.is_empty());
    assert_eq!(follow_up[0].submission_id, SubmissionId("2".into()));
    h.send(Op::Retract {
        submission_ids: vec![SubmissionId("2".into()), SubmissionId("missing".into())],
    })
    .await;
    let event = h
        .until(|event| matches!(event, EventKind::InputRetracted { .. }))
        .await;
    assert_eq!(
        event.event,
        EventKind::InputRetracted {
            inputs: vec![QueuedInput {
                submission_id: SubmissionId("2".into()),
                content: vec![InputContent::Text { text: "f1".into() }],
            }],
        }
    );
    release.send(()).unwrap();
    let mut requests = vec![];
    for _ in 0..3 {
        requests.push(h.request().await);
    }
    h.end(TurnStatus::Completed).await;
    h.end(TurnStatus::Completed).await;
    assert_eq!(users(&requests[2]), ["task", "f2"]);
    h.stop().await;
}

#[tokio::test]
async fn retry_then_success_honors_retry_after() {
    let mut script = Script::error(ErrorKind::RateLimit);
    script.error.as_mut().unwrap().retry_after = Some(Duration::from_millis(30));
    let mut h = Harness::with_retry(
        vec![script, answer("success")],
        Arc::new(BoundedRetry {
            max_retries: 2,
            initial_delay: Duration::ZERO,
            max_delay: Duration::from_millis(100),
        }),
    )
    .await;
    h.input("task", InputDelivery::Steer).await;
    let first = h.request().await;
    let start = Instant::now();
    let second = h.request().await;
    assert!(start.elapsed() >= Duration::from_millis(25));
    assert_eq!(first, second);
    h.end(TurnStatus::Completed).await;
    h.stop().await;
}

#[tokio::test]
async fn manual_retry_keeps_context_and_finished_tools_and_has_one_attempt() {
    let partial = Script {
        error: None,
        steps: vec![
            Step::Event(StreamEvent::ContentComplete {
                index: 0,
                content: call("unfinished"),
            }),
            Step::Error(provider_error(
                ErrorKind::InvalidRequest,
                "partial request failed",
            )),
        ]
        .into(),
    };
    let mut h = Harness::new(vec![
        reply(vec![call("finished")]),
        partial,
        Script::error(ErrorKind::Overloaded),
        answer("recovered"),
    ])
    .await;
    h.input("original task", InputDelivery::Steer).await;
    h.request().await;
    let failed = h.request().await;
    h.end(TurnStatus::Failed).await;
    h.send(Op::Retry).await;
    assert_eq!(h.request().await, failed);
    h.end(TurnStatus::Failed).await;
    assert!(h.requests.try_recv().is_err());
    h.send(Op::Retry).await;
    assert_eq!(h.request().await, failed);
    h.end(TurnStatus::Completed).await;
    assert_eq!(h.tool.runs.load(Ordering::SeqCst), 1);
    h.send(Op::Retry).await;
    h.until(|event| matches!(event, EventKind::Error { message } if message == "No failed request to retry.")).await;
    assert!(h.requests.try_recv().is_err());
    h.valid_transcript().await;
    h.stop().await;
}

#[tokio::test]
async fn new_input_cancels_retry_wait_and_invalidates_the_old_failure() {
    let mut h = Harness::with_retry(
        vec![Script::error(ErrorKind::Overloaded), answer("new response")],
        Arc::new(BoundedRetry {
            max_retries: 3,
            initial_delay: Duration::from_secs(60),
            max_delay: Duration::from_secs(60),
        }),
    )
    .await;
    h.input("old task", InputDelivery::Steer).await;
    h.request().await;
    h.until(|event| {
        matches!(
            event,
            EventKind::RequestRetry {
                delay_ms: Some(_),
                ..
            }
        )
    })
    .await;
    h.input("new task", InputDelivery::FollowUp).await;
    h.end(TurnStatus::Interrupted).await;
    let request = h.request().await;
    assert_eq!(
        request
            .messages
            .iter()
            .filter(|message| message.role == Role::User)
            .count(),
        2
    );
    h.end(TurnStatus::Completed).await;
    h.send(Op::Reload).await;
    let event = h
        .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    h.epoch = event.epoch;
    h.send(Op::Retry).await;
    h.until(|event| matches!(event, EventKind::Error { message } if message == "No failed request to retry.")).await;
    assert!(h.requests.try_recv().is_err());
    h.stop().await;
}

#[tokio::test]
async fn retry_after_reload_preserves_input_context_and_refreshes_request_context() {
    struct Context(Arc<AtomicUsize>);
    #[async_trait]
    impl tiechui_plugin_api::LifecycleInterceptor for Context {
        async fn on_event(
            &self,
            event: &LifecycleEvent,
            _: &tiechui_plugin_api::LifecycleContext,
            _: CancellationToken,
        ) -> Result<tiechui_plugin_api::LifecycleOutcome, ToolError> {
            let context = match event {
                LifecycleEvent::UserInput { .. } => {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    vec!["input context".into()]
                }
                LifecycleEvent::BeforeRequest => vec!["request context".into()],
                _ => vec![],
            };
            Ok(tiechui_plugin_api::LifecycleOutcome {
                context,
                ..Default::default()
            })
        }
    }
    let inputs = Arc::new(AtomicUsize::new(0));
    let mut h = Harness::with_contributions(
        vec![
            Script::error(ErrorKind::InvalidRequest),
            answer("recovered"),
        ],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| {
            registry.register_lifecycle_interceptor(Arc::new(Context(inputs.clone())));
        },
    )
    .await;
    h.input("task", InputDelivery::Steer).await;
    let original = h.request().await;
    h.end(TurnStatus::Failed).await;
    h.send(Op::Reload).await;
    let event = h
        .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    h.epoch = event.epoch;
    h.send(Op::Retry).await;
    assert_eq!(h.request().await, original);
    h.end(TurnStatus::Completed).await;
    assert_eq!(inputs.load(Ordering::SeqCst), 1);
    h.stop().await;
}

#[tokio::test]
async fn retry_now_consumes_scheduled_budget_and_interrupt_wait_can_retry() {
    for interrupt in [false, true] {
        let mut h = Harness::with_retry(
            vec![
                Script::error(ErrorKind::Overloaded),
                Script::error(ErrorKind::Overloaded),
            ],
            Arc::new(BoundedRetry {
                max_retries: 1,
                initial_delay: Duration::from_secs(60),
                max_delay: Duration::from_secs(60),
            }),
        )
        .await;
        h.input("original", InputDelivery::Steer).await;
        let failed = h.request().await;
        let turn_id = h.started_turn().await;
        h.until(|event| {
            matches!(
                event,
                EventKind::RequestRetry {
                    delay_ms: Some(_),
                    ..
                }
            )
        })
        .await;
        if interrupt {
            h.send(Op::Interrupt { turn_id }).await;
            h.end(TurnStatus::Interrupted).await;
        }
        h.send(Op::Retry).await;
        assert_eq!(h.request().await, failed);
        h.end(TurnStatus::Failed).await;
        assert!(h.requests.try_recv().is_err());
        h.stop().await;
    }
}

#[tokio::test]
async fn ambiguous_requests_retry_once_inside_the_shared_budget() {
    for (kinds, maximum, waits) in [
        (vec![ErrorKind::AmbiguousRequest], 3, vec![2000]),
        (
            vec![ErrorKind::AmbiguousRequest, ErrorKind::AmbiguousRequest],
            3,
            vec![2000],
        ),
        (
            vec![
                ErrorKind::Overloaded,
                ErrorKind::AmbiguousRequest,
                ErrorKind::Overloaded,
            ],
            2,
            vec![1, 2000],
        ),
        (
            vec![ErrorKind::Overloaded, ErrorKind::AmbiguousRequest],
            1,
            vec![1],
        ),
        (vec![ErrorKind::AmbiguousRequest], 0, vec![]),
    ] {
        let success = kinds.len() == waits.len();
        let mut scripts: Vec<_> = kinds.iter().copied().map(Script::error).collect();
        if success {
            scripts.push(answer("recovered"));
        }
        let mut h = Harness::with_retry(
            scripts,
            Arc::new(BoundedRetry {
                max_retries: maximum,
                initial_delay: Duration::from_millis(1),
                max_delay: Duration::from_secs(60),
            }),
        )
        .await;
        h.input("original task", InputDelivery::Steer).await;
        let original = h.request().await;
        for (index, delay) in waits.iter().enumerate() {
            let event = h
                .until(|event| {
                    matches!(
                        event,
                        EventKind::RequestRetry {
                            delay_ms: Some(_),
                            ..
                        }
                    )
                })
                .await;
            assert!(
                matches!(event.event, EventKind::RequestRetry { attempt, delay_ms: Some(actual), .. } if attempt == index as u32 + 1 && actual == *delay)
            );
            if *delay == 2000 {
                h.send(Op::Retry).await;
            }
            assert_eq!(h.request().await, original);
        }
        h.end(if success {
            TurnStatus::Completed
        } else {
            TurnStatus::Failed
        })
        .await;
        assert!(h.requests.try_recv().is_err());
        h.stop().await;
    }
}

#[tokio::test]
async fn settings_changes_during_retry_wait_apply_without_cancelling_it() {
    let mut h = Harness::with_retry(
        vec![Script::error(ErrorKind::Overloaded), answer("recovered")],
        Arc::new(BoundedRetry {
            max_retries: 1,
            initial_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(1),
        }),
    )
    .await;
    h.input("task", InputDelivery::Steer).await;
    let first = h.request().await;
    h.until(|event| {
        matches!(
            event,
            EventKind::RequestRetry {
                delay_ms: Some(_),
                ..
            }
        )
    })
    .await;
    h.send(Op::SetModel {
        provider: "fake".into(),
        model: "b".into(),
    })
    .await;
    h.send(Op::SetThinking {
        level: ThinkingLevel::High,
    })
    .await;
    let second = h.request().await;
    assert_eq!(second.model, ModelId("b".into()));
    assert_eq!(second.reasoning, Some(ReasoningLevel::High));
    assert_eq!(second.messages, first.messages);
    h.end(TurnStatus::Completed).await;
    assert!(!h.seen.iter().any(|event| matches!(&event.event, EventKind::TurnCompleted { turn } if turn.status == TurnStatus::Interrupted)));
    h.stop().await;
}

#[tokio::test]
async fn ask_and_approvals_serialize_and_block_retry_until_interrupt() {
    let (_release, wait) = oneshot::channel();
    let mut h = Harness::new(vec![Script {
        error: None,
        steps: vec![Step::Wait(wait)].into(),
    }])
    .await;
    h.input("task", InputDelivery::Steer).await;
    let turn_id = h.started_turn().await;
    h.request().await;
    let interactions = h.interactions.clone();
    let mut ask = Box::pin(interactions.ask(
        ApprovalRequest::Questions {
            questions: vec![Question {
                id: "choice".into(),
                header: "Choice".into(),
                question: "Which?".into(),
                options: None,
            }],
        },
        CancellationToken::new(),
    ));
    let mut permission = Box::pin(interactions.ask(
        ApprovalRequest::Permission {
            actor: None,
            call_id: "c".into(),
            tool: "bash".into(),
            arguments: json!({}),
            surface: "bash".into(),
            values: vec![],
            matched_pattern: None,
            origin: "test".into(),
            session_patterns: vec![],
        },
        CancellationToken::new(),
    ));
    assert!(futures::poll!(&mut ask).is_pending());
    assert!(futures::poll!(&mut permission).is_pending());
    h.until(|event| {
        matches!(
            event,
            EventKind::ApprovalRequested {
                request: ApprovalRequest::Questions { .. },
                ..
            }
        )
    })
    .await;
    h.send(Op::Retry).await;
    h.until(|event| matches!(event, EventKind::Error { message } if message == "Finish the current interaction before retrying.")).await;
    assert_eq!(
        h.seen
            .iter()
            .filter(|event| matches!(event.event, EventKind::ApprovalRequested { .. }))
            .count(),
        1
    );
    h.send(Op::Interrupt { turn_id }).await;
    assert_eq!(ask.await, Some(ApprovalDecision::Cancel));
    assert_eq!(permission.await, Some(ApprovalDecision::Cancel));
    h.end(TurnStatus::Interrupted).await;
    assert_eq!(
        h.seen
            .iter()
            .filter(|event| matches!(event.event, EventKind::ApprovalResolved { .. }))
            .count(),
        1
    );
    h.stop().await;
}

#[tokio::test]
async fn retry_after_partial_stream_does_not_repeat_completed_tools_or_replay_partial_blocks() {
    let failed = Script {
        error: None,
        steps: vec![
            Step::Event(StreamEvent::ContentComplete {
                index: 0,
                content: Content::Thinking {
                    text: "partial thought".into(),
                    redacted: false,
                    opaque: Some(OpaqueData("signature".into())),
                },
            }),
            Step::Event(StreamEvent::ContentComplete {
                index: 1,
                content: call("incomplete"),
            }),
            Step::Error(provider_error(ErrorKind::TransientNetwork, "stream failed")),
        ]
        .into(),
    };
    let mut h = Harness::new(vec![
        reply(vec![call("completed")]),
        failed,
        answer("success"),
    ])
    .await;
    h.input("task", InputDelivery::Steer).await;
    h.request().await;
    let before = h.request().await;
    let retry = h.request().await;
    h.end(TurnStatus::Completed).await;
    assert_eq!(before, retry);
    assert_eq!(h.tool.runs.load(Ordering::SeqCst), 1);
    h.valid_transcript().await;
    assert!(h.snapshot().await.build_context().iter().flat_map(|message| &message.content).any(|content| matches!(content, Content::ToolResult(result) if result.content == vec![text_content("No result provided".into())])));
    h.stop().await;
}

#[tokio::test]
async fn retry_exhausted_fails_after_bounded_attempts() {
    let mut h = Harness::new(
        (0..3)
            .map(|_| Script::error(ErrorKind::Overloaded))
            .collect(),
    )
    .await;
    h.input("task", InputDelivery::Steer).await;
    for _ in 0..3 {
        h.request().await;
    }
    h.end(TurnStatus::Failed).await;
    assert!(h.seen.iter().any(|event| matches!(&event.event, EventKind::Error { message } if message.contains("scripted failure"))));
    h.stop().await;
}

#[tokio::test]
async fn non_retryable_errors_end_the_turn_even_with_permissive_policy() {
    struct Always;
    impl RetryPolicy for Always {
        fn delay(&self, _: &ProviderError, _: u32) -> Option<Duration> {
            Some(Duration::ZERO)
        }
    }
    for kind in [
        ErrorKind::ContextOverflow,
        ErrorKind::Auth,
        ErrorKind::InvalidRequest,
        ErrorKind::Cancelled,
    ] {
        let mut h = Harness::with_retry(vec![Script::error(kind)], Arc::new(Always)).await;
        h.input("task", InputDelivery::Steer).await;
        h.request().await;
        h.end(if kind == ErrorKind::Cancelled {
            TurnStatus::Interrupted
        } else {
            TurnStatus::Failed
        })
        .await;
        if kind == ErrorKind::Cancelled {
            h.valid_transcript().await;
        } else {
            assert!(
                h.seen
                    .iter()
                    .any(|event| matches!(event.event, EventKind::Error { .. }))
            );
        }
        assert!(h.requests.try_recv().is_err());
        h.stop().await;
    }
}

#[tokio::test]
async fn interrupt_cancels_retry_backoff() {
    let retry = BoundedRetry {
        max_retries: 2,
        initial_delay: Duration::from_secs(3600),
        max_delay: Duration::from_secs(3600),
    };
    let mut h = Harness::with_retry(
        vec![Script::error(ErrorKind::TransientNetwork)],
        Arc::new(retry),
    )
    .await;
    h.input("task", InputDelivery::Steer).await;
    let turn = h.started_turn().await;
    h.request().await;
    h.send(Op::Interrupt { turn_id: turn }).await;
    h.end(TurnStatus::Interrupted).await;
    h.valid_transcript().await;
    assert!(h.requests.try_recv().is_err());
    h.stop().await;
}

#[tokio::test]
async fn invalid_arguments_unknown_tools_and_execution_errors_are_tool_results() {
    let mut invalid = call("invalid");
    if let Content::ToolCall(call) = &mut invalid {
        call.arguments = json!({});
    }
    let unknown = Content::ToolCall(ToolCall {
        id: "unknown".into(),
        name: "missing".into(),
        arguments: json!({}),
        opaque: None,
    });
    let mut h = Harness::new(vec![
        reply(vec![invalid, unknown, call("fail")]),
        answer("done"),
    ])
    .await;
    h.input("task", InputDelivery::Steer).await;
    h.request().await;
    let request = h.request().await;
    h.end(TurnStatus::Completed).await;
    let results = results(&request);
    assert!(results.iter().all(|result| result.is_error));
    let errors: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/tool-errors.json")).unwrap();
    assert_eq!(
        results[0].content,
        [text_content(
            errors["validation"][0]["text"].as_str().unwrap().into()
        )]
    );
    assert_eq!(
        results[1].content,
        [text_content(errors["missing"].as_str().unwrap().into())]
    );
    assert_eq!(
        results[2].content,
        [text_content(errors["failed"].as_str().unwrap().into())]
    );
    assert_eq!(h.tool.runs.load(Ordering::SeqCst), 1);
    h.valid_transcript().await;
    h.stop().await;
}

#[tokio::test]
async fn truncated_tool_calls_are_failed_without_execution() {
    let script = Script::events(vec![
        StreamEvent::ContentComplete {
            index: 0,
            content: call("truncated"),
        },
        finish(StopReason::Length),
    ]);
    let mut h = Harness::new(vec![script, answer("done")]).await;
    h.input("task", InputDelivery::Steer).await;
    h.request().await;
    let request = h.request().await;
    h.end(TurnStatus::Completed).await;
    let errors: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/tool-errors.json")).unwrap();
    assert_eq!(
        results(&request)[0].content,
        [text_content(errors["truncated"].as_str().unwrap().into())]
    );
    assert!(results(&request)[0].is_error);
    assert_eq!(h.tool.runs.load(Ordering::SeqCst), 0);
    h.valid_transcript().await;
    h.stop().await;
}

#[test]
fn retry_policy_has_exponential_cap_and_retry_after_floor() {
    let retry = BoundedRetry {
        max_retries: 4,
        initial_delay: Duration::from_secs(2),
        max_delay: Duration::from_secs(5),
    };
    let mut error = provider_error(ErrorKind::Overloaded, "busy");
    assert_eq!(retry.delay(&error, 0), Some(Duration::from_secs(2)));
    assert_eq!(retry.delay(&error, 1), Some(Duration::from_secs(4)));
    assert_eq!(retry.delay(&error, 2), Some(Duration::from_secs(5)));
    error.retry_after = Some(Duration::from_secs(5));
    assert_eq!(retry.delay(&error, 3), Some(Duration::from_secs(5)));
    assert_eq!(retry.delay(&error, 4), None);
    error.retry_after = Some(Duration::from_secs(80));
    assert_eq!(retry.delay(&error, 0), None);
}

#[tokio::test]
async fn retry_after_above_maximum_surfaces_error_without_retrying() {
    let mut script = Script::error(ErrorKind::RateLimit);
    script.error.as_mut().unwrap().retry_after = Some(Duration::from_secs(60));
    let mut h = Harness::with_retry(
        vec![script],
        Arc::new(BoundedRetry {
            max_retries: 2,
            initial_delay: Duration::ZERO,
            max_delay: Duration::from_secs(1),
        }),
    )
    .await;
    h.input("task", InputDelivery::Steer).await;
    h.request().await;
    h.end(TurnStatus::Failed).await;
    assert!(h.requests.try_recv().is_err());
    assert!(h.seen.iter().any(|event| matches!(&event.event, EventKind::Error { message } if message.contains("scripted failure"))));
    h.stop().await;
}

#[tokio::test]
async fn tool_receives_converted_arguments_while_transcript_keeps_original_call() {
    let schema = json!({"type":"object", "properties":{
        "value":{"type":"integer"}, "enabled":{"type":"boolean"}, "optional":{"type":"string"}
    }, "required":["value","enabled"]});
    let original = json!({"value":"5", "enabled":"true", "optional":null});
    let call = Content::ToolCall(ToolCall {
        id: "converted".into(),
        name: "probe".into(),
        arguments: original.clone(),
        opaque: None,
    });
    let mut h = Harness::with_schema(
        vec![reply(vec![call]), answer("done")],
        Arc::new(BoundedRetry::default()),
        schema,
    )
    .await;
    h.input("task", InputDelivery::Steer).await;
    h.request().await;
    let request = h.request().await;
    h.end(TurnStatus::Completed).await;
    assert_eq!(
        *h.tool.arguments.lock().unwrap(),
        [json!({"value":5,"enabled":true})]
    );
    let persisted = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|content| {
            if let Content::ToolCall(call) = content {
                Some(&call.arguments)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(persisted, &original);
    h.valid_transcript().await;
    h.stop().await;
}

#[test]
fn protocol_thinking_levels_map_to_model_levels() {
    for (protocol, model) in [
        (ThinkingLevel::Off, None),
        (ThinkingLevel::Minimal, Some(ReasoningLevel::Minimal)),
        (ThinkingLevel::Low, Some(ReasoningLevel::Low)),
        (ThinkingLevel::Medium, Some(ReasoningLevel::Medium)),
        (ThinkingLevel::High, Some(ReasoningLevel::High)),
        (ThinkingLevel::Xhigh, Some(ReasoningLevel::Xhigh)),
        (ThinkingLevel::Max, Some(ReasoningLevel::Max)),
    ] {
        assert_eq!(thinking_level(protocol), model);
    }
}

#[test]
fn system_prompt_retains_order_whitespace_and_empty_fragments() {
    assert_eq!(
        assemble_system_prompt(&[" a\n".into(), "".into(), "b ".into()]),
        " a\n\n\n\n\nb "
    );
    assert_eq!(assemble_system_prompt(&[]), "");
}

#[tokio::test]
async fn validation_errors_match_fixtures() {
    let h = Harness::new(vec![]).await;
    let errors: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/tool-errors.json")).unwrap();
    let mut registry = ToolRegistry::default();
    registry.register(h.tool.clone()).unwrap();
    for fixture in errors["validation"].as_array().unwrap() {
        let call = ToolCall {
            id: "validation".into(),
            name: "probe".into(),
            arguments: fixture["arguments"].clone(),
            opaque: None,
        };
        assert_eq!(
            validation::validate(&registry, &call).unwrap_err(),
            fixture["text"].as_str().unwrap()
        );
    }
    h.stop().await;
}

#[tokio::test]
async fn provider_cancelled_partial_call_gets_aborted_result() {
    let script = Script {
        error: None,
        steps: vec![
            Step::Event(StreamEvent::ContentComplete {
                index: 3,
                content: call("cancelled"),
            }),
            Step::Error(provider_error(ErrorKind::Cancelled, "cancelled")),
        ]
        .into(),
    };
    let mut h = Harness::new(vec![script]).await;
    h.input("task", InputDelivery::Steer).await;
    h.end(TurnStatus::Interrupted).await;
    h.valid_transcript().await;
    let errors: serde_json::Value =
        serde_json::from_str(include_str!("../tests/fixtures/tool-errors.json")).unwrap();
    let store = h.snapshot().await;
    let output = store
        .build_context()
        .into_iter()
        .flat_map(|message| message.content)
        .find_map(|content| {
            if let Content::ToolResult(result) = content {
                Some(result)
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(
        output.content,
        [text_content(errors["aborted"].as_str().unwrap().into())]
    );
    assert_eq!(h.tool.runs.load(Ordering::SeqCst), 0);
    h.stop().await;
}

#[tokio::test]
async fn thinking_is_clamped_per_model_and_the_requested_level_returns() {
    let mut h = Harness::new(vec![]).await;
    h.send(Op::SetThinking {
        level: ThinkingLevel::High,
    })
    .await;
    for model in ["plain", "b"] {
        h.send(Op::SetModel {
            provider: "fake".into(),
            model: model.into(),
        })
        .await;
    }
    let mut levels = Vec::new();
    for _ in 0..4 {
        let event = h
            .until(|event| matches!(event, EventKind::SessionUpdated { .. }))
            .await;
        if let EventKind::SessionUpdated {
            model, thinking, ..
        } = event.event
        {
            levels.push((model.id, thinking, model.thinking_levels.len()));
        }
    }
    assert_eq!(
        levels[1..],
        [
            ("a".into(), ThinkingLevel::High, 7),
            ("plain".into(), ThinkingLevel::Off, 1),
            ("b".into(), ThinkingLevel::High, 7),
        ]
    );
    h.stop().await;
}

#[tokio::test]
async fn historical_effort_survives_level_switch_and_reload() {
    fn efforts(store: &SessionStore) -> Vec<Option<ReasoningLevel>> {
        store
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                EntryKind::Message {
                    message, reasoning, ..
                } if message.role == Role::Assistant => {
                    assert!(
                        serde_json::to_value(message)
                            .unwrap()
                            .get("reasoning")
                            .is_none()
                    );
                    assert_eq!(
                        serde_json::to_value(entry).unwrap()["reasoning"],
                        serde_json::to_value(reasoning).unwrap()
                    );
                    Some(*reasoning)
                }
                _ => None,
            })
            .collect()
    }
    let mut h = Harness::new(vec![
        answer("low reply"),
        answer("high reply"),
        answer("resumed"),
    ])
    .await;
    h.send(Op::SetThinking {
        level: ThinkingLevel::Low,
    })
    .await;
    h.input("first", InputDelivery::Steer).await;
    assert_eq!(h.request().await.reasoning, Some(ReasoningLevel::Low));
    h.end(TurnStatus::Completed).await;
    assert_eq!(efforts(&h.snapshot().await), [Some(ReasoningLevel::Low)]);
    h.send(Op::SetThinking {
        level: ThinkingLevel::High,
    })
    .await;
    h.input("second", InputDelivery::Steer).await;
    let request = h.request().await;
    assert_eq!(request.reasoning, Some(ReasoningLevel::High));
    h.end(TurnStatus::Completed).await;
    let expected = [Some(ReasoningLevel::Low), Some(ReasoningLevel::High)];
    assert_eq!(efforts(&h.snapshot().await), expected);
    h.send(Op::Reload).await;
    let event = h
        .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    h.epoch = event.epoch;
    assert_eq!(efforts(&h.snapshot().await), expected);
    h.input("third", InputDelivery::Steer).await;
    let request = h.request().await;
    assert_eq!(request.reasoning, Some(ReasoningLevel::High));
    assert_eq!(
        request
            .messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .count(),
        2
    );
    h.end(TurnStatus::Completed).await;
    assert_eq!(
        efforts(&h.snapshot().await),
        [
            Some(ReasoningLevel::Low),
            Some(ReasoningLevel::High),
            Some(ReasoningLevel::High)
        ]
    );
    h.stop().await;
}

#[tokio::test]
async fn prompts_show_one_at_a_time_and_end_when_withdrawn_answered_or_reset() {
    let mut h = Harness::new(vec![]).await;
    let interactions = h.interactions.clone();
    let ask = |message: &str, cancel: &CancellationToken| {
        Box::pin(interactions.ask(
            ApprovalRequest::Elicitation {
                server: "docs".into(),
                request: json!({ "message": message }),
            },
            cancel.clone(),
        ))
    };
    let withdrawn = CancellationToken::new();
    let never = CancellationToken::new();
    let mut first = ask("first", &never);
    let mut second = ask("second", &withdrawn);
    let mut third = ask("third", &never);
    for pending in [&mut first, &mut second, &mut third] {
        assert!(futures::poll!(pending).is_pending());
    }
    let shown = async |h: &mut Harness| {
        let event = h
            .until(|event| {
                matches!(
                    event,
                    EventKind::ApprovalRequested { .. } | EventKind::ApprovalResolved { .. }
                )
            })
            .await;
        match event.event {
            EventKind::ApprovalRequested {
                approval_id,
                request: ApprovalRequest::Elicitation { request, .. },
                turn_id: None,
            } => format!("shown {} {}", approval_id.0, request["message"]),
            EventKind::ApprovalResolved { approval_id } => format!("resolved {}", approval_id.0),
            other => format!("{other:?}"),
        }
    };
    assert_eq!(shown(&mut h).await, "shown approval-1 \"first\"");
    withdrawn.cancel();
    assert_eq!(second.await, None);
    for approval_id in ["approval-2", "approval-1"] {
        h.send(Op::ApprovalResponse {
            approval_id: ApprovalId(approval_id.into()),
            decision: ApprovalDecision::Accept { content: None },
        })
        .await;
    }
    assert_eq!(
        first.await,
        Some(ApprovalDecision::Accept { content: None })
    );
    assert_eq!(shown(&mut h).await, "resolved approval-1");
    assert_eq!(shown(&mut h).await, "shown approval-3 \"third\"");
    h.send(Op::Reload).await;
    assert_eq!(third.await, Some(ApprovalDecision::Cancel));
    assert_eq!(shown(&mut h).await, "resolved approval-3");
    h.stop().await;
}

#[test]
fn clamping_prefers_the_nearest_higher_level() {
    use ReasoningLevel as R;
    let supported = [Some(R::Low), Some(R::High), Some(R::Max)];
    assert_eq!(crate::clamp(None, &supported), Some(R::Low));
    assert_eq!(crate::clamp(Some(R::Medium), &supported), Some(R::High));
    assert_eq!(crate::clamp(Some(R::Xhigh), &supported), Some(R::Max));
    assert_eq!(
        crate::clamp(Some(R::Max), &[None, Some(R::Low)]),
        Some(R::Low)
    );
    assert_eq!(crate::clamp(Some(R::High), &[None]), None);
}

#[tokio::test]
async fn model_and_thinking_changes_survive_reload_and_resume_without_changing_origin() {
    let (release, gate) = oneshot::channel();
    let held = Script {
        error: None,
        steps: vec![
            Step::Event(StreamEvent::TextDelta {
                index: 0,
                delta: "old model".into(),
            }),
            Step::Wait(gate),
            Step::Event(finish(StopReason::EndTurn)),
        ]
        .into(),
    };
    let mut h = Harness::new(vec![held, answer("new model"), answer("reloaded")]).await;
    h.input("task", InputDelivery::Steer).await;
    h.request().await;
    h.until(|event| matches!(event, EventKind::ItemDelta { .. }))
        .await;
    h.send(Op::SetModel {
        provider: "fake".into(),
        model: "b".into(),
    })
    .await;
    h.send(Op::SetThinking {
        level: ThinkingLevel::High,
    })
    .await;
    h.send(Op::SetModel {
        provider: "missing".into(),
        model: "invalid".into(),
    })
    .await;
    h.until(|event| matches!(event, EventKind::Error { .. }))
        .await;
    release.send(()).unwrap();
    h.end(TurnStatus::Completed).await;
    let store = h.snapshot().await;
    let origins: Vec<_> = store
        .entries()
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::Message {
                message,
                model,
                reasoning,
                ..
            } if message.role == Role::Assistant => {
                assert_eq!(*reasoning, None);
                model.as_ref()
            }
            _ => None,
        })
        .collect();
    assert_eq!(origins, [&origin("a")]);
    h.input("next", InputDelivery::Steer).await;
    let request = h.request().await;
    h.end(TurnStatus::Completed).await;
    assert_eq!(request.model, ModelId("b".into()));
    assert_eq!(request.reasoning, Some(ReasoningLevel::High));
    h.send(Op::Reload).await;
    let event = h
        .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    assert_eq!(event.epoch, SessionEpoch(1));
    h.epoch = event.epoch;
    h.input("reload", InputDelivery::Steer).await;
    let reloaded = h.request().await;
    h.end(TurnStatus::Completed).await;
    assert_eq!(reloaded.model, ModelId("b".into()));
    assert_eq!(reloaded.reasoning, Some(ReasoningLevel::High));
    // Reopen in a new actor with conflicting defaults; persisted settings must win.
    h.shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), &mut h.handle.task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let path = h.path.clone();
    let store = tokio::task::spawn_blocking(move || {
        SessionStore::open(&path, &CancellationToken::new()).unwrap()
    })
    .await
    .unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(vec![answer("resumed")].into()),
        requests: tx,
    });
    let config = SessionConfig {
        compaction: Default::default(),
        model: origin("wrong-default"),
        thinking: None,
        providers: BTreeMap::from([("fake".into(), provider as Arc<dyn ModelProvider>)]),
        registry: Registry::default(),
        retry: Arc::new(BoundedRetry::default()),
        interactions: None,
        epoch: SessionEpoch(4),
    };
    let shutdown = CancellationToken::new();
    let mut resumed = SessionActor::spawn(store, config, shutdown.clone());
    let initial = resumed.events.recv().await.unwrap();
    let EventKind::ThreadStarted { thread, .. } = initial.event else {
        panic!("expected ThreadStarted");
    };
    assert_eq!(
        (initial.epoch, thread.epoch),
        (SessionEpoch(4), SessionEpoch(4))
    );
    let transcript: Vec<_> = thread
        .turns
        .iter()
        .map(|turn| {
            turn.items
                .iter()
                .map(|item| match item {
                    Item::Known(KnownItem::UserMessage { content, .. }) => match &content[0] {
                        InputContent::Text { text } => format!("user:{text}"),
                        InputContent::Image { .. } => "user:image".into(),
                    },
                    Item::Known(KnownItem::AssistantMessage { text, .. }) => {
                        format!("assistant:{text}")
                    }
                    other => format!("{other:?}"),
                })
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(
        transcript,
        [
            ["user:task", "assistant:old model"],
            ["user:next", "assistant:new model"],
            ["user:reload", "assistant:reloaded"],
        ]
    );
    let usage = TokenUsage {
        input: 3,
        output: 2,
        cache_read: 1,
        cache_write: 4,
        ..Default::default()
    };
    assert_eq!(
        resumed.events.recv().await.unwrap().event,
        EventKind::SessionUpdated {
            model: ModelInfo {
                provider: "fake".into(),
                id: "b".into(),
                context_window: 10000,
                reasoning: true,
                thinking_levels: vec![
                    ThinkingLevel::Off,
                    ThinkingLevel::Minimal,
                    ThinkingLevel::Low,
                    ThinkingLevel::Medium,
                    ThinkingLevel::High,
                    ThinkingLevel::Xhigh,
                    ThinkingLevel::Max,
                ],
            },
            thinking: ThinkingLevel::High,
            context: Some(usage),
            total: TokenUsage {
                input: 9,
                output: 6,
                cache_read: 3,
                cache_write: 12,
                ..Default::default()
            },
        }
    );
    assert!(rx.try_recv().is_err(), "resume must not request the model");
    resumed
        .submissions
        .send(Submission {
            id: SubmissionId("resume".into()),
            thread_id: h.thread.clone(),
            epoch: SessionEpoch(4),
            op: Op::UserInput {
                content: vec![InputContent::Text {
                    text: "resume".into(),
                }],
                delivery: InputDelivery::Steer,
            },
        })
        .await
        .unwrap();
    let request = recv(&mut rx).await;
    assert_eq!(request.model, ModelId("b".into()));
    assert_eq!(request.reasoning, Some(ReasoningLevel::High));
    loop {
        if matches!(
            resumed.events.recv().await.unwrap().event,
            EventKind::TurnCompleted { .. }
        ) {
            break;
        }
    }
    shutdown.cancel();
    resumed.task.await.unwrap().unwrap();
}

#[tokio::test]
async fn branch_interrupts_old_work_and_restores_selected_settings_and_context() {
    let (_release, gate) = oneshot::channel();
    let held = Script {
        error: None,
        steps: vec![
            Step::Event(StreamEvent::TextDelta {
                index: 0,
                delta: "partial".into(),
            }),
            Step::Wait(gate),
        ]
        .into(),
    };
    let mut h = Harness::new(vec![held, answer("new branch")]).await;
    h.input("old branch", InputDelivery::Steer).await;
    h.until(|event| matches!(event, EventKind::ItemDelta { .. }))
        .await;
    h.send(Op::SetModel {
        provider: "fake".into(),
        model: "b".into(),
    })
    .await;
    h.send(Op::SetThinking {
        level: ThinkingLevel::Max,
    })
    .await;
    h.send(Op::Branch {
        entry_id: h.thread.0.clone(),
    })
    .await;
    h.end(TurnStatus::Interrupted).await;
    h.valid_transcript().await;
    let event = h
        .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    assert_eq!(event.epoch, SessionEpoch(1));
    // An input carrying the previous epoch must be discarded.
    h.input("stale input", InputDelivery::Steer).await;
    h.epoch = event.epoch;
    h.input("new branch", InputDelivery::Steer).await;
    h.request().await;
    let request = h.request().await;
    h.end(TurnStatus::Completed).await;
    assert_eq!(users(&request), ["new branch"]);
    assert_eq!(request.model, ModelId("a".into()));
    assert_eq!(request.reasoning, None);
    h.stop().await;
}

#[tokio::test]
async fn asynchronous_tool_update_from_old_epoch_is_discarded_during_new_turn() {
    let mut h = Harness::new(vec![
        reply(vec![call("same-id")]),
        answer("old"),
        reply(vec![call("same-id")]),
        answer("new"),
    ])
    .await;
    let (release_late, late) = oneshot::channel();
    *h.tool.late.lock().unwrap() = Some(late);
    h.input("old", InputDelivery::Steer).await;
    h.end(TurnStatus::Completed).await;
    recv(&mut h.started).await;
    h.send(Op::Reload).await;
    let event = h
        .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    h.epoch = event.epoch;
    let release_current = h.gate_tool("same-id");
    h.input("new", InputDelivery::Steer).await;
    recv(&mut h.started).await;
    release_late.send(()).unwrap();
    recv(&mut h.late_sent).await;
    release_current.send(()).unwrap();
    h.end(TurnStatus::Completed).await;
    assert!(!h.seen.iter().any(|event| matches!(&event.event, EventKind::ItemDelta { delta, .. } if delta.contains("stale update"))));
    assert!(h.seen.iter().any(|event| event.epoch == SessionEpoch(1) && matches!(&event.event, EventKind::ItemDelta { delta, .. } if delta.contains("current update"))));
    h.valid_transcript().await;
    h.stop().await;
}

#[tokio::test]
async fn streamed_item_ids_match_started_delta_and_completed_events() {
    let script = Script::events(vec![
        StreamEvent::TextDelta {
            index: 7,
            delta: "a".into(),
        },
        StreamEvent::TextDelta {
            index: 7,
            delta: "b".into(),
        },
        StreamEvent::ContentComplete {
            index: 7,
            content: text_content("ab".into()),
        },
        finish(StopReason::EndTurn),
    ]);
    let mut h = Harness::new(vec![script]).await;
    h.input("task", InputDelivery::Steer).await;
    h.end(TurnStatus::Completed).await;
    assert_event_items(&h.seen);
    h.stop().await;
}

fn assert_event_items(events: &[Event]) {
    let mut open = HashSet::new();
    for event in events {
        match &event.event {
            EventKind::ItemStarted {
                item: Item::Known(item),
                ..
            } => assert!(open.insert(item_id(item))),
            EventKind::ItemDelta { item_id, .. } => assert!(open.contains(item_id)),
            EventKind::ItemCompleted {
                item: Item::Known(item),
                ..
            } => assert!(open.remove(&item_id(item))),
            _ => {}
        }
    }
    assert!(open.is_empty());
}

#[path = "extension_tests.rs"]
mod extensions;

#[path = "hooks_tests.rs"]
mod hooks_tests;

struct Echo;

#[async_trait]
impl tiechui_plugin_api::Command for Echo {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "echo".into(),
            description: "Echo arguments".into(),
            argument_hint: None,
        }
    }
    async fn run(&self, arguments: &str, _: CancellationToken) -> Result<String, ToolError> {
        Ok(format!("args: {arguments}"))
    }
}

struct Shell(ToolSpec);

#[async_trait]
impl Tool for Shell {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn concurrency(&self, _: &serde_json::Value) -> Concurrency {
        Concurrency::Exclusive
    }
    async fn run(&self, call: ToolCall, _: ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::text(format!(
            "ran {}",
            call.arguments["command"].as_str().unwrap_or_default()
        )))
    }
}

async fn commanded(scripts: Vec<Script>) -> Harness {
    Harness::with_contributions(
        scripts,
        Arc::new(BoundedRetry::default()),
        json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}),
        |registry| {
            registry.register_command(Arc::new(Echo));
            registry
                .register_tool(Arc::new(Shell(ToolSpec {
                    name: "bash".into(),
                    description: "Offline shell".into(),
                    input_schema: json!({"type":"object"}),
                })))
                .unwrap();
        },
    )
    .await
}

#[tokio::test]
async fn module_commands_are_listed_and_answer_without_touching_the_model() {
    let mut harness = commanded(vec![]).await;
    let listed = harness
        .until(|event| matches!(event, EventKind::CommandsUpdated { .. }))
        .await;
    let EventKind::CommandsUpdated { commands } = listed.event else {
        unreachable!()
    };
    assert_eq!(commands[0].name, "echo");
    harness
        .send(Op::RunCommand {
            name: "echo".into(),
            arguments: "a b".into(),
        })
        .await;
    let output = harness
        .until(|event| matches!(event, EventKind::CommandOutput { .. }))
        .await;
    assert_eq!(
        output.event,
        EventKind::CommandOutput {
            name: "echo".into(),
            text: "args: a b".into(),
        }
    );
    harness
        .send(Op::RunCommand {
            name: "missing".into(),
            arguments: String::new(),
        })
        .await;
    let error = harness
        .until(|event| matches!(event, EventKind::Error { .. }))
        .await;
    assert_eq!(
        error.event,
        EventKind::Error {
            message: "unknown command /missing".into()
        }
    );
    assert!(harness.requests.try_recv().is_err());
    harness.stop().await;
}

#[tokio::test]
async fn user_shell_runs_as_a_tool_call_and_enters_the_context_between_turns() {
    let mut harness = commanded(vec![reply(vec![call("held")]), answer("done")]).await;
    harness
        .send(Op::RunShell {
            command: "ls -a".into(),
        })
        .await;
    harness.started_turn().await;
    harness.end(TurnStatus::Completed).await;
    let items: Vec<&KnownItem> = harness
        .seen
        .iter()
        .filter_map(|event| match &event.event {
            EventKind::ItemCompleted {
                item: Item::Known(item),
                ..
            } => Some(item),
            _ => None,
        })
        .collect();
    assert!(
        matches!(items[0], KnownItem::ToolCall { name, arguments, .. }
        if name == "bash" && arguments["command"] == "ls -a")
    );
    assert!(
        matches!(items[1], KnownItem::ToolResult { output, is_error: false, .. }
        if output["content"][0]["text"] == "ran ls -a")
    );
    // During a turn the command is refused rather than queued.
    let gate = harness.gate_tool("held");
    harness.input("next", InputDelivery::FollowUp).await;
    let request = harness.request().await;
    assert_eq!(
        users(&request),
        ["Ran `ls -a`\n```\nran ls -a\n```", "next"]
    );
    harness
        .send(Op::RunShell {
            command: "pwd".into(),
        })
        .await;
    let error = harness
        .until(|event| matches!(event, EventKind::Error { .. }))
        .await;
    assert_eq!(
        error.event,
        EventKind::Error {
            message: "shell commands run between turns".into()
        }
    );
    gate.send(()).unwrap();
    harness.end(TurnStatus::Completed).await;
    harness.valid_transcript().await;
    harness.stop().await;
}

#[path = "replay_resume_tests.rs"]
mod replay_resume;

#[path = "inspection_tests.rs"]
mod inspection_tests;

#[path = "rewind_tests.rs"]
mod rewind_tests;

#[path = "btw_tests.rs"]
mod btw_tests;

#[path = "subagent_tests.rs"]
mod subagent_tests;

#[path = "goal_tests.rs"]
mod goal_tests;

#[cfg(unix)]
#[path = "shell_tests.rs"]
mod shell_tests;

#[path = "compaction_tests.rs"]
mod compaction_tests;

#[path = "tool_search_tests.rs"]
mod tool_search_tests;

#[tokio::test]
async fn interceptors_stream_one_off_requests_on_the_turn_model() {
    struct Review;
    #[async_trait]
    impl tiechui_plugin_api::ToolInterceptor for Review {
        async fn before(
            &self,
            _: &ToolCall,
            ctx: ToolContext,
        ) -> Result<tiechui_plugin_api::BeforeToolCall, ToolError> {
            let invoker = ctx.invoker.clone().unwrap();
            let request = ModelRequest {
                disable_cache: true,
                model: invoker.model().unwrap(),
                messages: vec![],
                input_tokens: None,
                tools: vec![],
                max_output: None,
                reasoning: None,
            };
            let mut stream = invoker.model_stream(request, ctx.cancel.clone()).await?;
            let mut text = String::new();
            while let Some(event) = stream.next().await {
                if let StreamEvent::ContentComplete {
                    content: Content::Text { text: part, .. },
                    ..
                } = event?
                {
                    text.push_str(&part);
                }
            }
            Ok(tiechui_plugin_api::BeforeToolCall::Deny(text))
        }
    }
    let mut h = Harness::with_contributions(
        vec![reply(vec![call("a")]), answer("reviewed"), answer("done")],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}),
        |registry| registry.register_tool_interceptor(Arc::new(Review), Duration::from_secs(5)),
    )
    .await;
    h.input("task", InputDelivery::Steer).await;
    h.request().await;
    let review = h.request().await;
    assert!(review.disable_cache && review.messages.is_empty());
    assert_eq!(
        results(&h.request().await)[0].content,
        vec![text_content("reviewed".into())]
    );
    h.end(TurnStatus::Completed).await;
    h.stop().await;
}
