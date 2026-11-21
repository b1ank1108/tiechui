//! One running `claude` process: the queue its stdin is written from, the tasks that drain its
//! stdout and stderr, and the supervisor that owns the process and reaps its group.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::process::ChildStdin;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::debug;
use crate::process::{Frame, FrameReader, Process};

const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the reader hands the translator.
pub enum Inbound {
    Frame(Vec<u8>),
    Oversize(usize),
    Eof,
}

/// Where the handshake stands: Claude answers `initialize` once it is up and, when resuming,
/// has found the conversation; otherwise it reports an error `result` or exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hello {
    Pending,
    Ready,
    Failed(String),
}

/// What the supervisor is asked to do with the process.
pub enum Supervise {
    /// Stop the process group after this long unless the process has exited by then.
    Reap(Duration),
    Close(oneshot::Sender<()>),
}

/// What the backend and the translator of one process share.
pub struct Shared {
    pub hello: watch::Sender<Hello>,
    /// The `request_id` of the `initialize` request.
    pub init_id: String,
    /// The first user message was written to this process. Usage reported before it belongs to
    /// the conversation's past (a resumed process repeating it) and is not counted again.
    pub input_written: AtomicBool,
    /// A turn was started (or joined) and its `result` has not arrived.
    pub turn_active: AtomicBool,
    /// An `interrupt` request is out, so an aborted `result` is the interrupt's.
    pub interrupting: AtomicBool,
    /// The process has not exited and is not being reaped.
    pub usable: AtomicBool,
    pub supervisor: mpsc::UnboundedSender<Supervise>,
}

impl Shared {
    pub fn new(init_id: String) -> (Arc<Self>, mpsc::UnboundedReceiver<Supervise>) {
        let (supervisor, commands) = mpsc::unbounded_channel();
        let shared = Arc::new(Self {
            hello: watch::channel(Hello::Pending).0,
            init_id,
            input_written: AtomicBool::new(false),
            turn_active: AtomicBool::new(false),
            interrupting: AtomicBool::new(false),
            usable: AtomicBool::new(true),
            supervisor,
        });
        (shared, commands)
    }

    pub fn is_usable(&self) -> bool {
        self.usable.load(Ordering::SeqCst)
    }

    pub fn turn_active(&self) -> bool {
        self.turn_active.load(Ordering::SeqCst)
    }
}

/// Queues frames for a task that writes them to Claude's stdin, so nothing that handles Claude's
/// output ever waits on its input (a full pipe in both directions would deadlock both sides).
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

/// Owns the process: reaps its group when asked to (or when the grace after an error `result`
/// runs out), and when the process exits by itself stops whatever it left in its group.
pub async fn supervise(
    mut process: Process,
    mut commands: mpsc::UnboundedReceiver<Supervise>,
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
                Some(Supervise::Reap(grace)) => {
                    shared.usable.store(false, Ordering::SeqCst);
                    reap_at = Some(Instant::now() + grace);
                }
                Some(Supervise::Close(ack)) => {
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
