use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::json;
use tiechui_protocol::workspace::{
    TerminalAttached, TerminalCreateParams, TerminalExit, TerminalOutput,
};

use crate::{Result, Sink, lock};

const KEEP: usize = 64 << 10;

#[derive(Default)]
struct Output {
    buffer: VecDeque<u8>,
    /// Bytes sent so far.
    seq: u64,
    exit: Option<i32>,
}

pub(crate) struct Terminal {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    output: Arc<Mutex<Output>>,
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// The longest prefix that is whole UTF-8, leaving a split character for the next read.
fn complete(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => bytes.len(),
    }
}

impl Terminal {
    /// Starts the user's login shell.
    pub(crate) fn spawn(params: &TerminalCreateParams, sink: Sink) -> Result<Arc<Self>> {
        let pair = native_pty_system().openpty(size(params.cols, params.rows))?;
        let mut command = CommandBuilder::new_default_prog();
        command.cwd(&params.cwd);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let terminal = Arc::new(Self {
            writer: Mutex::new(pair.master.take_writer()?),
            master: Mutex::new(pair.master),
            child: Mutex::new(child),
            output: Arc::default(),
        });
        let id = params.id.clone();
        let output = terminal.output.clone();
        let weak = Arc::downgrade(&terminal);
        std::thread::spawn(move || {
            let mut chunk = [0; 8192];
            let mut pending = Vec::new();
            while let Ok(n @ 1..) = reader.read(&mut chunk) {
                pending.extend_from_slice(&chunk[..n]);
                let end = complete(&pending);
                let data = String::from_utf8_lossy(&pending[..end]).into_owned();
                pending.drain(..end);
                let mut output = lock(&output);
                output.buffer.extend(data.as_bytes());
                let excess = output.buffer.len().saturating_sub(KEEP);
                output.buffer.drain(..excess);
                output.seq += data.len() as u64;
                // Sent under the lock, so `attached` falls between two notifications.
                sink(
                    "terminal/output",
                    json!(TerminalOutput {
                        id: id.clone(),
                        seq: output.seq,
                        data,
                    }),
                );
            }
            let code = weak.upgrade().map_or(0, |terminal| {
                lock(&terminal.child)
                    .wait()
                    .map_or(-1, |status| status.exit_code() as i32)
            });
            lock(&output).exit = Some(code);
            sink("terminal/exit", json!(TerminalExit { id, code }));
        });
        Ok(terminal)
    }

    pub(crate) fn attached(&self) -> TerminalAttached {
        let output = lock(&self.output);
        let bytes: Vec<u8> = output.buffer.iter().copied().collect();
        // The kept bytes may start inside a character.
        let start = bytes
            .iter()
            .position(|byte| byte & 0xC0 != 0x80)
            .unwrap_or(bytes.len());
        TerminalAttached {
            buffer: String::from_utf8_lossy(&bytes[start..]).into_owned(),
            seq: output.seq,
            exit: output.exit,
        }
    }

    pub(crate) fn write(&self, data: &[u8]) -> Result<()> {
        let mut writer = lock(&self.writer);
        writer.write_all(data)?;
        Ok(writer.flush()?)
    }

    pub(crate) fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        Ok(lock(&self.master).resize(size(cols, rows))?)
    }

    pub(crate) fn kill(&self) {
        let mut child = lock(&self.child);
        if child.kill().is_ok() {
            let _ = child.wait();
        }
    }
}
