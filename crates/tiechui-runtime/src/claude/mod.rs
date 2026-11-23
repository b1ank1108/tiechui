//! The driver chooses the session id (`--session-id`): Claude's `system/init` only comes after
//! the first user message. `--resume` keeps the id.

mod catalogue;
mod items;
mod run;
mod translate;
mod wire;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tiechui_protocol::{
    ApprovalDecision, InputContent, ModelInfo, PERMISSIONS_COMMAND, RuntimeCapabilities,
    SteeringMode, ThinkingLevel,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::backend::{Backend, BackendEvent, CommandAction, StartParams, Started, Steered};
use crate::process::{self, Process, StderrTail, overridable, seconds};
use crate::registry::{Driver, Launch, Probe, probe_at_least, resolve_in_path};
use catalogue::SharedCatalogue;
use run::{Handshake, Shared, SupervisorCommand, Writer};
use translate::{CountedMessageIds, Translator};

/// The oldest Claude Code the driver supports: 2.1.322, the only version its frames were
/// recorded from. The driver relies on `--session-id` and `--resume` in stream-json mode,
/// `--permission-prompt-tool stdio`, `--thinking-display`, `--effort`, the `initialize`,
/// `interrupt`, `set_model`, `apply_flag_settings` and `set_max_thinking_tokens` control
/// requests, `system/commands_changed` and the per-message `stream_event` usage; a lower bound
/// needs recordings of an older version.
pub const MIN_VERSION: &str = "2.1.322";

/// The effort Claude runs when it is given none (`get_settings` on 2.1.322).
const CLAUDE_DEFAULT_EFFORT: ThinkingLevel = ThinkingLevel::High;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
const ERROR_RESULT_EXIT_GRACE: Duration = Duration::from_secs(9);

const HANDSHAKE_TIMEOUT_ENV: &str = "CHUI_CLAUDE_HANDSHAKE_TIMEOUT_MS";
const REAP_TIMEOUT_ENV: &str = "CHUI_CLAUDE_REAP_TIMEOUT_MS";

#[derive(Debug, Clone, Copy)]
struct Timeouts {
    handshake: Duration,
    reap: Duration,
}

impl Timeouts {
    fn current() -> Self {
        Self {
            handshake: overridable(HANDSHAKE_TIMEOUT, HANDSHAKE_TIMEOUT_ENV),
            reap: overridable(ERROR_RESULT_EXIT_GRACE, REAP_TIMEOUT_ENV),
        }
    }
}

fn debug(message: &str) {
    process::debug("claude", message);
}

pub struct ClaudeDriver;

#[async_trait]
impl Driver for ClaudeDriver {
    fn id(&self) -> &'static str {
        "claude"
    }

    fn label(&self) -> &'static str {
        "Claude Code"
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            // A message written while a turn runs joins that turn (recordings of 2.1.322).
            steering: SteeringMode::Inject,
            model: true,
            thinking: true,
            commands: true,
            permissions: true,
            ..RuntimeCapabilities::minimal()
        }
    }

    fn gated_ops(&self) -> &'static [&'static str] {
        &[
            "set_model",
            "set_thinking",
            "run_command",
            "command_input",
            "permissions",
        ]
    }

    async fn probe(&self, launch: &Launch) -> Probe {
        probe_at_least("claude", MIN_VERSION, launch).await
    }

    fn backend(&self, launch: &Launch) -> Result<Box<dyn Backend>, String> {
        Ok(Box::new(ClaudeBackend {
            launch: launch.clone(),
            timeouts: Timeouts::current(),
            params: None,
            session_id: String::new(),
            events: None,
            counted: CountedMessageIds::default(),
            catalogue: SharedCatalogue::default(),
            thinking: Thinking::default(),
            run: None,
            requests: 0,
        }))
    }
}

