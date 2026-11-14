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
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
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

/// ConPTY asks where the cursor is before the shell starts and waits for the answer.
const STARTUP_QUERY: &[u8] = b"\x1b[6n";

/// Removes the startup query from the front of the output. `None` means the bytes so
/// far could still be the query; `Some(true)` means it was removed and needs an answer.
fn startup_query(pending: &mut Vec<u8>) -> Option<bool> {
    if pending.len() < STARTUP_QUERY.len() && STARTUP_QUERY.starts_with(pending) {
        return None;
    }
    let found = pending.starts_with(STARTUP_QUERY);
    if found {
        pending.drain(..STARTUP_QUERY.len());
    }
    Some(found)
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
        if let Some(env) = &params.env {
            for (key, value) in env {
                command.env(key, value);
            }
        }
        let child = pair.slave.spawn_command(command)?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let terminal = Arc::new(Self {
            writer: Mutex::new(pair.master.take_writer()?),
            master: Mutex::new(Some(pair.master)),
            child: Mutex::new(child),
            output: Arc::default(),
        });
        let id = params.id.clone();
        let output = terminal.output.clone();
        let weak = Arc::downgrade(&terminal);
        #[cfg(windows)]
        {
            let weak = weak.clone();
            std::thread::spawn(move || {
                while let Some(terminal) = weak.upgrade() {
                    let exited = lock(&terminal.child)
                        .try_wait()
                        .map_or(true, |status| status.is_some());
                    if exited {
                        // ConPTY retains its output pipe until the pseudoconsole closes.
                        let master = lock(&terminal.master).take();
                        drop(master);
                        break;
                    }
                    drop(terminal);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            });
        }
        std::thread::spawn(move || {
            let mut chunk = [0; 8192];
            let mut pending = Vec::new();
            // Clients attach late or more than once, so the server answers the query
            // and keeps it out of the replay; later queries still reach the client.
            let mut startup = cfg!(windows);
            while let Ok(n @ 1..) = reader.read(&mut chunk) {
                pending.extend_from_slice(&chunk[..n]);
                if startup {
                    let Some(found) = startup_query(&mut pending) else {
                        continue;
                    };
                    startup = false;
                    if found && let Some(terminal) = weak.upgrade() {
                        // A new pseudoconsole starts at the top left.
                        let _ = terminal.write(b"\x1b[1;1R");
                    }
                    if pending.is_empty() {
                        continue;
                    }
                }
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
        let master = lock(&self.master);
        if let Some(master) = master.as_ref() {
            master.resize(size(cols, rows))?;
        }
        Ok(())
    }

    pub(crate) fn is_alive(&self) -> bool {
        lock(&self.output).exit.is_none()
    }

    pub(crate) fn kill(&self) {
        let mut child = lock(&self.child);
        if child.kill().is_ok() {
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_leading_cursor_query_is_answered_and_removed() {
        let mut pending = b"\x1b[".to_vec();
        assert_eq!(startup_query(&mut pending), None);
        pending.extend_from_slice(b"6nprompt$ ");
        assert_eq!(startup_query(&mut pending), Some(true));
        assert_eq!(pending, b"prompt$ ");
        for output in [&b"prompt\x1b[6n"[..], b"\x1b[31m", b"\x1b[?25l"] {
            let mut pending = output.to_vec();
            assert_eq!(startup_query(&mut pending), Some(false));
            assert_eq!(pending, output);
        }
    }
}
