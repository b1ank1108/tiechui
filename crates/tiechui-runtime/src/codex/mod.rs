//! Codex (`codex app-server`): the probe, and a backend that speaks its JSON-RPC over stdio.
//!
//! One `codex app-server` process serves one thread. Threads of Codex's own sub-agents share the
//! connection; their frames never touch the root thread's turn (see `translate`).
//!
//! The model, reasoning effort and permission mode the session switches to are only remembered;
//! the next `turn/start` carries them, and Codex keeps them for the turns after it.

mod items;
mod translate;
mod wire;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tiechui_protocol::{
    ApprovalDecision, CommandInfo, InputContent, ModelInfo, PERMISSIONS_COMMAND,
    RuntimeCapabilities, ThinkingLevel,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::backend::{Backend, BackendEvent, CommandAction, StartParams, Started, Steered};
use crate::process::{self, Process, StderrTail, overridable, seconds};
use crate::registry::{
    Driver, Launch, Probe, parse_version, probe_at_least, resolve_in_path, too_old,
};
use crate::rpc::{CallError, Rpc};
use items::Policy;
use translate::{Shared, Translator, TurnState};

/// The oldest Codex the driver supports: the oldest one whose frames were recorded for it
/// (0.156.1, which has `turn/steer`, `thread/resume` without turns and sub-agent items). The
/// newest verified live is 0.171.0.
pub const MIN_VERSION: &str = "0.156.1";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
/// `thread/start` and `thread/resume` load models, MCP servers and history.
const THREAD_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_ID_WAIT: Duration = Duration::from_secs(5);

const HANDSHAKE_TIMEOUT_ENV: &str = "CHUI_CODEX_HANDSHAKE_TIMEOUT_MS";

#[derive(Debug, Clone, Copy)]
struct Timeouts {
    handshake: Duration,
    thread: Duration,
    request: Duration,
}

impl Timeouts {
    fn current() -> Self {
        Self {
            handshake: overridable(HANDSHAKE_TIMEOUT, HANDSHAKE_TIMEOUT_ENV),
            thread: overridable(THREAD_TIMEOUT, HANDSHAKE_TIMEOUT_ENV),
            request: REQUEST_TIMEOUT,
        }
    }
}

fn debug(message: &str) {
    process::debug("codex", message);
}

pub struct CodexDriver;

#[async_trait]
impl Driver for CodexDriver {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn label(&self) -> &'static str {
        "Codex"
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            model: true,
            thinking: true,
            commands: true,
            permissions: true,
            ..RuntimeCapabilities::minimal()
        }
    }

    fn gated_ops(&self) -> &'static [&'static str] {
        &[
            "run_command",
            "command_input",
            "permissions",
            "set_thinking",
            "set_model",
        ]
    }

    async fn probe(&self, launch: &Launch) -> Probe {
        probe_at_least("codex", MIN_VERSION, launch).await
    }

    fn backend(&self, launch: &Launch) -> Result<Box<dyn Backend>, String> {
        Ok(Box::new(CodexBackend {
            launch: launch.clone(),
            timeouts: Timeouts::current(),
            run: None,
            policy: Policy::of(None),
            cwd: String::new(),
            model: None,
            thread_model: String::new(),
            effort: None,
            catalogue: Vec::new(),
            skills: Vec::new(),
        }))
    }
}

