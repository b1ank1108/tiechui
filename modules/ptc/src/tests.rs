#![allow(clippy::unwrap_used)]

use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tiechui_plugin_api::{
    CancellationToken, ToolInvoker,
    model::Content,
    protocol::{SessionEpoch, ThreadId, TurnId},
    serde_json,
    tools::ExecutionScope,
};

struct Fixture {
    spec: ToolSpec,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    settled: Arc<AtomicUsize>,
}
#[async_trait]
impl Tool for Fixture {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn programmatic(&self) -> bool {
        self.spec.name != "ask"
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Shared
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(active, Ordering::SeqCst);
        let delay = call.arguments["delay"].as_u64().unwrap_or(0);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(delay)) => {},
            _ = ctx.cancel.cancelled() => {},
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.settled.fetch_add(1, Ordering::SeqCst);
        if ctx.cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        if call.arguments["image"] == true {
            return Ok(ToolOutput {
                content: vec![Content::Image {
                    mime_type: "image/png".into(),
                    data: "aGk=".into(),
                }],
                structured_content: Some(json!({"secret":true})),
                ..Default::default()
            });
        }
        let text = call.arguments["text"]
            .as_str()
            .unwrap_or("ok")
            .repeat(call.arguments["repeat"].as_u64().unwrap_or(1) as usize);
        Ok(ToolOutput {
            is_error: call.arguments["fail"] == true,
            ..ToolOutput::text(text)
        })
    }
}

struct Invoker {
    registry: Registry,
}
#[async_trait]
impl ToolInvoker for Invoker {
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.registry.tools().get(name)
    }
    async fn invoke(&self, call: ToolCall, ctx: ToolContext) -> ToolOutput {
        match self.get(&call.name).unwrap().run(call, ctx).await {
            Ok(output) => output,
            Err(error) => ToolOutput {
                is_error: true,
                ..ToolOutput::text(error.to_string())
            },
        }
    }
}
struct Harness {
    tool: RunCode,
    ctx: ToolContext,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    settled: Arc<AtomicUsize>,
    updates: Arc<Mutex<Vec<ToolUpdate>>>,
}
impl Harness {
    fn new() -> Self {
        let mut registry = Registry::default();
        PtcModule
            .register(&mut registry, &ModuleConfig::default())
            .unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let settled = Arc::new(AtomicUsize::new(0));
        for name in ["read", "ask"] {
            registry
                .register_tool(Arc::new(Fixture {
                    spec: ToolSpec {
                        name: name.into(),
                        description: String::new(),
                        input_schema: json!({"type":"object"}),
                    },
                    active: active.clone(),
                    peak: peak.clone(),
                    settled: settled.clone(),
                }))
                .unwrap();
        }
        let tool = RunCode {
            spec: registry.tools().get("run_code").unwrap().spec().clone(),
            budget: Duration::from_millis(100),
        };
        let updates = Arc::new(Mutex::new(vec![]));
        let captured = updates.clone();
        Self {
            tool,
            active,
            peak,
            settled,
            updates,
            ctx: ToolContext {
                invoker: Some(Arc::new(Invoker { registry })),
                branch: Default::default(),
                parent_session: None,
                cwd: ".".into(),
                cancel: CancellationToken::new(),
                updates: Arc::new(move |update| captured.lock().unwrap().push(update)),
                append_entry: Arc::new(|_| Ok(())),
                epoch: SessionEpoch(3),
                scope: ExecutionScope {
                    thread_id: ThreadId("test".into()),
                    turn_id: TurnId("turn".into()),
                },
                origin: Default::default(),
            },
        }
    }
    async fn args(&self, args: Value) -> ToolOutput {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.tool.run(
                ToolCall {
                    id: "program".into(),
                    name: "run_code".into(),
                    arguments: args,
                    opaque: None,
                },
                self.ctx.clone(),
            ),
        )
        .await
        .expect("program must settle")
        .unwrap()
    }
    async fn run(&self, code: &str) -> ToolOutput {
        self.args(json!({"code":code})).await
    }
}
fn record(output: &ToolOutput) -> ProgramRun {
    serde_json::from_value(output.details.clone().unwrap()).unwrap()
}

#[test]
fn module_is_on_by_default_and_wrapper_is_neither_deferred_nor_programmatic() {
    let mut config = ModuleConfig::default();
    let registry = Registry::from_modules(&[&PtcModule], &config).unwrap();
    assert!(registry.tools().get("run_code").is_some());
    config.modules.insert("ptc".into(), false);
    let registry = Registry::from_modules(&[&PtcModule], &config).unwrap();
    assert!(registry.tools().get("run_code").is_none());
    let h = Harness::new();
    assert!(!h.tool.programmatic());
    assert!(!h.tool.deferrable());
    assert_eq!(h.tool.concurrency(&json!({})), Concurrency::Delegated);
    assert_eq!(JS_BUDGET, Duration::from_secs(30));
}

