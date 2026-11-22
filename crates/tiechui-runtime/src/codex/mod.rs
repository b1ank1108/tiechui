//! Codex (`codex app-server`): the probe, and a backend that speaks its JSON-RPC over stdio.
//!
//! One `codex app-server` process serves one thread. Threads of Codex's own sub-agents share the
//! connection; their frames never touch the root thread's turn (see `translate`).

mod items;
mod translate;
mod wire;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tiechui_protocol::{
    ApprovalDecision, InputContent, ModelInfo, RuntimeCapabilities, ThinkingLevel,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::backend::{Backend, BackendEvent, StartParams, Started, Steered};
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
        RuntimeCapabilities::minimal()
    }

    fn gated_ops(&self) -> &'static [&'static str] {
        &[]
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
        }))
    }
}

struct Run {
    process: Option<Process>,
    rpc: Arc<Rpc>,
    shared: Arc<Shared>,
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

    async fn handshake(&self, resume: Option<&str>) -> Result<Started, String> {
        let run = self.running()?;
        let (rpc, shared) = (&run.rpc, &run.shared);
        self.initialize(rpc, shared).await?;
        let (value, resumed) = self.open_thread(rpc, resume).await?;
        let thread: wire::ThreadResponse = serde_json::from_value(value)
            .map_err(|error| format!("codex thread response: {error}"))?;
        *shared.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread.thread.id.clone());
        let model = if thread.model.is_empty() {
            self.model.clone().unwrap_or_else(|| "codex".into())
        } else {
            thread.model
        };
        Ok(Started {
            external_id: thread.thread.id,
            resumed,
            model: ModelInfo {
                provider: "codex".into(),
                id: model,
                context_window: 0,
                reasoning: false,
                thinking_levels: vec![ThinkingLevel::Off],
            },
            thinking: ThinkingLevel::Off,
        })
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
            tasks,
        });
        self.policy = Policy::of(params.mode.as_deref());
        self.cwd = params.cwd.display().to_string();
        self.model = params.model.clone();
        match self.handshake(params.resume.as_deref()).await {
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
                    input: wire::user_input(input),
                    cwd: &self.cwd,
                    model: self.model.as_deref(),
                    approval_policy: self.policy.approval,
                    approvals_reviewer: self.policy.reviewer,
                    sandbox_policy: self.policy.sandbox_policy(),
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
                json!({"threadId": thread, "expectedTurnId": turn, "input": wire::user_input(input)}),
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