struct Run {
    process: Option<Process>,
    rpc: Arc<Rpc>,
    shared: Arc<Shared>,
    events: mpsc::WeakSender<BackendEvent>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Run {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

struct CodexBackend {
    launch: Launch,
    timeouts: Timeouts,
    run: Option<Run>,
    policy: Policy,
    cwd: String,
    model: Option<String>,
    thread_model: String,
    effort: Option<String>,
    catalogue: Vec<wire::Model>,
    skills: Vec<CommandInfo>,
}

impl CodexBackend {
    fn running(&self) -> Result<&Run, String> {
        self.run
            .as_ref()
            .ok_or_else(|| "codex is not running".into())
    }

    fn thread_params<'a>(&'a self, thread_id: Option<&'a str>) -> wire::ThreadParams<'a> {
        wire::ThreadParams {
            thread_id,
            cwd: &self.cwd,
            model: self.model.as_deref(),
            approval_policy: self.policy.approval,
            approvals_reviewer: self.policy.reviewer,
            sandbox: self.policy.sandbox,
            exclude_turns: thread_id.map(|_| true),
        }
    }

    async fn handshake(&mut self, params: &StartParams) -> Result<Started, String> {
        let run = self.running()?;
        let (rpc, shared, events) = (run.rpc.clone(), run.shared.clone(), run.events.clone());
        self.initialize(&rpc, &shared).await?;
        let (value, resumed) = self.open_thread(&rpc, params.resume.as_deref()).await?;
        let thread: wire::ThreadResponse = serde_json::from_value(value)
            .map_err(|error| format!("codex thread response: {error}"))?;
        *shared.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread.thread.id.clone());
        self.thread_model = if thread.model.is_empty() {
            self.model.clone().unwrap_or_else(|| "codex".into())
        } else {
            thread.model
        };
        self.catalogue = self.list_models(&rpc).await;
        self.effort = match params.thinking.map(|level| self.effort_for(level)) {
            Some(Ok(effort)) => Some(effort.to_owned()),
            Some(Err(error)) => {
                debug(&format!(
                    "the recorded thinking level stays unused: {error}"
                ));
                thread.reasoning_effort
            }
            None => thread.reasoning_effort,
        };
        self.skills = self.list_skills(&rpc).await;
        let models: Vec<ModelInfo> = self
            .catalogue
            .iter()
            .filter(|model| !model.hidden)
            .map(items::model_info)
            .collect();
        if let Some(events) = events.upgrade() {
            if !models.is_empty() {
                let _ = events.send(BackendEvent::Models(models)).await;
            }
            if !self.skills.is_empty() {
                let _ = events
                    .send(BackendEvent::Commands(self.skills.clone()))
                    .await;
            }
        }
        Ok(Started {
            external_id: thread.thread.id,
            resumed,
            model: self.model_info(),
            thinking: self.thinking(),
        })
    }

    async fn list_models(&self, rpc: &Rpc) -> Vec<wire::Model> {
        let mut models = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => json!({}),
            };
            let page = rpc
                .call("model/list", params, self.timeouts.request)
                .await
                .map_err(|error| error.describe("codex", "model/list"))
                .and_then(|value| {
                    serde_json::from_value::<wire::ModelPage>(value)
                        .map_err(|error| format!("codex model/list response: {error}"))
                });
            match page {
                Ok(page) => {
                    models.extend(page.data);
                    match page.next_cursor {
                        Some(next) => cursor = Some(next),
                        None => return models,
                    }
                }
                Err(error) => {
                    debug(&error);
                    return models;
                }
            }
        }
    }

    async fn list_skills(&self, rpc: &Rpc) -> Vec<CommandInfo> {
        let listed = rpc
            .call(
                "skills/list",
                json!({"cwds": [self.cwd]}),
                self.timeouts.request,
            )
            .await
            .map_err(|error| error.describe("codex", "skills/list"))
            .and_then(|value| {
                serde_json::from_value::<wire::SkillList>(value)
                    .map_err(|error| format!("codex skills/list response: {error}"))
            });
        match listed {
            Ok(list) => items::skill_commands(list.data),
            Err(error) => {
                debug(&error);
                Vec::new()
            }
        }
    }

    fn find_model(&self, name: &str) -> Option<&wire::Model> {
        self.catalogue
            .iter()
            .find(|model| model.id == name || model.model == name)
    }

    fn model_info(&self) -> ModelInfo {
        match self.find_model(&self.thread_model) {
            Some(model) => items::model_info(model),
            None => ModelInfo {
                provider: "codex".into(),
                id: self.thread_model.clone(),
                context_window: 0,
                reasoning: false,
                thinking_levels: vec![ThinkingLevel::Off],
            },
        }
    }

    fn thinking(&self) -> ThinkingLevel {
        let default = self
            .find_model(&self.thread_model)
            .and_then(|model| model.default_reasoning_effort.as_deref());
        [self.effort.as_deref(), default]
            .into_iter()
            .flatten()
            .find_map(items::effort_level)
            .unwrap_or(ThinkingLevel::Off)
    }

    fn effort_for(&self, level: ThinkingLevel) -> Result<&'static str, String> {
        let model = self
            .find_model(&self.thread_model)
            .ok_or_else(|| format!("codex lists no reasoning efforts for {}", self.thread_model))?;
        let takes = items::model_info(model)
            .thinking_levels
            .into_iter()
            .filter_map(items::level_effort)
            .collect::<Vec<_>>()
            .join(", ");
        let Some(effort) = items::level_effort(level) else {
            return Err(format!(
                "codex cannot turn reasoning off; {} takes {takes}",
                model.id
            ));
        };
        if !items::supports(model, effort) {
            return Err(format!(
                "{} does not support reasoning effort {effort}; it takes {takes}",
                model.id
            ));
        }
        Ok(effort)
    }

    fn input(&self, content: &[InputContent]) -> Vec<Value> {
        let content: Vec<InputContent> = content
            .iter()
            .map(|part| match part {
                InputContent::Text { text } => InputContent::Text {
                    text: items::skill_mentions(text, &self.skills),
                },
                image => image.clone(),
            })
            .collect();
        wire::user_input(&content)
    }

    async fn initialize(&self, rpc: &Rpc, shared: &Shared) -> Result<(), String> {
        let timeouts = self.timeouts;
        let (_, initialized) = rpc
            .send("initialize", wire::initialize_params())
            .await
            .map_err(|error| error.describe("codex", "initialize"))?;
        tokio::time::timeout(timeouts.handshake, shared.first_frame.notified())
            .await
            .map_err(|_| {
                format!(
                    "codex app-server timed out: no output within {} of starting",
                    seconds(timeouts.handshake)
                )
            })?;
        let response = tokio::time::timeout(timeouts.handshake, initialized)
            .await
            .map_err(|_| {
                format!(
                    "codex app-server timed out: no answer to initialize within {}",
                    seconds(timeouts.handshake)
                )
            })?
            .map_err(|_| "codex exited before it answered initialize".to_owned())?
            .map_err(|error| format!("codex initialize: {}", error.message))?;
        let agent = response["userAgent"].as_str().unwrap_or_default();
        if let Some(version) = parse_version(agent)
            && let Some(reason) = too_old("codex", &version, MIN_VERSION)
        {
            return Err(reason);
        }
        rpc.notify("initialized").await
    }

    async fn open_thread(
        &self,
        rpc: &Rpc,
        resume: Option<&str>,
    ) -> Result<(serde_json::Value, bool), String> {
        let timeouts = self.timeouts;
        if let Some(id) = resume {
            match rpc
                .call(
                    "thread/resume",
                    self.thread_params(Some(id)),
                    timeouts.thread,
                )
                .await
            {
                Ok(value) => return Ok((value, true)),
                // Codex no longer has the thread: the session continues on a new one.
                Err(CallError::Rpc(error)) => {
                    debug(&format!("thread/resume failed: {}", error.message));
                }
                Err(error) => return Err(error.describe("codex", "thread/resume")),
            }
        }
        let value = rpc
            .call("thread/start", self.thread_params(None), timeouts.thread)
            .await
            .map_err(|error| error.describe("codex", "thread/start"))?;
        Ok((value, false))
    }

    fn thread_id(&self) -> Result<String, String> {
        self.running()?
            .shared
            .root()
            .ok_or_else(|| "codex has no thread yet".into())
    }
}

