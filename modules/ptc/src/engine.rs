use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use rquickjs::{
    Coerced, Context, Ctx, Exception, FromJs, Function, Object, Promise, Runtime, Value,
};
use tiechui_plugin_api::{CancellationToken, serde_json};
use tokio::sync::mpsc::UnboundedSender;

use super::{Event, MAX_CALLS, Reply};

pub(super) struct Result {
    pub value: std::result::Result<Option<String>, String>,
    pub console: String,
}
impl Result {
    pub fn error(message: &str) -> Self {
        Self {
            value: Err(format!("Error: {message}")),
            console: String::new(),
        }
    }
}

struct Budget {
    exhausted: Rc<Cell<bool>>,
    cancel: CancellationToken,
    deadline: Option<Instant>,
    limit: Duration,
    spent: Cell<Duration>,
    active: Cell<Option<Instant>>,
}
impl Budget {
    fn reason(&self) -> Option<&'static str> {
        if self.exhausted.get() {
            Some("JavaScript heap limit exceeded (64 MiB)")
        } else if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            Some("Wall-clock timeout exceeded")
        } else if self.cancel.is_cancelled() {
            Some("Operation aborted")
        } else if self.spent.get()
            + self
                .active
                .get()
                .map_or(Duration::ZERO, |start| start.elapsed())
            >= self.limit
        {
            Some("JavaScript execution time budget exceeded")
        } else {
            None
        }
    }
    fn pause(&self) {
        if let Some(start) = self.active.take() {
            self.spent.set(self.spent.get() + start.elapsed());
        }
    }
    fn resume(&self) {
        self.active.set(Some(Instant::now()));
    }
}

pub(super) fn run(
    code: String,
    events: UnboundedSender<Event>,
    replies: Receiver<Reply>,
    cancel: CancellationToken,
    deadline: Option<Instant>,
    limit: Duration,
) -> Result {
    let console = Rc::new(RefCell::new(String::new()));
    let truncated = Rc::new(Cell::new(false));
    let exhausted = Rc::new(Cell::new(false));
    let budget = Rc::new(Budget {
        exhausted: exhausted.clone(),
        cancel,
        deadline,
        limit,
        spent: Cell::new(Duration::ZERO),
        active: Cell::new(Some(Instant::now())),
    });
    let value = (|| {
        // Observe allocation failures even when JavaScript catches the resulting exception.
        let runtime = Runtime::new_with_alloc(super::heap::Heap::new(exhausted))
            .map_err(|e| format!("Error: {e}"))?;
        runtime.set_max_stack_size(1024 * 1024);
        let interrupt = budget.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || interrupt.reason().is_some())));
        let context = Context::full(&runtime).map_err(|e| format!("Error: {e}"))?;
        context.with(|ctx| {
            let execute = || -> rquickjs::Result<Option<String>> {
                let counter = Cell::new(0);
                let call_budget = budget.clone();
                let send = Function::new(
                    ctx.clone(),
                    move |ctx: Ctx<'_>, name: String, args: String, failure: String| {
                        if let Some(reason) = call_budget.reason() {
                            return Err(Exception::throw_message(&ctx, reason));
                        }
                        let index = counter.get() + 1;
                        if index > MAX_CALLS {
                            return Err(Exception::throw_message(
                                &ctx,
                                "Tool call limit exceeded (200 calls)",
                            ));
                        }
                        counter.set(index);
                        let (arguments, error) = match serde_json::from_str(&args) {
                            Ok(arguments) => (arguments, (!failure.is_empty()).then_some(failure)),
                            Err(error) => (
                                serde_json::Value::Null,
                                Some(format!("Invalid tool arguments: {error}")),
                            ),
                        };
                        events
                            .send(Event::Call {
                                index,
                                name,
                                arguments,
                                error,
                            })
                            .map_err(|_| {
                                Exception::throw_message(&ctx, "Tool invocation stopped")
                            })?;
                        Ok(index as u32)
                    },
                )?;
                let captured = console.clone();
                let overflow = truncated.clone();
                let log = Function::new(ctx.clone(), move |text: String| {
                    let mut captured = captured.borrow_mut();
                    let remaining = (16 * 1024_usize).saturating_sub(captured.len());
                    let line = text + "\n";
                    let end = line.floor_char_boundary(remaining.min(line.len()));
                    captured.push_str(&line[..end]);
                    overflow.set(overflow.get() || end < line.len());
                })?;
                let factory: Function = ctx.eval(include_str!("bridge.js"))?;
                let bridge: Object = factory.call((send, log))?;
                ctx.globals()
                    .set("tools", bridge.get::<_, Object>("tools")?)?;
                ctx.globals()
                    .set("console", bridge.get::<_, Object>("console")?)?;
                let deliver: Function = bridge.get("deliver")?;
                let serialize: Function = bridge.get("serialize")?;
                let mut options = rquickjs::context::EvalOptions::default();
                options.filename = Some("run_code.js".into());
                let promise: Promise =
                    ctx.eval_with_options(format!("(async function(){{{code}\n}})()"), options)?;
                loop {
                    if let Some(reason) = budget.reason() {
                        return Err(Exception::throw_message(&ctx, reason));
                    }
                    if let Some(result) = promise.result::<Value>() {
                        let value = serialize.call((result?,))?;
                        if let Some(reason) = budget.reason() {
                            return Err(Exception::throw_message(&ctx, reason));
                        }
                        return Ok(value);
                    }
                    while let Ok(reply) = replies.try_recv() {
                        deliver.call::<_, ()>((
                            reply.index as u32,
                            reply.name,
                            reply.text,
                            reply.error,
                        ))?;
                    }
                    if ctx.execute_pending_job() {
                        continue;
                    }
                    budget.pause();
                    let reply = replies.recv_timeout(Duration::from_millis(10));
                    budget.resume();
                    if let Ok(reply) = reply {
                        deliver.call::<_, ()>((
                            reply.index as u32,
                            reply.name,
                            reply.text,
                            reply.error,
                        ))?;
                    }
                }
            };
            execute().map_err(|error| {
                if let Some(reason) = budget.reason() {
                    return format!("Error: {reason}");
                }
                if error.is_exception() {
                    let caught = ctx.catch();
                    if let Some(exception) = caught.as_exception() {
                        let message = exception
                            .message()
                            .unwrap_or_else(|| "JavaScript exception".into());
                        let stack = exception.stack().unwrap_or_default();
                        format!("Error: {message}\n{stack}").trim_end().to_owned()
                    } else {
                        let message = Coerced::<String>::from_js(&ctx, caught)
                            .map(|s| s.0)
                            .unwrap_or_else(|_| "JavaScript exception".into());
                        format!("Error: {message}")
                    }
                } else {
                    format!("Error: {error}")
                }
            })
        })
    })();
    let mut console = console.borrow().trim_end_matches('\n').to_owned();
    if truncated.get() {
        console.push_str("\n[console output truncated at 16 KiB]");
    }
    Result { value, console }
}
