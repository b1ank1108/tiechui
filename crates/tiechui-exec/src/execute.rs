use std::io;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::sys;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Exited,
    Cancelled,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcessResult {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: StopReason,
}

// The idle grace preserves active output without waiting forever for inherited stdio.
const POST_EXIT_IDLE: Duration = Duration::from_millis(100);

async fn drain_output(
    reader: &mut (impl AsyncRead + Unpin),
    on_data: &mut (impl FnMut(&[u8]) -> io::Result<()> + Send),
) -> io::Result<()> {
    let mut buffer = [0; 8192];
    loop {
        tokio::select! {
            biased;
            // A delayed executor may see both ready data and an expired timer.
            // Buffered output must win; only a pending read counts as idle.
            result = reader.read(&mut buffer) => {
                let length = result?;
                if length == 0 { return Ok(()); }
                on_data(&buffer[..length])?;
            }
            _ = tokio::time::sleep(POST_EXIT_IDLE) => return Ok(()),
        }
    }
}

/// Stream merged stdout/stderr in kernel write order. The callback must not block.
/// After shell exit, drain until EOF or 100 ms idle, resetting on each read.
/// Buffered output takes precedence over idle expiry; later descendant output is not guaranteed.
/// Cancellation and the command timeout remain active during the drain.
/// `on_spawn` runs once the process exists, before any output.
pub async fn execute(
    shell: &Path,
    command_text: &str,
    cwd: &Path,
    timeout: Option<Duration>,
    cancel: CancellationToken,
    on_spawn: impl FnOnce() + Send,
    mut on_data: impl FnMut(&[u8]) -> io::Result<()> + Send,
) -> io::Result<ProcessResult> {
    if cancel.is_cancelled() {
        return Ok(ProcessResult {
            exit_code: None,
            signal: None,
            reason: StopReason::Cancelled,
        });
    }
    let (mut reader, writer) = sys::output_pipe().await?;
    let mut command = Command::new(shell);
    command
        .arg("-c")
        .arg(command_text)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer.try_clone()?))
        .stderr(Stdio::from(writer));
    let (mut child, mut tree) = sys::Tree::spawn(&mut command)?;
    on_spawn();
    // Release the parent's copies of the write end so EOF follows the last writer.
    drop(command);
    let deadline = timeout.map(|duration| Instant::now() + duration);
    let timer = async {
        match deadline {
            Some(at) => tokio::time::sleep_until(at).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(timer);
    let mut buffer = [0; 8192];
    let mut eof = false;
    let mut reason = StopReason::Exited;
    let status = loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => { reason = StopReason::Cancelled; break None; }
            _ = &mut timer => { reason = StopReason::TimedOut; break None; }
            status = child.wait() => { break Some(status?); }
            result = reader.read(&mut buffer), if !eof => {
                let length = result?;
                if length == 0 { eof = true; } else { on_data(&buffer[..length])?; }
            }
        }
    };
    let status = match status {
        Some(status) => status,
        None => {
            tree.terminate(&mut child).await?;
            child.wait().await?
        }
    };
    if !eof {
        let drain = drain_output(&mut reader, &mut on_data);
        tokio::pin!(drain);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled(), if reason == StopReason::Exited => {
                    reason = StopReason::Cancelled;
                    tree.terminate(&mut child).await?;
                }
                _ = &mut timer, if reason == StopReason::Exited => {
                    reason = StopReason::TimedOut;
                    tree.terminate(&mut child).await?;
                }
                result = &mut drain => { result?; break; }
            }
        }
    }
    tree.disarm();
    let signal = sys::signal(&status);
    Ok(ProcessResult {
        exit_code: if signal.is_some() {
            None
        } else {
            status.code()
        },
        signal,
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::task::{Context, Waker};
    use tokio::io::AsyncWriteExt;

    #[tokio::test(start_paused = true)]
    async fn post_exit_output_is_drained_until_idle() {
        let (mut reader, mut writer) = tokio::io::duplex(4096);
        let mut output = Vec::new();
        let mut context = Context::from_waker(Waker::noop());
        {
            let mut on_data = |bytes: &[u8]| {
                output.extend_from_slice(bytes);
                Ok(())
            };
            let drain = drain_output(&mut reader, &mut on_data);
            tokio::pin!(drain);
            assert!(drain.as_mut().poll(&mut context).is_pending());
            for _ in 0..12 {
                // Virtual time covers 20 ms gaps over more than one idle window.
                tokio::time::advance(Duration::from_millis(20)).await;
                writer.write_all(b"chunk").await.unwrap();
                assert!(drain.as_mut().poll(&mut context).is_pending());
            }
            tokio::time::advance(Duration::from_millis(99)).await;
            assert!(drain.as_mut().poll(&mut context).is_pending());
            tokio::time::advance(Duration::from_millis(2)).await;
            assert!(matches!(
                drain.as_mut().poll(&mut context),
                std::task::Poll::Ready(Ok(()))
            ));
        }
        assert_eq!(output, b"chunk".repeat(12));
        // The writer is still open: completion was idle expiry, not EOF.
        drop(writer);
    }

    #[tokio::test(start_paused = true)]
    async fn post_exit_buffered_output_wins_over_expired_idle_timer() {
        let (mut reader, mut writer) = tokio::io::duplex(4096);
        let mut output = Vec::new();
        let mut context = Context::from_waker(Waker::noop());
        {
            let mut on_data = |bytes: &[u8]| {
                output.extend_from_slice(bytes);
                Ok(())
            };
            let drain = drain_output(&mut reader, &mut on_data);
            tokio::pin!(drain);
            assert!(drain.as_mut().poll(&mut context).is_pending());
            writer.write_all(b"buffered").await.unwrap();
            // Simulate the executor being descheduled until both branches are ready.
            tokio::time::advance(POST_EXIT_IDLE + Duration::from_millis(1)).await;
            assert!(drain.as_mut().poll(&mut context).is_pending());
            drop(writer);
            assert!(matches!(
                drain.as_mut().poll(&mut context),
                std::task::Poll::Ready(Ok(()))
            ));
        }
        assert_eq!(output, b"buffered");
    }
}
