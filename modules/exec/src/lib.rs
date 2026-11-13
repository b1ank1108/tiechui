//! Shell tools: `bash` with background tasks, their management tools and `/shells`.

mod shells;

use std::sync::Arc;

use serde::Deserialize;
use shells::{Progress, Read, Shells, Start, elapsed, summary};
use tiechui_plugin_api::{
    model::{ToolCall, ToolSpec},
    protocol::{CommandInfo, SHELLS_EVENT, ShellMode, ShellTask},
    serde_json::{Value, json},
    tools::{
        BashTool, ToolUpdate,
        bash::{check_cwd, parse_timeout},
    },
    *,
};

const CONTINUATION: &str = "tiechui.exec.completed";
const DEFAULT_READ: usize = 8 * 1024;
const MAX_READ: usize = 50 * 1024;
const MAX_WAIT_MS: u64 = 30_000;
/// How long a background start waits for early output or an immediate exit.
const STARTUP_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);

pub struct ExecModule {
    /// Session-owned background tasks; children keep the plain foreground tool.
    pub background: bool,
}

impl Module for ExecModule {
    fn id(&self) -> &'static str {
        "exec"
    }

    fn register(&self, registry: &mut Registry, config: &ModuleConfig) -> Result<(), ToolError> {
        let env = config.network.subprocess_env();
        if !self.background {
            return registry.register_tool(Arc::new(BashTool::with_env(env)));
        }
        let shells = Arc::new(Shells::with_env(env));
        registry.register_runtime(shells.clone());
        registry.register_background_work(shells.clone());
        registry.register_command(Arc::new(ShellsCommand(shells.clone())));
        registry.register_tool(Arc::new(Bash {
            shells: shells.clone(),
            spec: bash_spec(),
        }))?;
        for (kind, name, description, schema) in [
            (
                Kind::List,
                "shell_list",
                "List this session's shell tasks: running foreground and background commands and recent results.",
                json!({"type":"object","additionalProperties":false,"properties":{}}),
            ),
            (
                Kind::Output,
                "shell_output",
                "Read a shell task's output and state. Without cursor, returns the last max_bytes; pass the returned cursor to read only newer output. wait_ms waits for new output or completion. You are told when a background task finishes, so do not call this in a loop to wait for it; when you must wait, make one call with a long wait_ms.",
                json!({"type":"object","additionalProperties":false,"required":["task_id"],"properties":{
                    "task_id":{"type":"string"},
                    "cursor":{"type":"integer","minimum":0,"description":"Byte position returned by a previous read."},
                    "max_bytes":{"type":"integer","minimum":1,"maximum":MAX_READ,"description":"Default 8192."},
                    "wait_ms":{"type":"integer","minimum":0,"maximum":MAX_WAIT_MS,"description":"Default 0."}
                }}),
            ),
            (
                Kind::Stop,
                "shell_stop",
                "Stop a shell task and its process tree, then report its final state.",
                json!({"type":"object","additionalProperties":false,"required":["task_id"],"properties":{"task_id":{"type":"string"}}}),
            ),
        ] {
            registry.register_tool(Arc::new(Manage {
                shells: shells.clone(),
                kind,
                spec: ToolSpec {
                    name: name.into(),
                    description: description.into(),
                    input_schema: schema,
                },
            }))?;
        }
        Ok(())
    }
}

fn bash_spec() -> ToolSpec {
    let mut spec = BashTool::default().spec().clone();
    spec.description += " Set background to true for servers, watchers and other long-running commands you do not need to wait for: it returns a task ID with the first moment of output, or the full result if the command ends at once. You are told when a background task finishes, so do not poll shell_output for it; to wait for a command, run it in the foreground. Read newer output with shell_output and stop it with shell_stop. The timeout still applies. Background tasks end with the session.";
    spec.input_schema["properties"]["background"] = json!({
        "type": "boolean",
        "description": "Run in the background and return a task ID (default false)",
    });
    spec
}

#[derive(Deserialize)]
struct Input {
    command: String,
    timeout: Option<f64>,
    #[serde(default)]
    background: bool,
}

