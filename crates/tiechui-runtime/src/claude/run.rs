use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::debug;
use super::items::Pending;
use super::wire::ControlResponse;
use crate::process::Process;

const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Claude answers `initialize` once it is up and, when resuming, has found the conversation;
/// otherwise it reports an error `result` or exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handshake {
    Pending,
    Ready,
    Failed(String),
}

pub enum SupervisorCommand {
    ReapAfter(Duration),
    Close(oneshot::Sender<()>),
}

pub struct Shared {
    pub handshake: watch::Sender<Handshake>,
    pub initialize_request_id: String,
    pub user_message_sent: AtomicBool,
    /// A turn was started (or joined) and its `result` has not arrived.
    pub turn_active: AtomicBool,
    pub claude_turn: AtomicBool,
    pub interrupt_sent: AtomicBool,
    pub usable: AtomicBool,
    /// Sub-agents still running or waiting, a background one included.
    pub live_agents: AtomicUsize,
    pub supervisor: mpsc::UnboundedSender<SupervisorCommand>,
    answered_agents: mpsc::UnboundedSender<String>,
    pending_by_approval_id: Mutex<BTreeMap<String, Pending>>,
    /// The driver's own control requests, by request id, until Claude answers them.
    awaiting_replies: Mutex<HashMap<String, oneshot::Sender<ControlResponse>>>,
}

impl Shared {
    pub fn new(
        initialize_request_id: String,
    ) -> (
        Arc<Self>,
        mpsc::UnboundedReceiver<SupervisorCommand>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let (supervisor, commands) = mpsc::unbounded_channel();
        let (answered_agents, answered) = mpsc::unbounded_channel();
        let shared = Arc::new(Self {
            handshake: watch::channel(Handshake::Pending).0,
            initialize_request_id,
            user_message_sent: AtomicBool::new(false),
            turn_active: AtomicBool::new(false),
            claude_turn: AtomicBool::new(false),
            interrupt_sent: AtomicBool::new(false),
            usable: AtomicBool::new(true),
            live_agents: AtomicUsize::new(0),
            supervisor,
            answered_agents,
            pending_by_approval_id: Mutex::default(),
            awaiting_replies: Mutex::default(),
        });
        (shared, commands, answered)
    }

    pub fn try_finish_handshake(&self, outcome: Handshake) -> bool {
        self.handshake.send_if_modified(|state| {
            let waiting = *state == Handshake::Pending;
            if waiting {
                *state = outcome;
            }
            waiting
        })
    }

    pub fn is_usable(&self) -> bool {
        self.usable.load(Ordering::SeqCst)
    }

    pub fn turn_active(&self) -> bool {
        self.turn_active.load(Ordering::SeqCst)
    }

    pub fn claude_turn(&self) -> bool {
        self.claude_turn.load(Ordering::SeqCst)
    }

    pub fn approvals(&self) -> MutexGuard<'_, BTreeMap<String, Pending>> {
        self.pending_by_approval_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn replies(&self) -> MutexGuard<'_, HashMap<String, oneshot::Sender<ControlResponse>>> {
        self.awaiting_replies
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// `None` once Claude has exited: nothing would answer.
    pub fn expect_reply(&self, id: &str) -> Option<oneshot::Receiver<ControlResponse>> {
        let (reply, answer) = oneshot::channel();
        self.replies().insert(id.to_owned(), reply);
        if self.is_usable() {
            return Some(answer);
        }
        self.forget_reply(id);
        None
    }

    pub fn forget_reply(&self, id: &str) {
        self.replies().remove(id);
    }

    pub fn deliver_reply(&self, response: ControlResponse) {
        let reply = response
            .request_id
            .as_deref()
            .and_then(|id| self.replies().remove(id));
        if let Some(reply) = reply {
            let _ = reply.send(response);
        }
    }

    /// Called after the run is marked unusable, so no request waits for an answer that cannot
    /// come.
    pub fn drop_replies(&self) {
        self.replies().clear();
    }

    pub fn take_approval(&self, id: &str) -> Option<Pending> {
        self.approvals().remove(id)
    }

    pub fn take_for_answer(&self, id: &str) -> Option<Pending> {
        let pending = self.take_approval(id)?;
        if let Some(agent) = &pending.subagent_id {
            let _ = self.answered_agents.send(agent.clone());
        }
        Some(pending)
    }
}

/// Nothing that handles Claude's output may wait on its input: a full pipe in both directions
/// would deadlock both sides.
#[derive(Clone)]
pub struct Writer {
    queue: mpsc::UnboundedSender<Vec<u8>>,
}

impl Writer {
    /// When a write fails or times out the writer stops and the run becomes unusable, so the
    /// next turn starts a new process.
    pub fn spawn(
        mut stdin: impl AsyncWrite + Unpin + Send + 'static,
        shared: Arc<Shared>,
    ) -> (Self, JoinHandle<()>) {
        let (queue, mut lines) = mpsc::unbounded_channel::<Vec<u8>>();
        let task = tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                let write = async {
                    stdin.write_all(&line).await?;
                    stdin.flush().await
                };
                let failure = match tokio::time::timeout(WRITE_TIMEOUT, write).await {
                    Ok(Ok(())) => continue,
                    Ok(Err(error)) => format!("cannot write to claude: {error}"),
                    Err(_) => "claude stopped reading its input".to_owned(),
                };
                debug(&failure);
                shared.usable.store(false, Ordering::SeqCst);
                return;
            }
        });
        (Self { queue }, task)
    }

    pub fn send(&self, frame: &Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(frame).map_err(|error| error.to_string())?;
        line.push(b'\n');
        self.queue
            .send(line)
            .map_err(|_| "claude is not taking input".to_owned())
    }
}

pub async fn supervise(
    mut process: Process,
    mut commands: mpsc::UnboundedReceiver<SupervisorCommand>,
    shared: Arc<Shared>,
) {
    let mut reap_at: Option<Instant> = None;
    let mut closing: Option<oneshot::Sender<()>> = None;
    loop {
        let due = async {
            match reap_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = process.child.wait() => break,
            command = commands.recv() => match command {
                Some(SupervisorCommand::ReapAfter(grace)) => {
                    reap_at = Some(Instant::now() + grace);
                }
                Some(SupervisorCommand::Close(ack)) => {
                    closing = Some(ack);
                    break;
                }
                None => break,
            },
            () = due => break,
        }
    }
    shared.usable.store(false, Ordering::SeqCst);
    process.close().await;
    if let Some(ack) = closing {
        let _ = ack.send(());
    }
}
