//! Claude Code (`claude -p` with bidirectional stream-json): the probe, and a backend that
//! writes user messages and control requests to the process's stdin and maps what it prints.
//!
//! One `claude` process serves one conversation. Its session id is chosen by the driver
//! (`--session-id`) so it is known before the first frame; a reopened session resumes it with
//! `--resume`, which keeps the id. After an error `result` the process is stopped, and the next
//! turn starts a new one that resumes the same conversation.

mod items;
mod run;
mod translate;
mod wire;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use async_trait::async_trait;
use tiechui_protocol::{
    ApprovalDecision, InputContent, ModelInfo, RuntimeCapabilities, ThinkingLevel,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::backend::{Backend, BackendEvent, StartParams, Started, Steered};
use crate::process::Process;
use crate::registry::{Driver, Launch, Probe, probe_binary, resolve_in_path};
use run::{Hello, Shared, StderrTail, Supervise, Writer};
use translate::{Counted, Translator};

/// The oldest Claude Code the driver supports: 2.1.322, the only version its frames were
/// recorded from. The driver relies on `--session-id` and `--resume` in stream-json mode,
/// `--permission-prompt-tool stdio`, `--thinking-display`, the `initialize` and `interrupt`
/// control requests and the per-message `stream_event` usage; a lower bound needs recordings of
/// an older version.
pub const MIN_VERSION: &str = "2.1.322";

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a process that reported an error `result` may keep running before it is stopped.
const REAP_GRACE: Duration = Duration::from_secs(10);

#[cfg(feature = "fake-runtime")]
pub const HANDSHAKE_TIMEOUT_ENV: &str = "CHUI_CLAUDE_HANDSHAKE_TIMEOUT_MS";
#[cfg(feature = "fake-runtime")]
pub const REAP_TIMEOUT_ENV: &str = "CHUI_CLAUDE_REAP_TIMEOUT_MS";

#[derive(Debug, Clone, Copy)]
struct Timeouts {
    handshake: Duration,
    reap: Duration,
}

impl Timeouts {
    fn current() -> Self {
        #[allow(unused_mut)]
        let mut timeouts = Self {
            handshake: HANDSHAKE_TIMEOUT,
            reap: REAP_GRACE,
        };
        #[cfg(feature = "fake-runtime")]
        {
            let millis = |name: &str| std::env::var(name).ok().and_then(|v| v.parse().ok());
            if let Some(ms) = millis(HANDSHAKE_TIMEOUT_ENV) {
                timeouts.handshake = Duration::from_millis(ms);
            }
            if let Some(ms) = millis(REAP_TIMEOUT_ENV) {
                timeouts.reap = Duration::from_millis(ms);
            }
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
    if std::env::var_os("CHUI_CLAUDE_DEBUG").is_some() {
        eprintln!("chui claude: {message}");
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
        // A message written while a turn runs joins that turn (recordings of 2.1.322).
        RuntimeCapabilities::minimal()
    }

    fn gated_ops(&self) -> &'static [&'static str] {
        &[]
    }

    async fn probe(&self, launch: &Launch) -> Probe {
        let mut probe = probe_binary("claude", "--version", launch).await;
        if let (true, Some(version)) = (probe.available, &probe.version)
            && !at_least(version, MIN_VERSION)
        {
            probe.reason = Some(format!(
                "claude {version} is older than the minimum supported version {MIN_VERSION}"
            ));
            probe.available = false;
        }
        probe
    }

    fn backend(&self, launch: &Launch) -> Result<Box<dyn Backend>, String> {
        Ok(Box::new(ClaudeBackend {
            launch: launch.clone(),
            timeouts: Timeouts::current(),
            params: None,
            session_id: String::new(),
            events: None,
            counted: Counted::default(),
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
    /// Stops the process group and waits until it is gone.
    async fn close(mut self) {
        let (ack, done) = oneshot::channel();
        if self.shared.supervisor.send(Supervise::Close(ack)).is_ok() {
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

/// Why a `claude` process could not be brought up.
enum Failure {
    /// `--resume` named a conversation Claude does not have.
    NoConversation,
    Other(String),
}

impl From<Failure> for String {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::NoConversation => "claude has no such conversation".into(),
            Failure::Other(error) => error,
        }
    }
}

struct ClaudeBackend {
    launch: Launch,
    timeouts: Timeouts,
    params: Option<StartParams>,
    session_id: String,
    /// Kept so the channel outlives a process: a new one reports on the same channel.
    events: Option<mpsc::Sender<BackendEvent>>,
    counted: Counted,
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
        if let Some(level) = params.thinking {
            let (flag, value) = items::thinking_flag(level);
            args.extend([flag.to_owned(), value.to_owned()]);
        }
        args
    }

    /// Starts `claude` on the conversation `id` and waits until it answers `initialize`.
    async fn spawn_run(&self, id: &str, resume: bool) -> Result<Run, Failure> {
        let (Some(params), Some(events)) = (&self.params, &self.events) else {
            return Err(Failure::Other("claude is not started".into()));
        };
        let binary = resolve_in_path("claude", self.launch.path.as_deref())
            .ok_or_else(|| Failure::Other("claude is not on PATH".into()))?;
        let args = self.args(params, id, resume);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let mut process = Process::spawn_filtered(
            &binary,
            &args,
            &params.cwd,
            self.launch.path.as_ref(),
            items::is_host_variable,
        )
        .map_err(|error| Failure::Other(format!("cannot start {}: {error}", binary.display())))?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (
            process.stdin.take(),
            process.stdout.take(),
            process.stderr.take(),
        ) else {
            return Err(Failure::Other("claude started without its stdio".into()));
        };
        let (writer, write_task) = Writer::spawn(stdin);
        let (shared, commands) = Shared::new("tiechui-init".into());
        let tail = StderrTail::default();
        let (frames, inbound) = mpsc::channel(256);
        let translator = Translator::new(
            shared.clone(),
            writer.clone(),
            events.clone(),
            self.counted.clone(),
            self.timeouts.reap,
            tail.clone(),
        );
        let tasks = vec![
            write_task,
            tokio::spawn(run::read_frames(stdout, frames)),
            tokio::spawn(run::read_stderr(stderr, tail)),
            tokio::spawn(translator.run(inbound)),
            tokio::spawn(run::supervise(process, commands, shared.clone())),
        ];
        let run = Run {
            writer,
            shared,
            tasks,
        };
        self.handshake(run, resume).await
    }

    /// Sends `initialize` and waits for the answer. A run that does not come up is stopped.
    async fn handshake(&self, run: Run, resume: bool) -> Result<Run, Failure> {
        let mut hello = run.shared.hello.subscribe();
        let sent = run
            .writer
            .send(&wire::initialize_request(&run.shared.init_id));
        let outcome = match sent {
            Ok(()) => tokio::time::timeout(
                self.timeouts.handshake,
                hello.wait_for(|hello| *hello != Hello::Pending),
            )
            .await
            .map(|result| result.map(|hello| hello.clone())),
            Err(error) => Ok(Ok(Hello::Failed(error))),
        };
        let failure = match outcome {
            Ok(Ok(Hello::Ready)) => return Ok(run),
            Ok(Ok(Hello::Failed(reason))) if resume && reason.contains("No conversation found") => {
                Failure::NoConversation
            }
            Ok(Ok(Hello::Failed(reason))) => Failure::Other(reason),
            Ok(Ok(Hello::Pending) | Err(_)) => Failure::Other("claude exited".into()),
            Err(_) => Failure::Other(format!(
                "claude timed out: no answer to initialize within {}",
                seconds(self.timeouts.handshake)
            )),
        };
        run.close().await;
        Err(failure)
    }

    /// Starts the conversation: resumes `resume` when Claude still has it, else begins a new
    /// one. The flag says which.
    async fn open(&mut self, resume: Option<&str>) -> Result<(Run, bool), String> {
        if let Some(id) = resume {
            match self.spawn_run(id, true).await {
                Ok(run) => {
                    self.session_id = id.to_owned();
                    return Ok((run, true));
                }
                // Claude no longer has the conversation: the session continues on a new one.
                Err(Failure::NoConversation) => debug("--resume found no conversation"),
                Err(Failure::Other(error)) => return Err(error),
            }
        }
        self.session_id = items::new_session_id();
        let run = self.spawn_run(&self.session_id, false).await?;
        Ok((run, false))
    }

    fn started(&self, params: &StartParams, resumed: bool) -> Started {
        Started {
            external_id: self.session_id.clone(),
            resumed,
            model: ModelInfo {
                provider: "claude".into(),
                id: params.model.clone().unwrap_or_else(|| "claude".into()),
                context_window: 0,
                reasoning: false,
                thinking_levels: vec![ThinkingLevel::Off],
            },
            thinking: ThinkingLevel::Off,
        }
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
        if self.run.is_some() {
            return Err("claude is already running".into());
        }
        let (events, received) = mpsc::channel(1024);
        self.events = Some(events);
        self.params = Some(params.clone());
        match self.open(params.resume.as_deref()).await {
            Ok((run, resumed)) => {
                self.run = Some(run);
                Ok((self.started(&params, resumed), received))
            }
            Err(error) => {
                self.events = None;
                Err(error)
            }
        }
    }

    async fn start_turn(&mut self, input: &[InputContent]) -> Result<(), String> {
        if self.running().is_err() {
            // The process exited or was stopped after an error: start one that resumes.
            if let Some(old) = self.run.take() {
                old.close().await;
            }
            let id = self.session_id.clone();
            let run = match self.spawn_run(&id, true).await {
                Ok(run) => run,
                // Nothing was saved before the process went away: start the conversation anew
                // under the same id.
                Err(Failure::NoConversation) => {
                    self.spawn_run(&id, false).await.map_err(String::from)?
                }
                Err(Failure::Other(error)) => return Err(error),
            };
            self.run = Some(run);
        }
        let run = self.running()?;
        run.shared.interrupting.store(false, Ordering::SeqCst);
        run.shared.turn_active.store(true, Ordering::SeqCst);
        run.shared.input_written.store(true, Ordering::SeqCst);
        run.writer.send(&wire::user_message(input))
    }

    async fn steer(&mut self, input: &[InputContent]) -> Result<Steered, String> {
        let Ok(run) = self.running() else {
            return Ok(Steered::Rejected("claude is not running".into()));
        };
        if !run.shared.turn_active() {
            return Ok(Steered::Rejected("no turn is running".into()));
        }
        Ok(match run.writer.send(&wire::user_message(input)) {
            Ok(()) => Steered::Accepted,
            Err(error) => Steered::Rejected(error),
        })
    }

    async fn interrupt(&mut self) -> Result<(), String> {
        let id = self.next_request_id();
        let Ok(run) = self.running() else {
            return Ok(());
        };
        if !run.shared.turn_active() {
            return Ok(());
        }
        run.shared.interrupting.store(true, Ordering::SeqCst);
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
        let Some(pending) = run.shared.take_approval(approval_id) else {
            return Ok(());
        };
        run.writer.send(&wire::control_success(
            &pending.request_id,
            pending.response(&decision),
        ))
    }

    async fn shutdown(&mut self) {
        self.events = None;
        if let Some(run) = self.run.take() {
            run.close().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_dotted_numbers() {
        assert!(at_least("2.1.322", MIN_VERSION));
        assert!(at_least("2.1.400", MIN_VERSION));
        assert!(at_least("2.2.0", MIN_VERSION));
        assert!(at_least("3.0.0", MIN_VERSION));
        assert!(!at_least("2.1.321", MIN_VERSION));
        assert!(!at_least("2.0.999", MIN_VERSION));
        assert!(!at_least("1.9.9", MIN_VERSION));
    }
}