fn task_json(task: &ShellTask) -> Value {
    json!({
        "task_id": task.id,
        "command": task.command,
        "cwd": task.cwd,
        "background": task.mode == ShellMode::Background,
        "state": task.state,
        "exit_code": task.exit_code,
        "elapsed": elapsed(task),
    })
}

/// What the interface shows for a shell result. Unlike structured content, details stay with
/// the session, so a resumed transcript looks the same.
fn details(task: &ShellTask, new_output: Option<bool>) -> Value {
    let mut shell = task_json(task);
    if let Some(new_output) = new_output {
        shell["new_output"] = json!(new_output);
    }
    json!({ "shell": shell })
}

fn started(task: &ShellTask, text: String) -> ToolOutput {
    ToolOutput {
        structured_content: Some(json!({
            "task_id": task.id,
            "state": task.state,
            "background": true,
            "command": task.command,
        })),
        ..ToolOutput::text(text)
    }
}

/// Output read from a task, then a bracketed line with its state, cursor and any gap.
fn report(read: &Read, empty: &str, state: &str, hint: Option<&str>) -> ToolOutput {
    let mut notes = vec![format!(
        "{} · {state} · cursor {}",
        read.task.id, read.cursor
    )];
    if read.gap > 0 {
        notes.push(format!(
            "{} bytes before this text are no longer in memory",
            read.gap
        ));
    } else if read.earlier {
        notes.push("earlier output omitted".into());
    }
    if (read.gap > 0 || read.earlier)
        && let Some(path) = &read.log_path
    {
        notes.push(format!("log: {path}"));
    }
    notes.extend(hint.map(str::to_owned));
    let mut value = task_json(&read.task);
    value["output"] = json!(read.output);
    value["cursor"] = json!(read.cursor);
    value["gap_bytes"] = json!(read.gap);
    value["log_path"] = json!(read.log_path);
    ToolOutput {
        details: Some(details(&read.task, Some(!read.output.is_empty()))),
        structured_content: Some(value),
        ..ToolOutput::text(format!(
            "{}\n\n[{}]",
            if read.output.is_empty() {
                empty
            } else {
                &read.output
            },
            notes.join(" · ")
        ))
    }
}

struct Bash {
    shells: Arc<Shells>,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for Bash {
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
        (ctx.updates)(ToolUpdate {
            call_id: call.id.clone(),
            epoch: ctx.epoch,
            output: ToolOutput::default(),
        });
        parse_timeout(input.timeout)?;
        if ctx.cancel.is_cancelled() {
            return Err(ToolError("Command aborted".into()));
        }
        check_cwd(&ctx.cwd)?;
        let mut changes = self.shells.subscribe();
        let id = self.shells.start(Start {
            command: input.command,
            timeout: input.timeout,
            cwd: ctx.cwd.clone(),
            background: input.background,
            ctx: ctx.clone(),
            call_id: call.id,
        })?;
        // A dropped waiter must not leave its foreground process running.
        let _guard = Interrupt(&self.shells, &id);
        let mut interrupted = false;
        let now = tokio::time::Instant::now();
        let forever = now + std::time::Duration::from_secs(86_400 * 365);
        let window = now + STARTUP_WINDOW;
        loop {
            match self.shells.progress(&id)? {
                Progress::Done(result) => {
                    // The model reads this result, so the completion needs no notification.
                    self.shells.seen(&id);
                    return result;
                }
                Progress::Background => {
                    let task = self.shells.task(&id)?;
                    if !matches!(task.state, protocol::ShellState::Starting) {
                        if !input.background {
                            return Ok(started(
                                &task,
                                format!(
                                    "The user moved this command to the background as {id}; it is still running. You will be told when it finishes; read output with shell_output and stop it with shell_stop."
                                ),
                            ));
                        }
                        if interrupted || tokio::time::Instant::now() >= window {
                            let read = self.shells.read(&id, Some(0), DEFAULT_READ)?;
                            return Ok(report(
                                &read,
                                "(no output yet)",
                                "started in the background",
                                Some("you will be told when it finishes"),
                            ));
                        }
                    }
                }
                Progress::Starting | Progress::Running => {}
            }
            let cancel = if interrupted {
                CancellationToken::new()
            } else {
                ctx.cancel.clone()
            };
            let deadline = if input.background && tokio::time::Instant::now() < window {
                window
            } else {
                forever
            };
            if !Shells::wait(&mut changes, deadline, &cancel).await && ctx.cancel.is_cancelled() {
                interrupted = true;
                self.shells.interrupt(&id);
            }
        }
    }
}