struct Run {
    writer: Writer,
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl Run {
    async fn close(mut self) {
        let (ack, done) = oneshot::channel();
        if self
            .shared
            .supervisor
            .send(SupervisorCommand::Close(ack))
            .is_ok()
        {
            let _ = done.await;
        }
        self.abort();
    }

    fn abort(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.abort();
    }
}

enum SpawnFailure {
    ResumeNotFound,
    Other(String),
}

impl From<SpawnFailure> for String {
    fn from(failure: SpawnFailure) -> Self {
        match failure {
            SpawnFailure::ResumeNotFound => "claude has no such conversation".into(),
            SpawnFailure::Other(error) => error,
        }
    }
}

struct OpenedRun {
    run: Run,
    resumed: bool,
}

/// Why Claude did not do what a control request asked.
struct Refusal {
    message: String,
    code: Option<String>,
}

impl From<String> for Refusal {
    fn from(message: String) -> Self {
        Self {
            message,
            code: None,
        }
    }
}

/// `--thinking disabled` cannot be undone while Claude runs, a thinking-token cap can: `off` is a
/// zero cap, sent after `initialize`, and lifting the cap brings back the effort Claude runs.
struct Thinking {
    /// The level the session asked for; a model without it runs the nearest one.
    requested: ThinkingLevel,
    /// The level Claude runs.
    running: ThinkingLevel,
    /// Whether the driver set `running`, or it is Claude's default.
    chosen: bool,
}

impl Default for Thinking {
    fn default() -> Self {
        Self {
            requested: CLAUDE_DEFAULT_EFFORT,
            running: CLAUDE_DEFAULT_EFFORT,
            chosen: false,
        }
    }
}

struct ClaudeBackend {
    launch: Launch,
    timeouts: Timeouts,
    params: Option<StartParams>,
    session_id: String,
    events: Option<mpsc::Sender<BackendEvent>>,
    counted: CountedMessageIds,
    catalogue: SharedCatalogue,
    thinking: Thinking,
    run: Option<Run>,
    requests: u64,
}

impl ClaudeBackend {
    fn args(&self, params: &StartParams, id: &str, resume: bool) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            "--permission-prompt-tool",
            "stdio",
            "--thinking-display",
            "summarized",
            "--permission-mode",
            items::permission_mode(params.mode.as_deref()),
            if resume { "--resume" } else { "--session-id" },
            id,
        ]
        .map(str::to_owned)
        .into();
        if let Some(model) = &params.model {
            args.extend(["--model".to_owned(), model.clone()]);
        }
        if self.thinking.chosen
            && let Some(effort) = items::effort(self.thinking.running)
        {
            args.extend(["--effort".to_owned(), effort.to_owned()]);
        }
        args
    }

    async fn spawn_run(&mut self, id: &str, resume: bool) -> Result<Run, SpawnFailure> {
        let run = self.spawn_process(id, resume).await?;
        let run = self.handshake(run, resume).await?;
        if !self.thinking.chosen || self.thinking.running != ThinkingLevel::Off {
            return Ok(run);
        }
        let id = self.next_request_id();
        match self.ask(&run, id, thinking_cap(Some(0))).await {
            Ok(_) => Ok(run),
            Err(refusal) => {
                run.close().await;
                Err(SpawnFailure::Other(format!(
                    "claude did not turn thinking off: {}",
                    refusal.message
                )))
            }
        }
    }

    async fn spawn_process(&self, id: &str, resume: bool) -> Result<Run, SpawnFailure> {
        let (Some(params), Some(events)) = (&self.params, &self.events) else {
            return Err(SpawnFailure::Other("claude is not started".into()));
        };
        let binary = resolve_in_path("claude", self.launch.path.as_deref())
            .ok_or_else(|| SpawnFailure::Other("claude is not on PATH".into()))?;
        let args = self.args(params, id, resume);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut process = Process::spawn_filtered(
            &binary,
            &args,
            &params.cwd,
            self.launch.path.as_ref(),
            items::is_host_variable,
        )
        .map_err(|error| {
            SpawnFailure::Other(format!("cannot start {}: {error}", binary.display()))
        })?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (
            process.stdin.take(),
            process.stdout.take(),
            process.stderr.take(),
        ) else {
            return Err(SpawnFailure::Other(
                "claude started without its stdio".into(),
            ));
        };
        let (shared, commands, answered) = Shared::new("tiechui-init".into());
        let (writer, write_task) = Writer::spawn(stdin, shared.clone());
        let tail = StderrTail::default();
        let (frames, inbound) = mpsc::channel(256);
        let translator = Translator::new(
            shared.clone(),
            writer.clone(),
            events.clone(),
            self.counted.clone(),
            self.catalogue.clone(),
            self.timeouts.reap,
            tail.clone(),
        );
        let tasks = vec![
            write_task,
            tokio::spawn(process::read_frames("claude", stdout, frames)),
            tokio::spawn(process::read_stderr("claude", stderr, tail)),
            tokio::spawn(translator.run(inbound, answered)),
            tokio::spawn(run::supervise(process, commands, shared.clone())),
        ];
        Ok(Run {
            writer,
            shared,
            tasks,
        })
    }

    async fn handshake(&self, run: Run, resume: bool) -> Result<Run, SpawnFailure> {
        let mut state = run.shared.handshake.subscribe();
        let sent = run
            .writer
            .send(&wire::initialize_request(&run.shared.initialize_request_id));
        let outcome = match sent {
            Ok(()) => tokio::time::timeout(
                self.timeouts.handshake,
                state.wait_for(|state| *state != Handshake::Pending),
            )
            .await
            .map(|result| result.map(|state| state.clone())),
            Err(error) => Ok(Ok(Handshake::Failed(error))),
        };
        let failure = match outcome {
            Ok(Ok(Handshake::Ready)) => return Ok(run),
            Ok(Ok(Handshake::Failed(reason)))
                if resume && reason.contains("No conversation found") =>
            {
                SpawnFailure::ResumeNotFound
            }
            Ok(Ok(Handshake::Failed(reason))) => SpawnFailure::Other(reason),
            Ok(Ok(Handshake::Pending) | Err(_)) => SpawnFailure::Other("claude exited".into()),
            Err(_) => SpawnFailure::Other(format!(
                "claude timed out: no answer to initialize within {}",
                seconds(self.timeouts.handshake)
            )),
        };
        run.close().await;
        Err(failure)
    }

    async fn open(&mut self, resume: Option<&str>) -> Result<OpenedRun, String> {
        if let Some(id) = resume {
            match self.spawn_run(id, true).await {
                Ok(run) => {
                    self.session_id = id.to_owned();
                    return Ok(OpenedRun { run, resumed: true });
                }
                Err(SpawnFailure::ResumeNotFound) => debug("--resume found no conversation"),
                Err(SpawnFailure::Other(error)) => return Err(error),
            }
        }
        self.session_id = items::new_session_id();
        let id = self.session_id.clone();
        let run = self.spawn_run(&id, false).await?;
        Ok(OpenedRun {
            run,
            resumed: false,
        })
    }

    async fn respawn_resuming(&mut self) -> Result<(), String> {
        if let Some(old) = self.run.take() {
            old.close().await;
        }
        let id = self.session_id.clone();
        let run = match self.spawn_run(&id, true).await {
            Ok(run) => run,
            Err(SpawnFailure::ResumeNotFound) => self.spawn_run(&id, false).await?,
            Err(SpawnFailure::Other(error)) => return Err(error),
        };
        self.run = Some(run);
        Ok(())
    }

    /// Without `--model` Claude runs the model its catalogue calls `default`. A model the
    /// catalogue does not list has no known levels.
    fn model(&self, name: Option<&str>) -> ModelInfo {
        let name = name.unwrap_or("default");
        self.catalogue
            .lock()
            .model(name)
            .unwrap_or_else(|| ModelInfo {
                provider: "claude".into(),
                id: name.into(),
                context_window: 0,
                reasoning: false,
                thinking_levels: Vec::new(),
            })
    }

    fn current_model(&self) -> ModelInfo {
        self.model(
            self.params
                .as_ref()
                .and_then(|params| params.model.as_deref()),
        )
    }

    /// Claude runs the requested level, or the nearest one the model has.
    async fn follow_model(&mut self, levels: &[ThinkingLevel]) -> Result<(), Refusal> {
        let level = self.thinking.requested.clamp(levels);
        if level == self.thinking.running {
            return Ok(());
        }
        self.run_thinking(level).await
    }

    /// Lifting a zero cap restores the effort Claude was started with, so the effort is sent
    /// after it.
    async fn run_thinking(&mut self, level: ThinkingLevel) -> Result<(), Refusal> {
        match items::effort(level) {
            None => {
                self.control(thinking_cap(Some(0))).await?;
            }
            Some(effort) => {
                if self.thinking.running == ThinkingLevel::Off {
                    self.control(thinking_cap(None)).await?;
                }
                self.control(json!({"subtype": "apply_flag_settings",
                    "settings": {"effortLevel": effort}}))
                    .await?;
            }
        }
        self.thinking.running = level;
        self.thinking.chosen = true;
        Ok(())
    }

    async fn control(&mut self, request: Value) -> Result<Value, Refusal> {
        let id = self.next_request_id();
        let run = self.running()?;
        self.ask(run, id, request).await
    }

    async fn ask(&self, run: &Run, id: String, request: Value) -> Result<Value, Refusal> {
        let subtype = request["subtype"].as_str().unwrap_or_default().to_owned();
        let Some(answer) = run.shared.expect_reply(&id) else {
            return Err(String::from("claude is not running").into());
        };
        if let Err(error) = run.writer.send(&wire::control_request(&id, request)) {
            run.shared.forget_reply(&id);
            return Err(error.into());
        }
        match tokio::time::timeout(self.timeouts.handshake, answer).await {
            Ok(Ok(response)) if response.subtype == "success" => Ok(response.response),
            Ok(Ok(response)) => Err(Refusal {
                message: response
                    .error
                    .unwrap_or_else(|| format!("claude refused {subtype}")),
                code: response.error_code,
            }),
            Ok(Err(_)) => Err(String::from("claude exited").into()),
            Err(_) => {
                run.shared.forget_reply(&id);
                Err(format!(
                    "claude timed out: no answer to {subtype} within {}",
                    seconds(self.timeouts.handshake)
                )
                .into())
            }
        }
    }

    /// Claude only stops for a mode change at its start: the process is restarted on the same
    /// conversation, which has to be quiet.
    async fn switch_permissions(&mut self, mode: &str) -> Result<CommandAction, String> {
        let mode = match mode {
            "auto" | "yolo" => mode,
            _ => "rules",
        };
        let current = self
            .params
            .as_ref()
            .and_then(|params| params.mode.as_deref());
        if items::permission_mode(current) == items::permission_mode(Some(mode)) {
            return Ok(CommandAction::PermissionMode(mode.to_owned()));
        }
        let restart = match self.running() {
            Ok(run) => {
                let shared = &run.shared;
                if !shared.approvals().is_empty() {
                    return Err("有待处理的审批，无法切换权限".into());
                }
                if shared.turn_active() || shared.claude_turn() {
                    return Err("回合运行中无法切换权限".into());
                }
                if shared.live_agents.load(Ordering::SeqCst) > 0 {
                    return Err("后台子代理仍在运行，无法切换权限".into());
                }
                true
            }
            Err(_) => false,
        };
        let previous = self
            .params
            .as_mut()
            .and_then(|params| params.mode.replace(mode.to_owned()));
        if restart && let Err(error) = self.respawn_resuming().await {
            if let Some(params) = &mut self.params {
                params.mode = previous;
            }
            return Err(format!("claude 未能以新的权限档位重启：{error}"));
        }
        Ok(CommandAction::PermissionMode(mode.to_owned()))
    }

    fn running(&self) -> Result<&Run, String> {
        self.run
            .as_ref()
            .filter(|run| run.shared.is_usable())
            .ok_or_else(|| "claude is not running".into())
    }

    fn next_request_id(&mut self) -> String {
        self.requests += 1;
        format!("tiechui-{}", self.requests)
    }
}

