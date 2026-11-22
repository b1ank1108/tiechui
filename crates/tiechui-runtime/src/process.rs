use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tiechui_exec::Tree;
use tiechui_protocol::{ItemId, KnownItem};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::backend::{BackendEvent, BackendItem};

/// The longest frame a driver reads; a longer one is reported and skipped.
pub const MAX_FRAME: usize = 32 << 20;
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);
const FLUSH_BYTES: usize = 64 * 1024;

/// Writes `chui <runtime>: <message>` to stderr when `CHUI_<RUNTIME>_DEBUG` is set.
pub fn debug(runtime: &str, message: &str) {
    if std::env::var_os(format!("CHUI_{}_DEBUG", runtime.to_ascii_uppercase())).is_some() {
        eprintln!("chui {runtime}: {message}");
    }
}

/// `default`, or in a `fake-runtime` build the milliseconds in the environment variable
/// `variable`, which tests set to shorten a wait.
pub fn overridable(default: Duration, variable: &str) -> Duration {
    if cfg!(feature = "fake-runtime")
        && let Some(ms) = std::env::var(variable)
            .ok()
            .and_then(|value| value.parse().ok())
    {
        return Duration::from_millis(ms);
    }
    default
}

/// A timeout as error messages show it: `60 s`, `1.5 s`.
pub fn seconds(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{} s", duration.as_secs())
    } else {
        format!("{:.1} s", duration.as_secs_f64())
    }
}

/// A spawned CLI in its own process group (a job object on Windows). Dropping it kills the
/// group; `close` stops it gracefully first.
pub struct Process {
    pub child: Child,
    tree: Tree,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
}

impl Process {
    /// Starts `program` with piped stdio. `path` is the `PATH` it runs with, which locates the
    /// program too when it is a bare name.
    pub fn spawn(
        program: &Path,
        args: &[&str],
        cwd: &Path,
        path: Option<&OsString>,
    ) -> io::Result<Self> {
        Self::spawn_env(program, args, cwd, path, |_| false, &[])
    }

    /// Like `spawn`, but the process does not inherit the variables whose names `strip` accepts.
    pub fn spawn_filtered(
        program: &Path,
        args: &[&str],
        cwd: &Path,
        path: Option<&OsString>,
        strip: impl Fn(&str) -> bool,
    ) -> io::Result<Self> {
        Self::spawn_env(program, args, cwd, path, strip, &[])
    }

    /// Like `spawn_filtered`, and sets `set` in the child's environment.
    pub fn spawn_env(
        program: &Path,
        args: &[&str],
        cwd: &Path,
        path: Option<&OsString>,
        strip: impl Fn(&str) -> bool,
        set: &[(String, String)],
    ) -> io::Result<Self> {
        let mut command = Command::new(program);
        configure_env(&mut command, path, strip, set);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut child, tree) = Tree::spawn(&mut command)?;
        Ok(Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
            tree,
        })
    }

    /// Takes the three piped stdio handles out of a freshly spawned process.
    pub fn take_stdio(
        &mut self,
        peer: &str,
    ) -> Result<(ChildStdin, ChildStdout, ChildStderr), String> {
        match (self.stdin.take(), self.stdout.take(), self.stderr.take()) {
            (Some(stdin), Some(stdout), Some(stderr)) => Ok((stdin, stdout, stderr)),
            _ => Err(format!("{peer} started without piped stdio")),
        }
    }

    /// Stops the whole group (TERM, a grace period, then KILL) and reaps the root.
    pub async fn close(mut self) {
        self.stdin.take();
        let _ = self.tree.terminate(&mut self.child).await;
        let _ = self.child.wait().await;
    }
}

pub(crate) fn configure_env(
    command: &mut Command,
    path: Option<&OsString>,
    strip: impl Fn(&str) -> bool,
    set: &[(String, String)],
) {
    for (name, _) in std::env::vars_os() {
        if should_remove_var(name.to_str(), &strip, set) {
            command.env_remove(name);
        }
    }
    if let Some(path) = path {
        command.env("PATH", path);
    }
    for (key, value) in set {
        command.env(key, value);
    }
}

fn should_remove_var(
    name: Option<&str>,
    strip: &impl Fn(&str) -> bool,
    set: &[(String, String)],
) -> bool {
    let Some(name) = name else {
        return false;
    };
    strip(name) || is_windows_shadow(name, set)
}

fn is_windows_shadow(name: &str, set: &[(String, String)]) -> bool {
    cfg!(windows)
        && set
            .iter()
            .any(|(target, _)| name != target && name.eq_ignore_ascii_case(target))
}

/// One line of a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// The bytes before the `\n`.
    Line(Vec<u8>),
    /// A line longer than the limit, skipped; its length so far to the newline.
    Oversize(usize),
}

