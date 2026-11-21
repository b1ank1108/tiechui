//! What drivers need to run an external CLI: a process group that is reaped as a whole, frames
//! split on `\n` only, and merged text deltas.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::Stdio;

use tiechui_exec::Tree;
use tiechui_protocol::ItemId;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

/// The longest frame a driver reads; a longer one is reported and skipped.
pub const MAX_FRAME: usize = 32 << 20;

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
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(path) = path {
            command.env("PATH", path);
        }
        let (mut child, tree) = Tree::spawn(&mut command)?;
        Ok(Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
            tree,
        })
    }

    /// Stops the whole group (TERM, a grace period, then KILL) and reaps the root.
    pub async fn close(mut self) {
        self.stdin.take();
        let _ = self.tree.terminate(&mut self.child).await;
        let _ = self.child.wait().await;
    }
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
                return Ok(match (line.is_empty(), skipped) {
                    (true, 0) => None,
                    (_, 0) => Some(Frame::Line(line)),
                    (_, skipped) => Some(Frame::Oversize(skipped)),
                });
            }
            let (chunk, found) = match buffer.iter().position(|&byte| byte == b'\n') {
                Some(index) => (index, true),
                None => (buffer.len(), false),
            };
            if skipped > 0 || line.len() + chunk > self.limit {
                skipped += line.len() + chunk;
                line.clear();
                line.shrink_to_fit();
            } else {
                line.extend_from_slice(&buffer[..chunk]);
            }
            self.reader.consume(chunk + usize::from(found));
            if found {
                return Ok(Some(if skipped > 0 {
                    Frame::Oversize(skipped)
                } else {
                    Frame::Line(line)
                }));
            }
        }
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

    /// The merged deltas in the order their items first appeared. Call it before emitting
    /// anything else, so text is never reordered behind the event that followed it.
    pub fn drain(&mut self) -> Vec<(ItemId, String)> {
        let order = std::mem::take(&mut self.order);
        order
            .into_iter()
            .filter_map(|id| self.text.remove(&id).map(|text| (id, text)))
            .collect()
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
    #[tokio::test]
    async fn closing_a_process_reaps_its_grandchildren() {
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
        // SAFETY: signal 0 only checks that the process exists.
        let alive = |pid| unsafe { libc::kill(pid, 0) == 0 };
        assert!(alive(grandchild));
        process.close().await;
        for _ in 0..50 {
            if !alive(grandchild) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the grandchild outlived the process group");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_a_process_kills_the_group() {
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
        drop(process);
        // SAFETY: signal 0 only checks that the process exists.
        let alive = |pid| unsafe { libc::kill(pid, 0) == 0 };
        for _ in 0..50 {
            if !alive(grandchild) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("the grandchild outlived the dropped process");
    }
}
