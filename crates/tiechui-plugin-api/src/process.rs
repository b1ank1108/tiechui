//! Shared stdio process ownership for external module clients.
use crate::{CancellationToken, ToolError};
use std::{collections::BTreeMap, path::Path, process::Stdio, time::Duration};
use tokio::process::{Child, Command};

use rmcp::{
    Peer, RoleClient,
    model::{ClientRequest, ProgressNotificationParam, RequestId},
    service::PeerRequestOptions,
};
use std::sync::Arc;

pub type RequestProgress = (
    tokio::sync::broadcast::Receiver<ProgressNotificationParam>,
    Arc<dyn Fn(String) + Send + Sync>,
);

struct PendingRequest(Option<(Peer<RoleClient>, RequestId)>);
impl Drop for PendingRequest {
    fn drop(&mut self) {
        if let Some((peer, id)) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = peer
                    .notify_cancelled(rmcp::model::CancelledNotificationParam::new(
                        Some(id),
                        Some("Call cancelled".into()),
                    ))
                    .await;
            });
        }
    }
}

/// MCP and T2 share request cancellation and rmcp's progress-reset inactivity timer.
pub async fn request(
    peer: &Peer<RoleClient>,
    request: ClientRequest,
    timeout: Duration,
    cancel: CancellationToken,
    session: CancellationToken,
    mut progress: Option<RequestProgress>,
) -> Result<serde_json::Value, ToolError> {
    let error = |e: rmcp::ServiceError| ToolError(e.to_string());
    let operation = async {
        let handle = peer
            .send_request_with_option(
                request,
                PeerRequestOptions::with_timeout(timeout).reset_timeout_on_progress(),
            )
            .await
            .map_err(error)?;
        let mut pending = PendingRequest(Some((peer.clone(), handle.id.clone())));
        let token = handle.progress_token.clone();
        let response = handle.await_response();
        tokio::pin!(response);
        loop {
            tokio::select! {
                biased;
                update = async { match &mut progress { Some((rx, _)) => rx.recv().await, None => std::future::pending().await } } => {
                    if matches!(update, Err(tokio::sync::broadcast::error::RecvError::Closed)) { progress = None; }
                    if let Ok(update) = update && update.progress_token == token && let Some((_, callback)) = &progress {
                        callback(update.message.unwrap_or_else(|| update.progress.to_string()));
                    }
                }
                result = &mut response => {
                    let result = result.map_err(error)?;
                    pending.0 = None;
                    return serde_json::to_value(result).map_err(|e| ToolError(e.to_string()));
                }
            }
        }
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ToolError("Call cancelled".into())),
        _ = session.cancelled() => Err(ToolError("Session stopped".into())),
        result = operation => result,
    }
}

pub struct ProcessGroup {
    pub child: Child,
    _tree: tiechui_tools::exec::Tree,
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl ProcessGroup {
    pub fn spawn(
        command: &Path,
        args: &[String],
        cwd: &Path,
        env_vars: &[String],
        env: &BTreeMap<String, String>,
    ) -> Result<Self, ToolError> {
        let spawn = |command: &Path| {
            let mut process = Command::new(command);
            process
                .args(args)
                .current_dir(cwd)
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true);
            // Only a small default environment plus explicitly requested variables pass through.
            for key in [
                "HOME",
                "USER",
                "LOGNAME",
                "PATH",
                "SHELL",
                "TMPDIR",
                "TEMP",
                "TMP",
                "SYSTEMROOT",
                "WINDIR",
            ]
            .into_iter()
            .chain(env_vars.iter().map(String::as_str))
            {
                if let Some(value) = std::env::var_os(key) {
                    process.env(key, value);
                }
            }
            process.envs(env);
            tiechui_tools::exec::Tree::spawn(&mut process)
        };
        let spawned = spawn(command);
        #[cfg(windows)]
        let spawned = match spawned {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match batch_command(command, env) {
                    Some(command) => spawn(&command),
                    None => Err(error),
                }
            }
            result => result,
        };
        let (child, tree) = spawned?;
        Ok(Self { child, _tree: tree })
    }
}

#[cfg(windows)]
fn batch_command(command: &Path, env: &BTreeMap<String, String>) -> Option<std::path::PathBuf> {
    if command.components().count() != 1 || command.extension().is_some() {
        return None;
    }
    let path = env
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| std::ffi::OsString::from(value))
        .or_else(|| std::env::var_os("PATH"))?;
    for directory in std::env::split_paths(&path) {
        for extension in ["cmd", "bat"] {
            let candidate = directory.join(command).with_extension(extension);
            if candidate.is_file() {
                return std::path::absolute(candidate).ok();
            }
        }
    }
    None
}

pub fn seconds(value: Option<f64>, default: f64) -> Result<Duration, ToolError> {
    let value = value.unwrap_or(default);
    if value <= 0.0 {
        return Err(ToolError("Timeouts must be positive seconds".into()));
    }
    Duration::try_from_secs_f64(value).map_err(|_| ToolError("Invalid timeout".into()))
}