/// Reads frames separated by `\n` and nothing else, so a payload containing other line
/// separators stays whole.
pub struct FrameReader<R> {
    reader: BufReader<R>,
    limit: usize,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self::with_limit(reader, MAX_FRAME)
    }

    pub fn with_limit(reader: R, limit: usize) -> Self {
        Self {
            reader: BufReader::new(reader),
            limit,
        }
    }

    /// The next frame, or `None` at the end of the stream. A final line without `\n` is a frame.
    pub async fn next(&mut self) -> io::Result<Option<Frame>> {
        let mut line = Vec::new();
        let mut skipped = 0usize;
        loop {
            let buffer = self.reader.fill_buf().await?;
            if buffer.is_empty() {
                return Ok(end_of_stream_frame(line, skipped));
            }
            let found = buffer.iter().position(|&byte| byte == b'\n');
            let chunk = found.unwrap_or(buffer.len());
            if skipped > 0 || line.len() + chunk > self.limit {
                skipped += line.len() + chunk;
                line.clear();
                line.shrink_to_fit();
            } else {
                line.extend_from_slice(&buffer[..chunk]);
            }
            self.reader.consume(chunk + usize::from(found.is_some()));
            if found.is_some() {
                return Ok(Some(line_frame(line, skipped)));
            }
        }
    }
}

fn line_frame(line: Vec<u8>, skipped: usize) -> Frame {
    if skipped > 0 {
        Frame::Oversize(skipped)
    } else {
        Frame::Line(line)
    }
}

fn end_of_stream_frame(line: Vec<u8>, skipped: usize) -> Option<Frame> {
    if line.is_empty() && skipped == 0 {
        None
    } else {
        Some(line_frame(line, skipped))
    }
}

/// Sends the non-empty frames of a CLI's stdout until it ends, then drops `frames`. An oversize
/// frame is skipped.
pub async fn read_frames(
    runtime: &'static str,
    stdout: impl AsyncRead + Unpin,
    frames: mpsc::Sender<Vec<u8>>,
) {
    let mut reader = FrameReader::new(stdout);
    while let Ok(Some(frame)) = reader.next().await {
        let line = match frame {
            Frame::Line(line) if line.is_empty() => continue,
            Frame::Line(line) => line,
            Frame::Oversize(size) => {
                debug(
                    runtime,
                    &format!("dropped a frame of {size} bytes over the limit"),
                );
                continue;
            }
        };
        if frames.send(line).await.is_err() {
            return;
        }
    }
}

/// The last lines a CLI wrote to stderr, to say why it exited.
#[derive(Clone, Default)]
pub struct StderrTail(Arc<Mutex<VecDeque<String>>>);

impl StderrTail {
    fn push(&self, line: String) {
        let mut tail = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if tail.len() == 8 {
            tail.pop_front();
        }
        tail.push_back(line);
    }

    /// `: <line> | <line>`, or nothing when the CLI wrote nothing.
    pub fn detail(&self) -> String {
        let tail = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if tail.is_empty() {
            String::new()
        } else {
            format!(": {}", Vec::from(tail.clone()).join(" | "))
        }
    }
}

pub async fn read_stderr(runtime: &'static str, stderr: impl AsyncRead + Unpin, tail: StderrTail) {
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
        debug(runtime, &format!("stderr: {line}"));
        tail.push(line);
    }
}

/// Merges streamed text per item so a flood of small deltas becomes a few events.
#[derive(Debug, Default)]
pub struct DeltaCoalescer {
    order: Vec<ItemId>,
    text: HashMap<ItemId, String>,
}

