use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tiechui_exec::StopReason;
use tiechui_model::{ToolCall, ToolSpec};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::output::{OutputAccumulator, STRUCTURED_MAX_BYTES, Snapshot};
use crate::{Concurrency, Tool, ToolContext, ToolError, ToolOutput, ToolUpdate};

pub struct BashTool {
    spec: ToolSpec,
}

impl Default for BashTool {
    fn default() -> Self {
        Self { spec: ToolSpec {
            name: "bash".into(),
            description: "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 2000 lines or 50KB (whichever is hit first). If truncated, full output is saved to a temp file. Optionally provide a timeout in seconds.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Shell command to execute"},
                    "timeout": {"type": "number", "description": "Timeout in seconds (optional, no default timeout)"},
                },
                "required": ["command"],
            }),
        } }
    }
}

#[derive(Deserialize)]
struct Input {
    command: String,
    timeout: Option<f64>,
}

struct Throttle {
    dirty: bool,
    last_update: Instant,
}

/// A settled bash run; `reason` and `code` are absent when no process settled.
pub struct Outcome {
    pub output: Result<ToolOutput, ToolError>,
    pub reason: Option<StopReason>,
    pub code: Option<i32>,
}

impl From<ToolError> for Outcome {
    fn from(error: ToolError) -> Self {
        Self {
            output: Err(error),
            reason: None,
            code: None,
        }
    }
}

pub fn sanitize_binary_output(text: &mut String) {
    text.retain(|character| !matches!(character, '\u{0}'..='\u{8}' | '\u{b}'..='\u{c}' | '\u{e}'..='\u{1f}' | '\u{fff9}'..='\u{fffb}'));
}

fn update(mut snapshot: Snapshot) -> ToolOutput {
    sanitize_binary_output(&mut snapshot.truncation.content);
    let mut details = serde_json::Map::new();
    if snapshot.truncation.truncated {
        details.insert("truncation".into(), json!(snapshot.truncation));
    }
    if let Some(path) = snapshot.full_output_path {
        details.insert("fullOutputPath".into(), json!(path));
    }
    let mut output = ToolOutput::text(snapshot.truncation.content);
    output.details = Some(Value::Object(details));
    output
}

fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, ToolError> {
    mutex
        .lock()
        .map_err(|_| ToolError("output lock poisoned".into()))
}

fn append_status(text: String, status: &str) -> String {
    if text.is_empty() {
        status.into()
    } else {
        format!("{text}\n\n{status}")
    }
}

/// Validates the tool's `timeout` seconds.
pub fn parse_timeout(seconds: Option<f64>) -> Result<Option<Duration>, ToolError> {
    match seconds {
        Some(seconds) if !seconds.is_finite() || seconds <= 0.0 => Err(ToolError(
            "Invalid timeout: must be a finite number of seconds".into(),
        )),
        Some(seconds) if seconds * 1000.0 > 2_147_483_647.0 => Err(ToolError(
            "Invalid timeout: maximum is 2147483.647 seconds".into(),
        )),
        Some(seconds) => Ok(Some(Duration::from_millis(
            (seconds * 1000.0).max(1.0) as u64
        ))),
        None => Ok(None),
    }
}

pub fn check_cwd(cwd: &Path) -> Result<(), ToolError> {
    if cwd.try_exists()? {
        Ok(())
    } else {
        Err(ToolError(format!(
            "Working directory does not exist: {}\nCannot execute bash commands.",
            cwd.display()
        )))
    }
}

fn timeout_text(seconds: f64) -> String {
    if seconds < 0.000001 {
        format!("{seconds:e}")
    } else {
        seconds.to_string()
    }
}

#[async_trait]
impl Tool for BashTool {
    fn programmatic(&self) -> bool {
        true
    }
    fn source(&self) -> Option<String> {
        Some("Built-in tools".into())
    }
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let input: Input =
            serde_json::from_value(call.arguments).map_err(|error| ToolError(error.to_string()))?;
        let emit = |output| {
            (ctx.updates)(ToolUpdate {
                call_id: call.id.clone(),
                epoch: ctx.epoch,
                output,
            })
        };
        emit(ToolOutput::default());
        parse_timeout(input.timeout)?;
        let output = Mutex::new(OutputAccumulator::new());
        run_bash(
            &input.command,
            input.timeout,
            &ctx.cwd,
            ctx.cancel.clone(),
            &output,
            || {},
            emit,
        )
        .await
        .output
    }
}

