//! JavaScript orchestration through the host's tool invocation path.

mod engine;
mod heap;
mod output;

use std::sync::Arc;
use std::time::{Duration, Instant};
use tiechui_plugin_api::{
    Concurrency, Module, ModuleConfig, Registry, Tool, ToolContext, ToolError, ToolOutput,
    async_trait,
    futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered},
    model::{ToolCall, ToolSpec},
    protocol::{ProgramCall, ProgramCallState, ProgramRun},
    serde_json::{Value, json},
    tools::ToolUpdate,
};
use tokio::sync::{Semaphore, mpsc};

const DESCRIPTION: &str = include_str!("description.txt");
const JS_BUDGET: Duration = Duration::from_secs(30);
const MAX_CALLS: usize = 200;
const MAX_RESULT: usize = 1024 * 1024;

pub struct PtcModule;

impl Module for PtcModule {
    fn id(&self) -> &str {
        "ptc"
    }
    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        registry.register_tool(Arc::new(RunCode {
            spec: ToolSpec {
                name: "run_code".into(),
                description: DESCRIPTION.trim_end().into(),
                input_schema: json!({
                    "type":"object", "properties": {
                        "code":{"type":"string", "maxLength":65536, "description":"Async JavaScript function body, at most 64 KiB UTF-8."},
                        "description":{"type":"string", "description":"One sentence describing the program."},
                        "timeout":{"type":"number", "minimum":1, "maximum":3600, "description":"Optional wall-clock limit in seconds."}
                    }, "required":["code"], "additionalProperties":false
                }),
            },
            budget: JS_BUDGET,
        }))
    }
}

struct RunCode {
    spec: ToolSpec,
    budget: Duration,
}

enum Event {
    Call {
        index: usize,
        name: String,
        arguments: Value,
        error: Option<String>,
    },
    Finished(engine::Result),
}

struct Reply {
    index: usize,
    name: String,
    text: String,
    error: bool,
}