#[async_trait]
impl Backend for CodexBackend {
    async fn start(
        &mut self,
        params: StartParams,
    ) -> Result<(Started, mpsc::Receiver<BackendEvent>), String> {
        if self.run.is_some() {
            return Err("codex is already running".into());
        }
        let binary =
            resolve_in_path("codex", self.launch.path.as_deref()).ok_or("codex is not on PATH")?;
        let mut process = Process::spawn(
            &binary,
            &["app-server", "--listen", "stdio://"],
            &params.cwd,
            self.launch.path.as_ref(),
        )
        .map_err(|error| format!("cannot start {}: {error}", binary.display()))?;
        let (stdin, stdout, stderr) = process.take_stdio("codex")?;
        let rpc = Arc::new(Rpc::new(stdin, "codex", false));
        let shared = Arc::new(Shared::default());
        let tail = StderrTail::default();
        let (events, received) = mpsc::channel(1024);
        let weak = events.downgrade();
        let (frames, inbound) = mpsc::channel(256);
        let translator = Translator::new(shared.clone(), rpc.clone(), events, tail.clone());
        let tasks = vec![
            tokio::spawn(process::read_frames("codex", stdout, frames)),
            tokio::spawn(process::read_stderr("codex", stderr, tail)),
            tokio::spawn(translator.run(inbound)),
        ];
        self.run = Some(Run {
            process: Some(process),
            rpc,
            shared,
            events: weak,
            tasks,
        });
        self.policy = Policy::of(params.mode.as_deref());
        self.cwd = params.cwd.display().to_string();
        self.model = params.model.clone();
        match self.handshake(&params).await {
            Ok(started) => Ok((started, received)),
            Err(error) => {
                self.shutdown().await;
                Err(error)
            }
        }
    }