pub async fn deadline<T>(
    timeout: Duration,
    cancel: CancellationToken,
    operation: impl std::future::Future<Output = Result<T, ToolError>>,
) -> Result<T, ToolError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ToolError("Request cancelled".into())),
        result = tokio::time::timeout(timeout, operation) => result.map_err(|_| ToolError("Request timed out".into()))?,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[cfg(windows)]
    #[tokio::test]
    async fn bare_batch_commands_use_child_path_and_keep_std_argument_escaping() {
        use tokio::io::AsyncReadExt;

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("batch files");
        std::fs::create_dir(&directory).unwrap();
        let env = BTreeMap::from([("Path".into(), directory.display().to_string())]);
        for extension in ["cmd", "bat"] {
            let script = directory.join(format!("tiechui-fixture.{extension}"));
            std::fs::write(
                &script,
                "@echo off\r\n@echo %1\r\n@if defined APPDATA echo leaked\r\n",
            )
            .unwrap();
            let mut group = ProcessGroup::spawn(
                Path::new("tiechui-fixture"),
                &["hello & goodbye".into()],
                temp.path(),
                &[],
                &env,
            )
            .unwrap();
            drop(group.child.stdin.take());
            let mut output = String::new();
            group
                .child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut output)
                .await
                .unwrap();
            assert!(group.child.wait().await.unwrap().success());
            assert_eq!(output, "\"hello & goodbye\"\r\n");
            assert!(!temp.path().join("goodbye").exists());
            std::fs::remove_file(script).unwrap();
        }
        assert!(batch_command(Path::new("./tiechui-fixture"), &env).is_none());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn native_executables_take_precedence_over_batch_fallbacks() {
        let temp = tempfile::tempdir().unwrap();
        let command = temp.path().join("tiechui-fixture");
        std::fs::copy(
            Path::new(&std::env::var_os("SYSTEMROOT").unwrap()).join("System32/cmd.exe"),
            command.with_extension("exe"),
        )
        .unwrap();
        std::fs::write(command.with_extension("cmd"), "@exit /b 42\r\n").unwrap();
        let env = BTreeMap::from([("PATH".into(), temp.path().display().to_string())]);
        let mut group = ProcessGroup::spawn(
            Path::new("tiechui-fixture"),
            &["/D".into(), "/C".into(), "exit /b 23".into()],
            temp.path(),
            &[],
            &env,
        )
        .unwrap();
        assert_eq!(group.child.wait().await.unwrap().code(), Some(23));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_process_group_preserves_unrelated_child_in_callers_group() {
        let mut unrelated = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let group = ProcessGroup::spawn(
            Path::new("/bin/sleep"),
            &["30".into()],
            Path::new("/"),
            &[],
            &BTreeMap::new(),
        )
        .unwrap();
        let parent_group = unsafe { libc::getpgrp() };
        let id = group.child.id().unwrap() as i32;
        assert_eq!(unsafe { libc::getpgid(id) }, id);
        assert_ne!(id, parent_group);
        assert_eq!(
            unsafe { libc::getpgid(unrelated.id().unwrap() as i32) },
            parent_group
        );
        drop(group);
        unrelated
            .stdin
            .take()
            .unwrap()
            .write_all(b"alive\n")
            .await
            .unwrap();
        let output = tokio::time::timeout(Duration::from_secs(5), unrelated.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"alive\n");
    }

    #[tokio::test]
    async fn progress_extends_inactivity_and_cancel_notifies_the_peer() {
        let (client, server) = tokio::io::duplex(4096);
        let cancel = CancellationToken::new();
        let service = rmcp::service::serve_directly_with_ct((), client, None, cancel.clone());
        let server = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut lines = BufReader::new(read).lines();
            let request: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            for n in 0..5 {
                tokio::time::sleep(Duration::from_millis(30)).await;
                write.write_all(format!("{}\n", json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":request["params"]["_meta"]["progressToken"],"progress":n}})).as_bytes()).await.unwrap();
            }
            write
                .write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":request["id"],"result":{"done":true}})
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let request: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let cancelled: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(cancelled["method"], "notifications/cancelled");
            assert_eq!(cancelled["params"]["requestId"], request["id"]);
        });
        let request_value = || {
            ClientRequest::CustomRequest(rmcp::model::CustomRequest::new("test", Some(json!({}))))
        };
        assert_eq!(
            request(
                service.peer(),
                request_value(),
                Duration::from_millis(100),
                CancellationToken::new(),
                cancel.clone(),
                None
            )
            .await
            .unwrap(),
            json!({"done":true})
        );
        let interrupted = CancellationToken::new();
        let stop = interrupted.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            stop.cancel();
        });
        assert!(
            request(
                service.peer(),
                request_value(),
                Duration::from_secs(1),
                interrupted,
                cancel.clone(),
                None
            )
            .await
            .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        cancel.cancel();
    }
}