#[async_trait]
impl Backend for ClaudeBackend {
    async fn start(
        &mut self,
        params: StartParams,
    ) -> Result<(Started, mpsc::Receiver<BackendEvent>), String> {
        let (events, received) = mpsc::channel(1024);
        self.events = Some(events);
        self.catalogue
            .lock()
            .set_skill_roots(catalogue::skill_roots(&params.cwd));
        self.thinking = Thinking {
            chosen: params.thinking.is_some(),
            ..Thinking::default()
        };
        if let Some(level) = params.thinking {
            self.thinking.requested = level;
            self.thinking.running = level;
        }
        let resume = params.resume.clone();
        self.params = Some(params);
        let opened = self.open(resume.as_deref()).await?;
        self.run = Some(opened.run);
        let model = self.current_model();
        // A level the session did not ask for is Claude's own default and is left alone.
        if self.thinking.chosen {
            self.follow_model(&model.thinking_levels)
                .await
                .map_err(|refusal| refusal.message)?;
        } else {
            self.thinking.running = self.thinking.requested.clamp(&model.thinking_levels);
        }
        let started = Started {
            external_id: self.session_id.clone(),
            resumed: opened.resumed,
            model,
            thinking: self.thinking.running,
        };
        Ok((started, received))
    }

    async fn start_turn(&mut self, input: &[InputContent]) -> Result<(), String> {
        if self.running().is_err() {
            self.respawn_resuming().await?;
        }
        let input = self.catalogue.lock().dispatch(input);
        let input = input.as_slice();
        let run = self.running()?;
        let shared = &run.shared;
        shared.interrupt_sent.store(false, Ordering::SeqCst);
        shared.turn_active.store(true, Ordering::SeqCst);
        let first_message = !shared.user_message_sent.swap(true, Ordering::SeqCst);
        let sent = run.writer.send(&wire::user_message(input));
        if sent.is_err() {
            shared.turn_active.store(false, Ordering::SeqCst);
            shared
                .user_message_sent
                .store(!first_message, Ordering::SeqCst);
        }
        sent
    }

