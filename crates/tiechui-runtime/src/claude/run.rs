use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::debug;
use super::items::Pending;
use crate::process::{Frame, FrameReader, Process};

const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

pub enum Inbound {
    Frame(Vec<u8>),
    Oversize(usize),
    Eof,
}

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
    pub supervisor: mpsc::UnboundedSender<SupervisorCommand>,
    answered_agents: mpsc::UnboundedSender<String>,
    pending_by_approval_id: Mutex<BTreeMap<String, Pending>>,
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
            supervisor,
            answered_agents,
            pending_by_approval_id: Mutex::default(),
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
    pub fn spawn(mut stdin: ChildStdin) -> (Self, JoinHandle<()>) {
        let (queue, mut lines) = mpsc::unbounded_channel::<Vec<u8>>();
        let task = tokio::spawn(async move {
            while let Some(line) = lines.recv().await {
                let write = async {
                    stdin.write_all(&line).await?;
                    stdin.flush().await
                };
                match tokio::time::timeout(WRITE_TIMEOUT, write).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        debug(&format!("cannot write to claude: {error}"));
                        return;
                    }
                    Err(_) => {
                        debug("claude stopped reading its input");
                        return;
                    }
                }
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

pub type StderrTail = Arc<Mutex<VecDeque<String>>>;

pub async fn read_frames(stdout: impl AsyncRead + Unpin, frames: mpsc::Sender<Inbound>) {
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

pub async fn read_stderr(stderr: impl AsyncRead + Unpin, tail: StderrTail) {
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
                    shared.usable.store(false, Ordering::SeqCst);
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
