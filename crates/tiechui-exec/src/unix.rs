use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::time::Duration;

use tokio::process::{Child, Command};

pub fn resolve_shell() -> io::Result<PathBuf> {
    Ok(resolve_shell_from(
        Path::new("/bin/bash"),
        std::env::var_os("PATH").as_deref(),
    ))
}

fn resolve_shell_from(bin_bash: &Path, path: Option<&OsStr>) -> PathBuf {
    if bin_bash.exists() {
        return bin_bash.into();
    }
    if let Some(path) = path {
        for directory in std::env::split_paths(path) {
            let candidate = directory.join("bash");
            if candidate.is_file()
                && let Ok(path) = CString::new(candidate.as_os_str().as_bytes())
            {
                // access checks executable permission for the current user, including ACLs.
                if unsafe { libc::access(path.as_ptr(), libc::X_OK) } == 0 {
                    return candidate;
                }
            }
        }
    }
    PathBuf::from("sh")
}

/// One socket carries merged stdout/stderr in kernel write order.
pub(crate) async fn output_pipe() -> io::Result<(tokio::net::UnixStream, OwnedFd)> {
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    Ok((tokio::net::UnixStream::from_std(reader)?, writer.into()))
}

pub(crate) fn signal(status: &ExitStatus) -> Option<i32> {
    status.signal()
}

fn signal_group(group: i32, value: i32) -> io::Result<()> {
    // kill has no memory-safety preconditions. A negative PID addresses the process group.
    if unsafe { libc::kill(-group, value) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(io::Error::new(
            error.kind(),
            format!("signal {value} to process group {group}: {error}"),
        ))
    }
}

// A group holding only zombies can return EPERM; that is not a failure.
fn signal_reaped(group: i32, value: i32) -> io::Result<()> {
    match signal_group(group, value) {
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(()),
        result => result,
    }
}

/// The process group of a spawned child; dropping it while armed kills the group.
pub struct Tree {
    root: i32,
    armed: bool,
}

impl Tree {
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        let child = command.process_group(0).spawn()?;
        let root = child
            .id()
            .ok_or_else(|| io::Error::other("spawned process has no PID"))?
            as i32;
        Ok((child, Self { root, armed: true }))
    }

    /// Blocking variant for synchronous callers outside a runtime.
    pub fn spawn_std(
        command: &mut std::process::Command,
    ) -> io::Result<(std::process::Child, Self)> {
        use std::os::unix::process::CommandExt;
        let child = command.process_group(0).spawn()?;
        let root = child.id() as i32;
        Ok((child, Self { root, armed: true }))
    }

    /// Asks the group to stop.
    pub fn interrupt(&self) -> io::Result<()> {
        signal_reaped(self.root, libc::SIGTERM)
    }

    pub fn kill(&self) -> io::Result<()> {
        signal_reaped(self.root, libc::SIGKILL)
    }

    /// Interrupt, a grace period, then kill.
    pub async fn terminate(&self, child: &mut Child) -> io::Result<()> {
        self.interrupt()?;
        tokio::time::sleep(Duration::from_millis(250)).await;
        // Reap a terminated root before signaling again; zombie-only groups may reject signals.
        child.try_wait()?;
        self.kill()
    }

    /// Call once the root has been reaped and the group must outlive this handle.
    pub fn disarm(&mut self) -> io::Result<()> {
        self.armed = false;
        Ok(())
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        if self.armed {
            let _ = signal_group(self.root, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn shell_resolution_prefers_bin_bash_then_executable_bash_on_path_then_sh() {
        let directory = tempfile::tempdir().unwrap();
        let bin_bash = directory.path().join("bin-bash");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(resolve_shell_from(&bin_bash, Some(&path)), Path::new("sh"));
        assert_eq!(resolve_shell_from(&bin_bash, None), Path::new("sh"));
        for location in [&first, &second] {
            std::fs::write(location.join("bash"), "#!/bin/sh\n").unwrap();
        }
        std::fs::set_permissions(first.join("bash"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        std::fs::set_permissions(second.join("bash"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        assert_eq!(
            resolve_shell_from(&bin_bash, Some(&path)),
            second.join("bash")
        );
        std::fs::set_permissions(first.join("bash"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        assert_eq!(
            resolve_shell_from(&bin_bash, Some(&path)),
            first.join("bash")
        );
        std::fs::write(&bin_bash, "").unwrap();
        assert_eq!(resolve_shell_from(&bin_bash, Some(&path)), bin_bash);
    }
}