    async fn start_turn(&mut self, input: &[InputContent]) -> Result<(), String> {
        let thread = self.thread_id()?;
        let run = self.running()?;
        run.shared
            .turn
            .send_modify(|state| state.awaiting_turn_started = true);
        let result = run
            .rpc
            .call(
                "turn/start",
                wire::TurnStartParams {
                    thread_id: &thread,
                    input: self.input(input),
                    cwd: &self.cwd,
                    model: self.model.as_deref(),
                    approval_policy: self.policy.approval,
                    approvals_reviewer: self.policy.reviewer,
                    sandbox_policy: self.policy.sandbox_policy(),
                    effort: self.effort.as_deref(),
                    summary: "auto",
                },
                self.timeouts.request,
            )
            .await;
        let started = result
            .map_err(|error| error.describe("codex", "turn/start"))
            .and_then(|value| {
                serde_json::from_value::<wire::TurnResponse>(value)
                    .map_err(|error| format!("codex turn/start response: {error}"))
            });
        match started {
            Ok(response) => {
                run.shared
                    .turn
                    .send_modify(|state| state.record_turn_id_if_unset(response.turn.id));
                Ok(())
            }
            Err(error) => {
                run.shared
                    .turn
                    .send_modify(|state| state.awaiting_turn_started = false);
                Err(error)
            }
        }
    }

    async fn steer(&mut self, input: &[InputContent]) -> Result<Steered, String> {
        let thread = self.thread_id()?;
        let run = self.running()?;
        let Some(turn) = run.shared.turn.borrow().active.clone() else {
            return Ok(Steered::Rejected("no turn is running".into()));
        };
        let result = run
            .rpc
            .call(
                "turn/steer",
                json!({"threadId": thread, "expectedTurnId": turn, "input": self.input(input)}),
                self.timeouts.request,
            )
            .await;
        Ok(match result {
            Ok(_) => Steered::Accepted,
            Err(error) => Steered::Rejected(error.describe("codex", "turn/steer")),
        })
    }

    async fn interrupt(&mut self) -> Result<(), String> {
        let thread = self.thread_id()?;
        let run = self.running()?;
        let mut turns = run.shared.turn.subscribe();
        // A turn that was just started has no id yet.
        let state: TurnState = tokio::time::timeout(
            TURN_ID_WAIT,
            turns.wait_for(|state| state.active.is_some() || !state.awaiting_turn_started),
        )
        .await
        .map_err(|_| "the turn did not report its id".to_owned())?
        .map_err(|_| "codex exited".to_owned())?
        .clone();
        let Some(turn) = state.active else {
            return Ok(());
        };
        // The response only says the request was taken; the turn ends with turn/completed.
        run.rpc
            .send(
                "turn/interrupt",
                json!({"threadId": thread, "turnId": turn}),
            )
            .await
            .map(|_| ())
            .map_err(|error| error.describe("codex", "turn/interrupt"))
    }

    async fn answer_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), String> {
        let run = self.running()?;
        let Some(pending) = run.shared.take_approval(approval_id) else {
            return Ok(());
        };
        run.rpc
            .respond(pending.rpc_id(), pending.response(&decision))
            .await
    }

    async fn run_command(&mut self, name: &str, arguments: &str) -> Result<CommandAction, String> {
        if name == PERMISSIONS_COMMAND {
            self.policy = Policy::of(Some(arguments.trim()));
            return Ok(CommandAction::PermissionMode(self.policy.mode.into()));
        }
        if !self.skills.iter().any(|skill| skill.name == name) {
            return Err(format!("codex has no skill {name}"));
        }
        let arguments = arguments.trim();
        Ok(CommandAction::Prompt(if arguments.is_empty() {
            format!("${name}")
        } else {
            format!("${name} {arguments}")
        }))
    }

    async fn command_input(&mut self, name: &str, _: Value) -> Result<(), String> {
        Err(format!("codex takes no command input (for {name})"))
    }

    async fn set_model(&mut self, model: &str) -> Result<ModelInfo, String> {
        let chosen = self
            .find_model(model)
            .cloned()
            .ok_or_else(|| format!("codex does not list model {model}"))?;
        // Codex keeps an effort for later turns; one the new model lacks becomes its default.
        if self
            .effort
            .as_deref()
            .is_some_and(|effort| !items::supports(&chosen, effort))
        {
            self.effort = chosen.default_reasoning_effort.clone();
        }
        self.model = Some(chosen.model.clone());
        self.thread_model = chosen.model.clone();
        Ok(items::model_info(&chosen))
    }

    async fn set_thinking(&mut self, level: ThinkingLevel) -> Result<ThinkingLevel, String> {
        self.effort = Some(self.effort_for(level)?.to_owned());
        Ok(level)
    }

    async fn shutdown(&mut self) {
        if let Some(mut run) = self.run.take() {
            for task in &run.tasks {
                task.abort();
            }
            if let Some(process) = run.process.take() {
                process.close().await;
            }
        }
    }
}