/// Runs a command under the bash tool's contract, streaming throttled snapshots to `emit`.
/// `timeout` must have passed [`parse_timeout`].
pub async fn run_bash(
    command: &str,
    timeout: Option<f64>,
    cwd: &Path,
    cancel: CancellationToken,
    output: &Mutex<OutputAccumulator>,
    on_spawn: impl FnOnce() + Send,
    emit: impl Fn(ToolOutput) + Sync,
) -> Outcome {
    let limit = match parse_timeout(timeout) {
        Ok(limit) => limit,
        Err(error) => return error.into(),
    };
    if cancel.is_cancelled() {
        return ToolError("Command aborted".into()).into();
    }
    if let Err(error) = check_cwd(cwd) {
        return error.into();
    }
    let started = Instant::now();
    let throttle = Mutex::new(Throttle {
        dirty: false,
        last_update: Instant::now() - Duration::from_millis(100),
    });
    let shell = match tiechui_exec::resolve_shell() {
        Ok(shell) => shell,
        Err(error) => return ToolError(error.to_string()).into(),
    };
    let execution = tiechui_exec::execute(&shell, command, cwd, limit, cancel, on_spawn, |data| {
        let next = {
            let mut throttle = throttle
                .lock()
                .map_err(|_| std::io::Error::other("output lock poisoned"))?;
            let mut output = output
                .lock()
                .map_err(|_| std::io::Error::other("output lock poisoned"))?;
            output.append(data)?;
            throttle.dirty = true;
            if throttle.last_update.elapsed() >= Duration::from_millis(100) {
                throttle.dirty = false;
                throttle.last_update = Instant::now();
                Some(update(output.snapshot()))
            } else {
                None
            }
        };
        if let Some(output) = next {
            emit(output);
        }
        Ok(())
    });
    tokio::pin!(execution);
    let process = loop {
        let deadline = match lock(&throttle) {
            Ok(throttle) if throttle.dirty => throttle.last_update + Duration::from_millis(100),
            Ok(_) => Instant::now() + Duration::from_millis(100),
            Err(error) => return error.into(),
        };
        tokio::select! {
            result = &mut execution => break result,
            _ = tokio::time::sleep_until(deadline) => {
                let next = {
                    let mut throttle = match lock(&throttle) {
                        Ok(throttle) => throttle,
                        Err(error) => return error.into(),
                    };
                    if throttle.dirty && throttle.last_update.elapsed() >= Duration::from_millis(100) {
                        throttle.last_update = Instant::now();
                        throttle.dirty = false;
                        match lock(output) {
                            Ok(output) => Some(update(output.snapshot())),
                            Err(error) => return error.into(),
                        }
                    } else { None }
                };
                if let Some(output) = next { emit(output); }
            }
        }
    };
    let finished = (|| {
        let dirty = lock(&throttle)?.dirty;
        let mut output = lock(output)?;
        output.finish()?;
        let mut snapshot = output.snapshot();
        sanitize_binary_output(&mut snapshot.truncation.content);
        let final_update = dirty.then(|| update(output.snapshot()));
        let structured = if process
            .as_ref()
            .is_ok_and(|result| result.reason == StopReason::Exited)
        {
            Some(output.full_output(STRUCTURED_MAX_BYTES)?)
        } else {
            None
        };
        Ok::<_, ToolError>((snapshot, final_update, structured))
    })();
    let (snapshot, final_update, structured) = match finished {
        Ok(finished) => finished,
        Err(error) => return error.into(),
    };
    if let Some(output) = final_update {
        emit(output);
    }
    let process = match process {
        Ok(process) => process,
        Err(error) => return ToolError::from(error).into(),
    };
    let settled = |output| Outcome {
        output,
        reason: Some(process.reason),
        code: process
            .exit_code
            .or_else(|| process.signal.map(|signal| 128 + signal)),
    };
    match process.reason {
        StopReason::Cancelled => {
            return settled(Err(ToolError(append_status(
                snapshot.text(""),
                "Command aborted",
            ))));
        }
        StopReason::TimedOut => {
            return settled(Err(ToolError(append_status(
                snapshot.text(""),
                &format!(
                    "Command timed out after {} seconds",
                    timeout_text(timeout.unwrap_or_default())
                ),
            ))));
        }
        StopReason::Exited => {}
    }
    let Some(code) = process
        .exit_code
        .or_else(|| process.signal.map(|signal| 128 + signal))
    else {
        return settled(Err(ToolError(append_status(
            snapshot.text("(no output)"),
            "Command terminated without an exit code",
        ))));
    };
    let text = snapshot.text("(no output)");
    let mut result = ToolOutput::text(if code == 0 {
        text
    } else {
        append_status(text, &format!("Command exited with code {code}"))
    });
    result.is_error = code != 0;
    result.details = snapshot.details();
    if let Some((mut output, truncated)) = structured {
        sanitize_binary_output(&mut output);
        let mut value = json!({
            "output": output, "truncated": truncated, "exit_code": code,
            "wall_time_seconds": (started.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        });
        if truncated {
            value["full_output_path"] = json!(snapshot.full_output_path);
        }
        result.structured_content = Some(value);
    }
    settled(Ok(result))
}
