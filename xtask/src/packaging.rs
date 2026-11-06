use std::ffi::OsString;
use std::path::Path;

use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::Result;

pub async fn run(root: &Path, args: &[OsString], cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err("packaging cancelled".into());
    }
    let python = if cfg!(windows) { "python" } else { "python3" };
    let mut child = Command::new(python)
        .arg("-B")
        .arg(root.join("scripts/release.py"))
        .args(args)
        .current_dir(root)
        .spawn()?;
    let status = tokio::select! {
        status = child.wait() => status?,
        _ = cancel.cancelled() => {
            if let Some(id) = child.id() {
                // Python forwards termination to its process group and removes build exports.
                #[cfg(unix)]
                Command::new("kill").args(["-TERM", &id.to_string()]).status().await?;
                #[cfg(windows)]
                Command::new("taskkill").args(["/T", "/F", "/PID", &id.to_string()]).status().await?;
            }
            child.wait().await?;
            return Err("packaging cancelled".into());
        }
    };
    if status.success() {
        Ok(())
    } else {
        Err(format!("packaging failed ({status})").into())
    }
}