#[tokio::test]
async fn returns_values_console_errors_and_image_placeholders() {
    let h = Harness::new();
    let result = h.run("const texts = await Promise.all([tools.read({text:'a'}), tools.read({text:'b'})]); return texts.join(',');").await;
    assert_eq!(output::text(&result), "a,b\n\ntool calls: 2 succeeded");
    let result = h.run("try { await tools.read({text:'missing',fail:true}); } catch (e) { console.log(e.tool); } return 'done';").await;
    assert!(!result.is_error);
    assert_eq!(
        output::text(&result),
        "done\n\nconsole:\nread\n\ntool calls: 0 succeeded, 1 failed\n✗ read {\"fail\":true,\"text\":\"missing\"}: missing"
    );
    let result = h.run("return await tools.read({image:true});").await;
    assert_eq!(
        output::text(&result),
        "[image: image/png, 2 bytes]\n\ntool calls: 1 succeeded"
    );
    for (code, expected) in [
        ("return {a:1};", "{\n  \"a\": 1\n}"),
        ("return;", "tool calls: 0 succeeded"),
    ] {
        let result = h.run(code).await;
        assert!(!result.is_error, "{result:?}");
        assert!(output::text(&result).starts_with(expected));
    }
}

#[tokio::test]
async fn exceptions_have_source_lines_and_circular_returns_fail() {
    let h = Harness::new();
    let result = h.run("const x = 1;\nthrow new Error('boom');").await;
    assert!(result.is_error);
    let text = output::text(&result);
    assert!(text.starts_with("Error: boom\n"), "{text}");
    assert!(text.contains("run_code.js:2"), "{text}");
    let result = h.run("const x = {}; x.x = x; return x;").await;
    assert!(result.is_error);
    let result = h.run("await tools.read({fail:true,text:'no'});").await;
    assert!(result.is_error);
    assert!(output::text(&result).contains("run_code.js:1"));
}