impl DeltaCoalescer {
    pub fn push(&mut self, item: &ItemId, delta: &str) {
        if delta.is_empty() {
            return;
        }
        match self.text.get_mut(item) {
            Some(text) => text.push_str(delta),
            None => {
                self.order.push(item.clone());
                self.text.insert(item.clone(), delta.to_owned());
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn drain(&mut self) -> Vec<(ItemId, String)> {
        let order = std::mem::take(&mut self.order);
        order
            .into_iter()
            .filter_map(|id| self.text.remove(&id).map(|text| (id, text)))
            .collect()
    }
}

/// A translator's events, in order. Streamed text is merged per item and sent after
/// `FLUSH_INTERVAL`, once `FLUSH_BYTES` are merged, or before any other event. Every send
/// returns false once the receiver is gone.
pub struct Outbox {
    events: mpsc::Sender<BackendEvent>,
    deltas: DeltaCoalescer,
    buffered: usize,
    flush_at: Option<Instant>,
}

impl Outbox {
    pub fn new(events: mpsc::Sender<BackendEvent>) -> Self {
        Self {
            events,
            deltas: DeltaCoalescer::default(),
            buffered: 0,
            flush_at: None,
        }
    }

    /// Resolves when merged text is due to be sent; never while there is none.
    pub async fn flush_due(&self) {
        match self.flush_at {
            Some(at) => tokio::time::sleep_until(at).await,
            None => std::future::pending().await,
        }
    }

    pub async fn flush(&mut self) -> bool {
        self.flush_at = None;
        self.buffered = 0;
        for (item_id, delta) in self.deltas.drain() {
            if !self
                .send_now(BackendEvent::ItemDelta { item_id, delta })
                .await
            {
                return false;
            }
        }
        true
    }

    pub async fn send_now(&self, event: BackendEvent) -> bool {
        self.events.send(event).await.is_ok()
    }

    pub async fn emit_after_flush(&mut self, event: BackendEvent) -> bool {
        self.flush().await && self.send_now(event).await
    }

    pub async fn push_delta(&mut self, item: &ItemId, delta: &str) -> bool {
        self.deltas.push(item, delta);
        self.buffered += delta.len();
        if self.flush_at.is_none() && !delta.is_empty() {
            self.flush_at = Some(Instant::now() + FLUSH_INTERVAL);
        }
        self.buffered < FLUSH_BYTES || self.flush().await
    }

    pub async fn complete_root_tool(
        &mut self,
        id: &str,
        name: &str,
        arguments: serde_json::Value,
        output: serde_json::Value,
        is_error: bool,
    ) -> bool {
        let call = ItemId(id.to_owned());
        self.emit_after_flush(BackendEvent::ItemCompleted(BackendItem::root(
            KnownItem::ToolCall {
                id: call.clone(),
                name: name.to_owned(),
                arguments,
            },
        )))
        .await
            && self
                .send_now(BackendEvent::ItemCompleted(BackendItem::root(
                    KnownItem::ToolResult {
                        id: ItemId(format!("{id}:result")),
                        call_id: call,
                        output,
                        is_error,
                    },
                )))
                .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    async fn frames(input: &[u8], limit: usize) -> Vec<Frame> {
        let mut reader = FrameReader::with_limit(input, limit);
        let mut out = Vec::new();
        while let Some(frame) = reader.next().await.unwrap() {
            out.push(frame);
        }
        out
    }

    #[tokio::test]
    async fn frames_split_on_newline_only_and_keep_other_separators() {
        let input = "a\r\u{2028}b\u{85}\n\nlast".as_bytes();
        assert_eq!(
            frames(input, 1024).await,
            vec![
                Frame::Line("a\r\u{2028}b\u{85}".as_bytes().to_vec()),
                Frame::Line(vec![]),
                Frame::Line(b"last".to_vec()),
            ]
        );
    }

    #[tokio::test]
    async fn an_oversize_line_is_reported_and_skipped_without_ending_the_stream() {
        let mut input = vec![b'x'; 50];
        input.extend_from_slice(b"\nok\n");
        input.extend(vec![b'y'; 40]);
        assert_eq!(
            frames(&input, 16).await,
            vec![
                Frame::Oversize(50),
                Frame::Line(b"ok".to_vec()),
                Frame::Oversize(40),
            ]
        );
    }

    #[tokio::test]
    async fn a_line_of_exactly_the_limit_is_a_frame() {
        let mut input = vec![b'x'; 16];
        input.push(b'\n');
        assert_eq!(frames(&input, 16).await, vec![Frame::Line(vec![b'x'; 16])]);
    }

    #[test]
    fn coalescing_merges_per_item_in_first_seen_order() {
        let mut merged = DeltaCoalescer::default();
        let (a, b) = (ItemId("a".into()), ItemId("b".into()));
        merged.push(&a, "he");
        merged.push(&b, "x");
        merged.push(&a, "llo");
        merged.push(&b, "");
        assert!(!merged.is_empty());
        assert_eq!(
            merged.drain(),
            vec![(a, "hello".to_owned()), (b, "x".to_owned())]
        );
        assert!(merged.is_empty() && merged.drain().is_empty());
    }

    #[cfg(unix)]
    async fn spawn_with_grandchild() -> (tempfile::TempDir, Process, i32) {
        use tokio::io::AsyncBufReadExt;
        let dir = tempfile::tempdir().unwrap();
        let mut process = Process::spawn(
            Path::new("/bin/sh"),
            &["-c", "sleep 300 & echo $!; wait"],
            dir.path(),
            None,
        )
        .unwrap();
        let mut lines = tokio::io::BufReader::new(process.stdout.take().unwrap()).lines();
        let grandchild: i32 = lines.next_line().await.unwrap().unwrap().parse().unwrap();
        (dir, process, grandchild)
    }

    #[cfg(unix)]
    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[cfg(unix)]
    async fn assert_gone(pid: i32, what: &str) {
        for _ in 0..50 {
            if !alive(pid) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the grandchild outlived {what}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closing_a_process_reaps_its_grandchildren() {
        let (_dir, process, grandchild) = spawn_with_grandchild().await;
        assert!(alive(grandchild));
        process.close().await;
        assert_gone(grandchild, "the process group").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_process_kills_the_group() {
        let (_dir, process, grandchild) = spawn_with_grandchild().await;
        drop(process);
        assert_gone(grandchild, "the dropped process").await;
    }
}
