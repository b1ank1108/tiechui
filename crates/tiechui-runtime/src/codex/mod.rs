//! Codex (`codex app-server`): the probe, and a backend that speaks its JSON-RPC over stdio.
//!
//! One `codex app-server` process serves one thread. Threads of Codex's own sub-agents share the
//! connection; their frames never touch the root thread's turn (see `translate`).

mod items;
mod rpc;
mod translate;
mod wire;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;
use tiechui_protocol::{
    ApprovalDecision, InputContent, ModelInfo, RuntimeCapabilities, ThinkingLevel,
};
use tokio::io::AsyncRead;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::backend::{Backend, BackendEvent, StartParams, Started, Steered};
use crate::process::{Frame, FrameReader, Process};
use crate::registry::{Driver, Launch, Probe, parse_version, probe_binary, resolve_in_path};
use items::Policy;
use rpc::{CallError, Rpc};
use translate::{Inbound, Shared, Translator, TurnState};

/// The oldest Codex the driver supports: the oldest one whose frames were recorded for it
/// (0.156.1, which has `turn/steer`, `thread/resume` without turns and sub-agent items). The
/// newest verified live is 0.171.0.
pub const MIN_VERSION: &str = "0.156.1";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
/// `thread/start` and `thread/resume` load models, MCP servers and history.
const THREAD_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const TURN_ID_WAIT: Duration = Duration::from_secs(5);

#[cfg(feature = "fake-runtime")]
pub const HANDSHAKE_TIMEOUT_ENV: &str = "CHUI_CODEX_HANDSHAKE_TIMEOUT_MS";

#[derive(Debug, Clone, Copy)]
struct Timeouts {
    handshake: Duration,
    thread: Duration,
    request: Duration,
}

impl Timeouts {
    fn current() -> Self {
        #[allow(unused_mut)]
        let mut timeouts = Self {
            handshake: HANDSHAKE_TIMEOUT,
            thread: THREAD_TIMEOUT,
            request: REQUEST_TIMEOUT,
        };
        #[cfg(feature = "fake-runtime")]
        if let Some(ms) = std::env::var(HANDSHAKE_TIMEOUT_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
        {
            timeouts.handshake = Duration::from_millis(ms);
            timeouts.thread = Duration::from_millis(ms);
        }
        timeouts
    }
}

fn seconds(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{} s", duration.as_secs())
    } else {
        format!("{:.1} s", duration.as_secs_f64())
    }
}

fn debug(message: &str) {
    if std::env::var_os("CHUI_CODEX_DEBUG").is_some() {
        eprintln!("chui codex: {message}");
    }
}

