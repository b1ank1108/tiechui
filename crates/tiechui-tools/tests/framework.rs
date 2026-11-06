#![allow(clippy::unwrap_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tiechui_model::{ToolCall, ToolSpec};
use tiechui_protocol::{SessionEpoch, ThreadId, TurnId};
use tiechui_tools::{
    BashTool, Concurrency, ExecutionScope, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
    ToolScheduler,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn context() -> ToolContext {
    ToolContext {
        invoker: None,
        branch: Default::default(),
        parent_session: None,
        cwd: std::env::current_dir().unwrap(),
        cancel: CancellationToken::new(),
        updates: Arc::new(|_| {}),
        append_entry: Arc::new(|_| Ok(())),
        epoch: SessionEpoch(7),
        scope: ExecutionScope {
            thread_id: ThreadId("thread".into()),
            turn_id: TurnId("turn".into()),
        },
        origin: Default::default(),
    }
}

fn call(id: &str, shared: bool, delay: u64) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "fake".into(),
        arguments: json!({"shared": shared, "delay": delay}),
        opaque: None,
    }
}

#[derive(Default)]
struct State {
    active: AtomicUsize,
    peak: AtomicUsize,
    events: Mutex<Vec<String>>,
    parallel: Notify,
}

struct Fake {
    spec: ToolSpec,
    state: Arc<State>,
}

impl Fake {
    fn new(name: &str, state: Arc<State>) -> Self {
        Self {
            spec: ToolSpec {
                name: name.into(),
                description: "Instrumented tool".into(),
                input_schema: json!({"type":"object"}),
            },
            state,
        }
    }
}

#[async_trait]
impl Tool for Fake {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, input: &Value) -> Concurrency {
        if input["shared"] == true {
            Concurrency::Shared
        } else {
            Concurrency::Exclusive
        }
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        assert_eq!(ctx.scope, context().scope);
        assert_eq!(ctx.epoch, SessionEpoch(7));
        let active = self.state.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.state.peak.fetch_max(active, Ordering::SeqCst);
        if self.concurrency(&call.arguments) == Concurrency::Exclusive {
            assert_eq!(active, 1);
        }
        self.state
            .events
            .lock()
            .unwrap()
            .push(format!("start:{}", call.id));
        if active >= 2 {
            self.state.parallel.notify_one();
        }
        if call.id == "a" {
            self.state.parallel.notified().await;
        }
        tokio::select! {
            _ = ctx.cancel.cancelled() => {},
            _ = tokio::time::sleep(Duration::from_millis(call.arguments["delay"].as_u64().unwrap())) => {},
        }
        self.state
            .events
            .lock()
            .unwrap()
            .push(format!("end:{}", call.id));
        self.state.active.fetch_sub(1, Ordering::SeqCst);
        if call.id == "error" {
            Err(ToolError("fake failure".into()))
        } else {
            Ok(ToolOutput::text(call.id))
        }
    }
}

#[test]
fn registry_spec_bytes_are_stable_and_snapshot_bash_contract() {
    let mut registry = ToolRegistry::default();
    registry
        .register(Arc::new(Fake::new("z", Arc::default())))
        .unwrap();
    registry.register(Arc::new(BashTool::default())).unwrap();
    let bytes = registry.spec_bytes().unwrap();
    insta::assert_snapshot!(
        "registry_spec_bytes",
        String::from_utf8(bytes.clone()).unwrap()
    );
    let mut reversed = ToolRegistry::default();
    reversed.register(Arc::new(BashTool::default())).unwrap();
    reversed
        .register(Arc::new(Fake::new("z", Arc::default())))
        .unwrap();
    assert_eq!(reversed.spec_bytes().unwrap(), bytes);
    assert!(registry.register(Arc::new(BashTool::default())).is_err());
    assert_eq!(registry.spec_bytes().unwrap(), bytes);
    let expected: ToolSpec = serde_json::from_str(include_str!("fixtures/bash-spec.json")).unwrap();
    assert_eq!(BashTool::default().spec(), &expected);
    assert_eq!(
        BashTool::default().concurrency(&Value::Null),
        Concurrency::Exclusive
    );
}

#[tokio::test]
async fn scheduler_parallelizes_shared_serializes_exclusive_and_keeps_result_order() {
    let state = Arc::new(State::default());
    let mut registry = ToolRegistry::default();
    registry
        .register(Arc::new(Fake::new("fake", state.clone())))
        .unwrap();
    let scheduler = ToolScheduler::default();
    let results = tokio::time::timeout(
        Duration::from_secs(3),
        scheduler.run_batch(
            &registry,
            vec![
                call("a", true, 20),
                call("b", true, 1),
                call("x", false, 1),
                call("c", true, 1),
                call("error", false, 1),
            ],
            context(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(
        results[..4]
            .iter()
            .map(|result| result.as_ref().unwrap().clone())
            .collect::<Vec<_>>(),
        ["a", "b", "x", "c"].map(ToolOutput::text)
    );
    assert_eq!(results[4], Err(ToolError("fake failure".into())));
    assert_eq!(state.peak.load(Ordering::SeqCst), 2);
    assert_eq!(
        *state.events.lock().unwrap(),
        [
            "start:a",
            "start:b",
            "end:b",
            "end:a",
            "start:x",
            "end:x",
            "start:c",
            "end:c",
            "start:error",
            "end:error"
        ]
    );
}

#[tokio::test]
async fn gate_spans_batches_and_waiting_calls_cancel_without_running() {
    let state = Arc::new(State::default());
    let mut registry = ToolRegistry::default();
    registry
        .register(Arc::new(Fake::new("fake", state.clone())))
        .unwrap();
    let scheduler = ToolScheduler::default();
    let ctx = context();
    let other_ctx = ctx.clone();
    let first = scheduler.run_batch(&registry, vec![call("x", false, 50)], ctx.clone());
    let second = async {
        tokio::time::sleep(Duration::from_millis(5)).await;
        scheduler
            .run_batch(&registry, vec![call("c", true, 1)], other_ctx)
            .await
    };
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(15)).await;
        ctx.cancel.cancel();
    };
    let (_, results, _) = tokio::join!(first, second, cancel);
    assert_eq!(results[0], Err(ToolError("Operation aborted".into())));
    assert_eq!(*state.events.lock().unwrap(), ["start:x", "end:x"]);
}

#[tokio::test]
async fn unknown_tool_error_does_not_drop_other_results() {
    let scheduler = ToolScheduler::default();
    let registry = ToolRegistry::default();
    assert!(
        scheduler
            .run_batch(&registry, vec![], context())
            .await
            .is_empty()
    );
    let result = scheduler
        .run_batch(&registry, vec![call("unknown", true, 0)], context())
        .await;
    assert_eq!(result, [Err(ToolError("Unknown tool: fake".into()))]);
}
