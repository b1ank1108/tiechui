use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tiechui_plugin_api::tools::exec;
use tiechui_plugin_api::{CancellationToken, ToolError};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::{Hook, check, config};

pub(crate) async fn run(
    hook: &Hook,
    input: &str,
    cwd: &Path,
    cancel: CancellationToken,
) -> Result<std::process::Output, ToolError> {
    check(&cancel)?;
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ToolError("Operation aborted".into())),
        result = tokio::time::timeout(Duration::from_secs(hook.timeout), execute(hook, input, cwd, &cancel)) => match result {
            Ok(result) => result,
            Err(_) => Err(ToolError(format!("hook timed out after {}s", hook.timeout))),
        },
    }
}

async fn execute(
    hook: &Hook,
    input: &str,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<std::process::Output, ToolError> {
    if let Some(source) = &hook.plugin {
        let source = source.clone();
        let scan_cancel = cancel.child_token();
        let _guard = scan_cancel.clone().drop_guard();
        let hash = tokio::task::spawn_blocking(move || source.content_hash(&scan_cancel))
            .await
            .map_err(|e| ToolError(e.to_string()))??;
        if hook.hash.split_once('/').map(|(_, hash)| hash) != Some(hash.as_str()) {
            return Err(ToolError(
                "Plugin contents changed; run chui hooks trust".into(),
            ));
        }
    }
    if hook.project || hook.plugin.is_some() {
        let bytes = tokio::fs::read(&hook.path)
            .await
            .map_err(|e| ToolError(e.to_string()))?;
        if !config::verify(hook, &bytes) {
            return Err(ToolError(
                "Project hook changed; run chui hooks trust".into(),
            ));
        }
    }
    #[cfg(unix)]
    let shell =
        std::env::var_os("SHELL").map_or_else(|| "/bin/sh".into(), std::path::PathBuf::from);
    #[cfg(windows)]
    let shell = exec::resolve_shell().map_err(|e| ToolError(e.to_string()))?;
    let mut command = Command::new(shell);
    command
        .arg("-lc")
        .arg(&hook.command)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(source) = &hook.plugin {
        tiechui_plugin_api::private::create_dir_all(source.data_root())?;
        command
            .env("PLUGIN_ROOT", source.root())
            .env("CLAUDE_PLUGIN_ROOT", source.root());
        command
            .env("PLUGIN_DATA", source.data_root())
            .env("CLAUDE_PLUGIN_DATA", source.data_root());
    }
    for name in [
        "CODEX_EXEC_SERVER_NOISE_AUTH_TOKEN",
        "NODE_REPL_AUTH_TOKEN",
        "CODEX_GUARDIAN_DECISIONS_API_KEY",
        "OPENAI_FEDERATION_RULE_ID",
        "OPENAI_IDENTITY_TOKEN_FILE",
        "OPENAI_WORKLOAD_IDENTITY_CONTEXT",
    ] {
        command.env_remove(name);
    }
    // The hook owns a process tree; cancellation must also kill descendants.
    let (mut child, mut tree) =
        exec::Tree::spawn(&mut command).map_err(|e| ToolError(e.to_string()))?;
    let stdin = child.stdin.take();
    let write = async {
        if let Some(mut stdin) = stdin {
            match stdin.write_all(input.as_bytes()).await {
                Err(error) if error.kind() != ErrorKind::BrokenPipe => return Err(error),
                _ => {}
            }
        }
        Ok(())
    };
    let (_, output) =
        tokio::try_join!(write, child.wait_with_output()).map_err(|e| ToolError(e.to_string()))?;
    #[cfg(windows)]
    let output = {
        use std::os::windows::process::ExitStatusExt;
        let mut output = output;
        if let Some(signal) = exec::bash_signal(&output.status) {
            output.status = std::process::ExitStatus::from_raw((128 + signal) as u32);
        }
        output
    };
    #[cfg(windows)]
    tree.disarm()?;
    #[cfg(not(windows))]
    tree.disarm();
    Ok(output)
}