    async fn steer(&mut self, input: &[InputContent]) -> Result<Steered, String> {
        let Ok(run) = self.running() else {
            return Ok(Steered::Rejected("claude is not running".into()));
        };
        if !run.shared.turn_active() {
            return Ok(Steered::Rejected("no turn is running".into()));
        }
        let input = self.catalogue.lock().dispatch(input);
        Ok(match run.writer.send(&wire::user_message(&input)) {
            Ok(()) => Steered::Accepted,
            Err(error) => Steered::Rejected(error),
        })
    }

    async fn interrupt(&mut self) -> Result<(), String> {
        let id = self.next_request_id();
        let Ok(run) = self.running() else {
            return Ok(());
        };
        // A turn Claude started by itself is interrupted the same way.
        if !run.shared.turn_active() && !run.shared.claude_turn() {
            return Ok(());
        }
        run.shared.interrupt_sent.store(true, Ordering::SeqCst);
        // The response only says the request was taken; the turn ends with an aborted `result`.
        run.writer.send(&wire::interrupt_request(&id))
    }

    async fn answer_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), String> {
        let Some(run) = &self.run else {
            return Ok(());
        };
        let Some(pending) = run.shared.take_for_answer(approval_id) else {
            return Ok(());
        };
        run.writer.send(&wire::control_success(
            &pending.request_id,
            pending.response(&decision),
        ))
    }

    async fn run_command(&mut self, name: &str, arguments: &str) -> Result<CommandAction, String> {
        if name == PERMISSIONS_COMMAND {
            return self.switch_permissions(arguments.trim()).await;
        }
        if !self.catalogue.lock().offers(name) {
            return Err(format!("claude 没有命令或技能 /{name}"));
        }
        let arguments = arguments.trim();
        Ok(CommandAction::Prompt(if arguments.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {arguments}")
        }))
    }

    async fn command_input(&mut self, name: &str, _: Value) -> Result<(), String> {
        Err(format!("claude 的命令 /{name} 不接受额外输入"))
    }

    async fn set_model(&mut self, model: &str) -> Result<ModelInfo, String> {
        if self.running().is_err() {
            self.respawn_resuming().await?;
        }
        if let Err(refusal) = self
            .control(json!({"subtype": "set_model", "model": model}))
            .await
        {
            return Err(model_refused(model, refusal));
        }
        if let Some(params) = &mut self.params {
            params.model = Some(model.to_owned());
        }
        let info = self.model(Some(model));
        // Like the session, a model without known levels keeps the level shown.
        if !info.thinking_levels.is_empty()
            && let Err(refusal) = self.follow_model(&info.thinking_levels).await
            && let Some(events) = &self.events
        {
            let message = format!("claude 已切换模型，但未能调整思考档位：{}", refusal.message);
            let _ = events.send(BackendEvent::Error(message)).await;
        }
        Ok(info)
    }

    /// Taken while a turn runs too; Claude applies it from its next request.
    async fn set_thinking(&mut self, level: ThinkingLevel) -> Result<ThinkingLevel, String> {
        let model = self.current_model();
        if !model.thinking_levels.is_empty() && !model.thinking_levels.contains(&level) {
            let levels: Vec<&str> = model
                .thinking_levels
                .iter()
                .map(|level| items::level_name(*level))
                .collect();
            return Err(format!(
                "claude 的模型 {} 没有思考档位 {}，可选：{}",
                model.id,
                items::level_name(level),
                levels.join(", ")
            ));
        }
        if self.running().is_ok() {
            self.run_thinking(level)
                .await
                .map_err(|refusal| refusal.message)?;
        } else {
            // The next process starts with it.
            self.thinking.running = level;
            self.thinking.chosen = true;
        }
        self.thinking.requested = level;
        Ok(level)
    }

    async fn shutdown(&mut self) {
        self.events = None;
        if let Some(run) = self.run.take() {
            run.close().await;
        }
    }
}

/// `None` lifts the cap: Claude goes back to the thinking it was started with.
fn thinking_cap(tokens: Option<u64>) -> Value {
    json!({"subtype": "set_max_thinking_tokens", "max_thinking_tokens": tokens})
}

fn model_refused(model: &str, refusal: Refusal) -> String {
    match refusal.code.as_deref() {
        Some("not_found" | "catalog_unknown") => format!("claude 找不到模型 {model}"),
        Some("auth_failed") => "claude 未登录或登录已过期，请在终端运行 claude 完成登录".into(),
        Some("restricted_by_org" | "unavailable_for_account" | "not_offered") => {
            format!("模型 {model} 受限：{}", refusal.message)
        }
        _ => refusal.message,
    }
}