#[tokio::test]
async fn unavailable_and_control_tools_are_rejected_and_counted() {
    let h = Harness::new();
    for name in ["ask", "run_code", "unknown", "call_tool", "tool_search"] {
        let result = h.run(&format!("await tools.{name}({{}});")).await;
        assert!(result.is_error, "{name}");
        assert_eq!(record(&result).calls[0].state, ProgramCallState::Failed);
        assert!(output::text(&result).contains(name));
        if matches!(name, "ask" | "run_code") {
            assert!(output::text(&result).contains("cannot be called from run_code"));
        }
    }
    assert_eq!(h.settled.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn concurrency_is_bounded_and_waiting_does_not_consume_js_budget() {
    let h = Harness::new();
    let result = h
        .run("return await Promise.all(Array.from({length:12}, () => tools.read({delay:150}))); ")
        .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(h.peak.load(Ordering::SeqCst), 4);
    assert_eq!(h.active.load(Ordering::SeqCst), 0);
    assert_eq!(record(&result).calls.len(), 12);
    let updates = h.updates.lock().unwrap();
    assert!(!updates.is_empty());
    assert!(updates.iter().all(|update| update.call_id == "program"
        && update.epoch == SessionEpoch(3)
        && update.output.content.is_empty()));
}

#[tokio::test]
async fn completion_cancels_and_settles_unawaited_calls() {
    let h = Harness::new();
    let result = h.run("tools.read({delay:5000}); return 'done';").await;
    assert!(!result.is_error, "{result:?}");
    assert!(output::text(&result).contains("1 calls were still running and were cancelled"));
    assert_eq!(record(&result).calls[0].state, ProgramCallState::Cancelled);
    assert_eq!(h.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn interrupt_timeout_and_user_cancellation_settle_calls() {
    let h = Harness::new();
    let started = Instant::now();
    let result = h.run("while (true) {}").await;
    assert!(result.is_error);
    assert!(output::text(&result).contains("JavaScript execution time budget exceeded"));
    assert!(started.elapsed() < Duration::from_secs(2));
    let result = h
        .args(json!({"code":"await tools.read({delay:5000});", "timeout":1}))
        .await;
    assert!(result.is_error);
    assert!(output::text(&result).contains("Wall-clock timeout exceeded"));
    assert_eq!(record(&result).calls[0].state, ProgramCallState::Cancelled);
    let cancellation = async {
        while h.active.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        h.ctx.cancel.cancel();
    };
    let (result, ()) = tokio::join!(h.run("await tools.read({delay:5000});"), cancellation);
    assert!(result.is_error);
    assert_eq!(h.active.load(Ordering::SeqCst), 0);
    assert_eq!(record(&result).calls[0].state, ProgramCallState::Cancelled);
}

#[tokio::test]
async fn heap_stack_and_job_loops_are_bounded_and_runtime_recovers() {
    let h = Harness::new();
    for code in [
        "return new ArrayBuffer(70 * 1024 * 1024);",
        "try { new ArrayBuffer(70 * 1024 * 1024); } catch {} return 'caught';",
        "function f() { return f(); } f();",
        "while(true) { await Promise.resolve(); }",
    ] {
        let result = h.run(code).await;
        assert!(result.is_error, "{code}: {result:?}");
        assert!(!h.run("return 'alive';").await.is_error);
    }
}

#[tokio::test]
async fn call_result_console_and_final_output_limits() {
    let h = Harness::new();
    let result = h
        .run("for (let i=0;i<201;i++) { try { await tools.ask({}); } catch {} } return 'done';")
        .await;
    assert!(!result.is_error);
    assert_eq!(record(&result).calls.len(), 200);
    let text = output::text(&result);
    assert!(text.contains("200 failed"));
    assert_eq!(
        text.lines().filter(|line| line.starts_with('✗')).count(),
        10
    );
    assert!(text.contains("190 more failed calls"));
    let result = h.run("for(let i=0;i<201;i++) await tools.read({});").await;
    assert!(result.is_error);
    assert!(output::text(&result).contains("Tool call limit exceeded"));
    let result = h
        .run("return await tools.read({text:'x',repeat:1048577});")
        .await;
    assert!(result.is_error);
    assert!(output::text(&result).contains("has already executed"));
    assert!(output::text(&result).contains("1 MiB"));
    let result = h.run("console.log('中'.repeat(10000)); return 'ok';").await;
    let text = output::text(&result);
    assert!(text.contains("console output truncated at 16 KiB"));
    assert!(text.len() < 16 * 1024 + 200);
    let result = h.run("return 'x'.repeat(100000);").await;
    let text = output::text(&result);
    assert!(text.contains("Full output:"), "{text}");
    assert!(text.len() < 52 * 1024);
}

#[tokio::test]
async fn no_host_apis_await_tools_is_harmless_and_state_is_fresh() {
    let h = Harness::new();
    let result = h.run("await tools; return [typeof require, typeof std, typeof os, typeof setTimeout, typeof fetch, typeof process, typeof tools[Symbol.iterator]];").await;
    assert!(!result.is_error);
    assert_eq!(output::text(&result).matches("undefined").count(), 7);
    assert!(h.run("return import('std');").await.is_error);
    assert!(!h.run("globalThis.saved = 1;").await.is_error);
    assert!(output::text(&h.run("return typeof saved;").await).starts_with("undefined\n"));
}

#[tokio::test]
async fn details_roundtrip_and_unicode_argument_truncation() {
    let h = Harness::new();
    let result = h
        .run("return await tools.read({text:'中'.repeat(300),nested:{list:['文'.repeat(201)]}});")
        .await;
    let run = record(&result);
    assert_eq!(run.calls[0].id, "program.1");
    assert_eq!(
        run.calls[0].arguments["text"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        200
    );
    assert_eq!(
        run.calls[0].arguments["nested"]["list"][0]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        200
    );
    assert_eq!(
        serde_json::from_str::<ProgramRun>(&serde_json::to_string(&run).unwrap()).unwrap(),
        run
    );
    for state in [
        ProgramCallState::Running,
        ProgramCallState::Completed,
        ProgramCallState::Failed,
        ProgramCallState::Cancelled,
    ] {
        let value = serde_json::to_value(state).unwrap();
        assert_eq!(
            serde_json::from_value::<ProgramCallState>(value).unwrap(),
            state
        );
    }
}

#[tokio::test]
async fn invalid_json_arguments_are_counted_and_later_calls_keep_contiguous_ids() {
    let h = Harness::new();
    let result = h.run("let nested = {}; for(let i=0;i<150;i++) nested = {nested}; try { await tools.read(nested); } catch {} const cycle = {}; cycle.self = cycle; try { await tools.read(cycle); } catch {} return await tools.read({text:'alive'});").await;
    assert!(!result.is_error, "{result:?}");
    let calls = record(&result).calls;
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0].state, ProgramCallState::Failed);
    assert_eq!(calls[1].state, ProgramCallState::Failed);
    assert_eq!(calls[2].id, "program.3");
    assert_eq!(calls[2].state, ProgramCallState::Completed);
    assert!(output::text(&result).contains("1 succeeded, 2 failed"));
}

#[tokio::test]
async fn input_limits_are_checked_in_utf8_bytes() {
    let h = Harness::new();
    for args in [
        json!({"code":"中".repeat(22000)}),
        json!({"code":1}),
        json!({"code":"return 1", "timeout":0}),
        json!({"code":"return 1", "timeout":3601}),
    ] {
        assert!(
            h.tool
                .run(
                    ToolCall {
                        id: "invalid".into(),
                        name: "run_code".into(),
                        arguments: args,
                        opaque: None
                    },
                    h.ctx.clone()
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn fatal_resource_limits_cancel_pending_tools_and_user_cancel_interrupts_busy_js() {
    let h = Harness::new();
    for code in [
        "tools.read({delay:5000}); while(true){}",
        "tools.read({delay:5000}); try { new ArrayBuffer(70 * 1024 * 1024); } catch {}",
    ] {
        let result = h.run(code).await;
        assert!(result.is_error);
        assert_eq!(record(&result).calls[0].state, ProgramCallState::Cancelled);
        assert_eq!(h.active.load(Ordering::SeqCst), 0);
    }
    let mut h = Harness::new();
    h.tool.budget = JS_BUDGET;
    let cancel = async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        h.ctx.cancel.cancel();
    };
    let (result, ()) = tokio::join!(h.run("while(true){}"), cancel);
    assert!(result.is_error);
    assert!(output::text(&result).contains("Operation aborted"));
}