struct Interrupt<'a>(&'a Shells, &'a str);

impl Drop for Interrupt<'_> {
    fn drop(&mut self) {
        self.0.interrupt(self.1);
    }
}

enum Kind {
    List,
    Output,
    Stop,
}

struct Manage {
    shells: Arc<Shells>,
    kind: Kind,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for Manage {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn source(&self) -> Option<String> {
        Some("Built-in tools".into())
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Shared
    }

    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        if self.shells.session()? != ctx.scope.thread_id.0 {
            return Err(ToolError("Shell tasks belong to another session".into()));
        }
        let arguments = &call.arguments;
        let id = arguments["task_id"].as_str().unwrap_or_default();
        let number = |key: &str| arguments[key].as_u64();
        match self.kind {
            Kind::List => {
                let tasks = self.shells.tasks()?;
                let text = if tasks.is_empty() {
                    "No shell tasks in this session.".into()
                } else {
                    tasks.iter().map(line).collect::<Vec<_>>().join("\n")
                };
                Ok(ToolOutput {
                    structured_content: Some(
                        json!({"tasks": tasks.iter().map(task_json).collect::<Vec<_>>()}),
                    ),
                    ..ToolOutput::text(text)
                })
            }
            Kind::Output => {
                let cursor = number("cursor").map(|c| c as usize);
                let max =
                    number("max_bytes").map_or(DEFAULT_READ, |m| (m as usize).clamp(1, MAX_READ));
                let wait = std::time::Duration::from_millis(
                    number("wait_ms").unwrap_or(0).min(MAX_WAIT_MS),
                );
                let deadline = tokio::time::Instant::now() + wait;
                let mut changes = self.shells.subscribe();
                let read = loop {
                    let read = self.shells.read(id, cursor, max)?;
                    if !read.output.is_empty()
                        || !read.task.state.active()
                        || !Shells::wait(&mut changes, deadline, &ctx.cancel).await
                    {
                        break read;
                    }
                };
                if !read.task.state.active() {
                    self.shells.seen(id);
                }
                let state = if read.task.state.active() {
                    read.task.state.label().to_owned()
                } else {
                    summary(&read.task)
                };
                Ok(report(&read, "(no new output)", &state, None))
            }
            Kind::Stop => {
                let mut changes = self.shells.subscribe();
                self.shells.stop(id)?;
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
                while self.shells.task(id)?.state.active()
                    && Shells::wait(&mut changes, deadline, &ctx.cancel).await
                {}
                let task = self.shells.task(id)?;
                if !task.state.active() {
                    self.shells.seen(id);
                }
                Ok(ToolOutput {
                    details: Some(details(&task, None)),
                    structured_content: Some(task_json(&task)),
                    ..ToolOutput::text(if task.state.active() {
                        format!("{id} is stopping.")
                    } else {
                        format!("{id} {}.", summary(&task))
                    })
                })
            }
        }
    }
}

/// `sh1  background  running  01:42  npm run dev`
fn line(task: &ShellTask) -> String {
    format!(
        "{}  {}  {}  {}  {}",
        task.id,
        task.mode.label(),
        task.exit_code.map_or_else(
            || task.state.label().to_owned(),
            |code| format!("exit {code}")
        ),
        elapsed(task),
        task.command.lines().next().unwrap_or_default()
    )
}

impl RuntimeExtension for Shells {
    fn event(&self, event: &RuntimeEvent, branch: &BranchView) -> Result<RuntimeOutput, ToolError> {
        match event {
            RuntimeEvent::Restore { .. } => self.restore(&branch.session_id),
            RuntimeEvent::Fork { .. } | RuntimeEvent::BeforeNavigate | RuntimeEvent::Shutdown => {
                self.stop_all()
            }
            RuntimeEvent::Input {
                continuation: Some(continuation),
                ..
            } if continuation.kind == CONTINUATION => self.delivered(&continuation.identity),
            _ => {}
        }
        Ok(RuntimeOutput {
            events: self.events(),
            ..Default::default()
        })
    }