fn version_parts(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

fn at_least(version: &str, minimum: &str) -> bool {
    let (mut version, mut minimum) = (version_parts(version), version_parts(minimum));
    let len = version.len().max(minimum.len());
    version.resize(len, 0);
    minimum.resize(len, 0);
    version >= minimum
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
        let mut probe = probe_binary("codex", "--version", launch).await;
        if let (true, Some(version)) = (probe.available, &probe.version)
            && !at_least(version, MIN_VERSION)
        {
            probe.reason = Some(format!(
                "codex {version} is older than the minimum supported version {MIN_VERSION}"
            ));
            probe.available = false;
        }
        probe
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

type StderrTail = Arc<Mutex<VecDeque<String>>>;

async fn read_frames(stdout: impl AsyncRead + Unpin, frames: mpsc::Sender<Inbound>) {
    let mut reader = FrameReader::new(stdout);
    loop {
        let message = match reader.next().await {
            Ok(Some(Frame::Line(line))) if line.is_empty() => continue,
            Ok(Some(Frame::Line(line))) => Inbound::Frame(line),
            Ok(Some(Frame::Oversize(size))) => Inbound::Oversize(size),
            Ok(None) | Err(_) => {
                let _ = frames.send(Inbound::Eof).await;
                return;
            }
        };
        if frames.send(message).await.is_err() {
            return;
        }
    }
}

async fn read_stderr(stderr: impl AsyncRead + Unpin, tail: StderrTail) {
    let mut reader = FrameReader::with_limit(stderr, 64 * 1024);
    while let Ok(Some(frame)) = reader.next().await {
        let Frame::Line(line) = frame else { continue };
        let line: String = String::from_utf8_lossy(&line)
            .trim()
            .chars()
            .take(300)
            .collect();
        if line.is_empty() {
            continue;
        }
        debug(&format!("stderr: {line}"));
        let mut tail = tail.lock().unwrap_or_else(|e| e.into_inner());
        if tail.len() == 8 {
            tail.pop_front();
        }
        tail.push_back(line);
    }
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
        let timeouts = self.timeouts;
        let (_, initialized) = rpc
            .send("initialize", wire::initialize_params())
            .await
            .map_err(|error| error.describe("initialize"))?;
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
            && !at_least(&version, MIN_VERSION)
        {
            return Err(format!(
                "codex {version} is older than the minimum supported version {MIN_VERSION}"
            ));
        }
        rpc.notify("initialized").await?;

        let mut resumed = false;
        let mut thread = None;
        if let Some(id) = resume {
            match rpc
                .call(
                    "thread/resume",
                    self.thread_params(Some(id)),
                    timeouts.thread,
                )
                .await
            {
                Ok(value) => {
                    resumed = true;
                    thread = Some(value);
                }
                // Codex no longer has the thread: the session continues on a new one.
                Err(CallError::Rpc(error)) => {
                    debug(&format!("thread/resume failed: {}", error.message));
                }
                Err(error) => return Err(error.describe("thread/resume")),
            }
        }
        let value = match thread {
            Some(value) => value,
            None => rpc
                .call("thread/start", self.thread_params(None), timeouts.thread)
                .await
                .map_err(|error| error.describe("thread/start"))?,
        };
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
        let (Some(stdin), Some(stdout), Some(stderr)) = (
            process.stdin.take(),
            process.stdout.take(),
            process.stderr.take(),
        ) else {
            return Err("codex started without its stdio".into());
        };
        let rpc = Arc::new(Rpc::new(stdin));
        let shared = Arc::new(Shared::default());
        let tail = StderrTail::default();
        let (events, received) = mpsc::channel(1024);
        let (frames, inbound) = mpsc::channel(256);
        let translator = Translator::new(shared.clone(), rpc.clone(), events, tail.clone());
        let tasks = vec![
            tokio::spawn(read_frames(stdout, frames)),
            tokio::spawn(read_stderr(stderr, tail)),
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
        run.shared.turn.send_modify(|state| state.pending = true);
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
            .map_err(|error| error.describe("turn/start"))
            .and_then(|value| {
                serde_json::from_value::<wire::TurnResponse>(value)
                    .map_err(|error| format!("codex turn/start response: {error}"))
            });
        match started {
            Ok(response) => {
                // The turn/started notification usually got there first; if it did not, this
                // is the turn id, unless the turn has already ended.
                run.shared.turn.send_modify(|state| {
                    if state.pending {
                        state.active = Some(response.turn.id);
                        state.pending = false;
                    }
                });
                Ok(())
            }
            Err(error) => {
                run.shared.turn.send_modify(|state| state.pending = false);
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
            Err(error) => Steered::Rejected(error.describe("turn/steer")),
        })
    }

    async fn interrupt(&mut self) -> Result<(), String> {
        let thread = self.thread_id()?;
        let run = self.running()?;
        let mut turns = run.shared.turn.subscribe();
        // A turn that was just started has no id yet.
        let state: TurnState = tokio::time::timeout(
            TURN_ID_WAIT,
            turns.wait_for(|state| state.active.is_some() || !state.pending),
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
            .map_err(|error| error.describe("turn/interrupt"))
    }

    async fn answer_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), String> {
        let run = self.running()?;
        // The entry is removed first, so a second answer finds nothing and writes nothing.
        let Some(pending) = run.shared.approvals().remove(approval_id) else {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_dotted_numbers() {
        assert!(at_least("0.171.0", "0.156.1"));
        assert!(at_least("0.156.1", "0.156.1"));
        assert!(at_least("1.0.0", "0.156.1"));
        assert!(at_least("0.156.1-alpha.2", "0.156.1"));
        assert!(at_least("0.200", "0.156.1"));
        assert!(!at_least("0.156.0", "0.156.1"));
        assert!(!at_least("0.99.9", "0.156.1"));
        assert!(!at_least("0.156", "0.156.1"));
    }
}