#[async_trait]
impl Tool for RunCode {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Delegated
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let code = call.arguments["code"]
            .as_str()
            .filter(|code| code.len() <= 64 * 1024)
            .ok_or_else(|| ToolError("code must be a string of at most 64 KiB UTF-8".into()))?
            .to_owned();
        let timeout = call
            .arguments
            .get("timeout")
            .map(|value| {
                value
                    .as_f64()
                    .filter(|s| s.is_finite() && (1.0..=3600.0).contains(s))
                    .map(Duration::from_secs_f64)
                    .ok_or_else(|| ToolError("timeout must be between 1 and 3600 seconds".into()))
            })
            .transpose()?;
        let invoker = ctx
            .invoker
            .clone()
            .ok_or_else(|| ToolError("Tool invocation is unavailable.".into()))?;
        let cancel = ctx.cancel.child_token();
        let _guard = cancel.clone().drop_guard();
        let deadline = timeout.map(|duration| Instant::now() + duration);
        let (events, mut inbox) = mpsc::unbounded_channel();
        let (replies, responses) = std::sync::mpsc::channel();
        let engine_cancel = cancel.clone();
        let budget = self.budget;
        let thread = std::thread::Builder::new()
            .name("run_code".into())
            .stack_size(8 * 1024 * 1024)
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    engine::run(
                        code,
                        events.clone(),
                        responses,
                        engine_cancel,
                        deadline,
                        budget,
                    )
                }))
                .unwrap_or_else(|_| engine::Result::error("JavaScript engine panicked"));
                let _ = events.send(Event::Finished(result));
            })?;
        let permits = Arc::new(Semaphore::new(4));
        let mut children: FuturesUnordered<BoxFuture<'static, (usize, ProgramCallState, Reply)>> =
            FuturesUnordered::new();
        let mut record = ProgramRun::default();
        let mut finished = None;
        let mut abandoned = 0;
        let mut dirty = false;
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let deadline_wait = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(deadline_wait);
        loop {
            tokio::select! {
                biased;
                _ = ctx.cancel.cancelled(), if !cancel.is_cancelled() => cancel.cancel(),
                _ = &mut deadline_wait, if !cancel.is_cancelled() => cancel.cancel(),
                Some((index, state, reply)) = children.next(), if !children.is_empty() => {
                    let child: &mut ProgramCall = &mut record.calls[index - 1];
                    child.state = state;
                    child.error = reply.error.then(|| reply.text.clone());
                    let _ = replies.send(reply);
                    dirty = true;
                }
                event = inbox.recv(), if finished.is_none() => {
                    match event {
                        Some(Event::Call { index, name, arguments, error }) => {
                            record.calls.push(ProgramCall {
                                id: format!("{}.{index}", call.id), name: name.clone(),
                                arguments: output::arguments(arguments.clone()),
                                state: ProgramCallState::Running, error: None,
                            });
                            let mut child_ctx = ctx.clone();
                            child_ctx.cancel = cancel.child_token();
                            // Child output stays inside the program; progress is the call's state.
                            child_ctx.updates = Arc::new(|_| {});
                            let child = ToolCall {
                                id: format!("{}.{index}", call.id), name, arguments, opaque: None,
                            };
                            children.push(Box::pin(invoke(index, child, child_ctx, invoker.clone(), permits.clone(), error)));
                            dirty = true;
                        }
                        Some(Event::Finished(result)) => {
                            abandoned = record.calls.iter().filter(|call| call.state == ProgramCallState::Running).count();
                            finished = Some(result);
                            cancel.cancel();
                        }
                        None => {
                            finished = Some(engine::Result::error("JavaScript thread disconnected"));
                            cancel.cancel();
                        }
                    }
                }
                _ = tick.tick() => {
                    if dirty {
                        (ctx.updates)(ToolUpdate {
                            call_id: call.id.clone(), epoch: ctx.epoch,
                            output: ToolOutput { details: Some(json!(record)), ..Default::default() },
                        });
                        dirty = false;
                    }
                }
            }
            if finished.is_some() && children.is_empty() {
                break;
            }
        }
        thread
            .join()
            .map_err(|_| ToolError("JavaScript thread panicked".into()))?;
        output::finish(
            finished.ok_or_else(|| ToolError("Missing program result".into()))?,
            record,
            abandoned,
        )
    }
}

async fn invoke(
    index: usize,
    call: ToolCall,
    ctx: ToolContext,
    invoker: Arc<dyn tiechui_plugin_api::ToolInvoker>,
    permits: Arc<Semaphore>,
    error: Option<String>,
) -> (usize, ProgramCallState, Reply) {
    let result = async {
        if let Some(error) = error { return Err(error); }
        let _permit = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err("Operation aborted".into()),
            permit = permits.acquire() => permit.map_err(|_| "Tool queue closed".to_owned())?,
        };
        let tool = invoker.get(&call.name).ok_or_else(|| format!("Unknown or unavailable tool {}", call.name))?;
        if !tool.programmatic() {
            return Err(format!("{} cannot be called from run_code", call.name));
        }
        let output = invoker.invoke(call.clone(), ctx.clone()).await;
        let text = output::text(&output);
        if text.len() > MAX_RESULT {
            return Err(format!("{} has already executed, but its result exceeds 1 MiB and cannot be passed to JavaScript", call.name));
        }
        if output.is_error { Err(text) } else { Ok(text) }
    }.await;
    let state = match &result {
        Ok(_) => ProgramCallState::Completed,
        Err(_) if ctx.cancel.is_cancelled() => ProgramCallState::Cancelled,
        Err(_) => ProgramCallState::Failed,
    };
    let reply = Reply {
        index,
        name: call.name,
        error: result.is_err(),
        text: result.unwrap_or_else(|text| text),
    };
    (index, state, reply)
}

#[cfg(test)]
mod tests;