    fn continuation(&self) -> Option<Continuation> {
        Shells::continuation(self)
    }

    fn accepts(&self, next: &Continuation) -> bool {
        Shells::continuation(self).is_some_and(|current| {
            current.generation == next.generation && current.identity == next.identity
        })
    }
}

#[async_trait]
impl BackgroundWork for Shells {
    async fn quiesce(&self) {
        Shells::quiesce(self).await;
    }
}

struct ShellsCommand(Arc<Shells>);

const USAGE: &str = "Usage: /shells [output|background|stop <id>]";

impl ShellsCommand {
    fn text(&self, arguments: &str) -> Result<String, ToolError> {
        let shells = &self.0;
        let mut words = arguments.split_whitespace();
        let (action, id) = (words.next(), words.next());
        if words.next().is_some() {
            return Err(ToolError(USAGE.into()));
        }
        match (action, id) {
            (None, _) => {
                let tasks = shells.tasks()?;
                Ok(if tasks.is_empty() {
                    "No shell tasks in this session.".into()
                } else {
                    tasks.iter().map(line).collect::<Vec<_>>().join("\n")
                })
            }
            (Some("output"), Some(id)) => {
                let task = shells.task(id)?;
                let detail = shells.view(false, Some(id))?.detail;
                let output = detail.map(|d| d.output).unwrap_or_default();
                Ok(format!(
                    "{}\ncwd: {}\n{}\n\n{}",
                    line(&task),
                    task.cwd,
                    task.command,
                    if output.is_empty() {
                        "(no output)"
                    } else {
                        output.trim_end()
                    }
                ))
            }
            (Some("background"), Some(id)) => {
                shells.background(id)?;
                Ok(format!("{id} moved to the background."))
            }
            (Some("stop"), Some(id)) => Ok(match shells.stop(id)? {
                state if state.active() => format!("Stopping {id}."),
                _ => format!("{id} already {}.", summary(&shells.task(id)?)),
            }),
            _ => Err(ToolError(USAGE.into())),
        }
    }

    async fn emit(
        &self,
        session: &CommandSession,
        view: protocol::ShellsView,
    ) -> Result<(), ToolError> {
        session
            .host
            .apply(SessionEffect::Emit {
                kind: SHELLS_EVENT.into(),
                data: json!(view),
            })
            .await
    }
}

#[async_trait]
impl Command for ShellsCommand {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "shells".into(),
            description: "View shell tasks; move a running command to the background or stop it"
                .into(),
            argument_hint: Some("[output|background|stop <id>]".into()),
        }
    }
    fn allows_busy(&self, _: &str) -> bool {
        true
    }
    fn background(&self) -> bool {
        true
    }
    async fn run(&self, arguments: &str, _: CancellationToken) -> Result<String, ToolError> {
        self.text(arguments)
    }
    async fn session_run(
        &self,
        arguments: &str,
        session: CommandSession,
        _: CancellationToken,
    ) -> Result<String, ToolError> {
        if !arguments.trim().is_empty() {
            return self.text(arguments);
        }
        self.emit(&session, self.0.view(true, None)?).await?;
        Ok(String::new())
    }
    async fn input(
        &self,
        data: Value,
        session: CommandSession,
        _: CancellationToken,
    ) -> Result<String, ToolError> {
        let shells = &self.0;
        if shells.session()? != session.branch.session_id {
            return Ok(String::new());
        }
        let id = |key: &str| data[key].as_str();
        let mut detail = None;
        if let Some(target) = id("detail") {
            shells.watch(Some(target))?;
            detail = Some(target);
        } else if data["close"].as_bool() == Some(true) {
            shells.watch(None)?;
        } else if let Some(target) = id("stop") {
            shells.stop(target)?;
        } else if let Some(target) = id("background") {
            shells.background(target)?;
        }
        self.emit(&session, shells.view(false, detail)?).await?;
        Ok(String::new())
    }
}

#[cfg(test)]
mod tests;
