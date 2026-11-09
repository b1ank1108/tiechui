use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::process::{Child, Command};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};

pub const GIT_BASH_ENV: &str = "TIECHUI_GIT_BASH_PATH";

#[link(name = "ntdll")]
unsafe extern "system" {
    // Resumes every thread of a process created with CREATE_SUSPENDED.
    fn NtResumeProcess(process: HANDLE) -> i32;
}

/// Git for Windows bash; WSL's System32 bash runs a different filesystem view and is skipped.
pub fn resolve_shell() -> io::Result<PathBuf> {
    resolve_shell_from(
        std::env::var_os(GIT_BASH_ENV).as_deref(),
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("ProgramFiles").as_deref(),
    )
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Git Bash was not found. Install Git for Windows (https://git-scm.com/downloads/win) or set {GIT_BASH_ENV} to bash.exe"
            ),
        )
    })
}

fn resolve_shell_from(
    explicit: Option<&OsStr>,
    path: Option<&OsStr>,
    program_files: Option<&OsStr>,
) -> Option<PathBuf> {
    if let Some(explicit) = explicit.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(explicit)).filter(|path| path.is_file());
    }
    let directories: Vec<_> = path
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    let on_path = |name: &str| {
        directories
            .iter()
            .map(|directory| directory.join(name))
            .filter(|candidate| candidate.is_file())
            .find(|candidate| !is_wsl_launcher(candidate))
    };
    if let Some(bash) = on_path("bash.exe") {
        return Some(bash);
    }
    let from_git = on_path("git.exe").into_iter().flat_map(|git| {
        let root = git.parent().and_then(Path::parent).map(Path::to_path_buf);
        [
            root.as_ref().map(|root| root.join(r"bin\bash.exe")),
            root.map(|root| root.join(r"usr\bin\bash.exe")),
        ]
    });
    let installed = program_files.map(|root| Path::new(root).join(r"Git\bin\bash.exe"));
    from_git
        .flatten()
        .chain(installed)
        .find(|candidate| candidate.is_file())
}

fn is_wsl_launcher(path: &Path) -> bool {
    let lower = path.to_string_lossy().to_ascii_lowercase();
    lower.contains(r"\system32\") || lower.contains(r"\windowsapps\")
}

/// A named pipe carries merged stdout/stderr; the child gets a synchronous write handle.
pub(crate) async fn output_pipe() -> io::Result<(NamedPipeServer, File)> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = OsString::from(format!(
        r"\\.\pipe\tiechui-exec-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .access_outbound(false)
        .create(&name)?;
    let writer = std::fs::OpenOptions::new().write(true).open(&name)?;
    server.connect().await?;
    Ok((server, writer))
}

/// Decode the signal status returned by Git for Windows Bash to native parents.
pub fn signal(status: &ExitStatus) -> Option<i32> {
    let code = status.code()? as u32;
    let signal = ((code >> 8) & 0x7f) as i32;
    (code != 0 && code & 0xff == 0 && (1..=64).contains(&signal)).then_some(signal)
}

fn job() -> io::Result<OwnedHandle> {
    // SAFETY: null attributes and name create an anonymous job owned by the returned handle.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a fresh, valid handle that nothing else owns.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    job_limits(&job, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)?;
    Ok(job)
}

fn job_limits(job: &OwnedHandle, flags: u32) -> io::Result<()> {
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = flags;
    // SAFETY: the handle and the correctly sized limits structure remain valid for the call.
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The child starts suspended so no descendant can escape before it joins the job.
fn start(job: &OwnedHandle, process: HANDLE) -> io::Result<()> {
    // SAFETY: both handles are valid for the duration of the calls.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } == 0
        || unsafe { NtResumeProcess(process) } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The job object holding a spawned child; dropping it while armed terminates the job.
pub struct Tree {
    job: OwnedHandle,
    armed: bool,
}

impl Tree {
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        let job = job()?;
        let mut child = command
            .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED)
            .spawn()?;
        let process = child
            .raw_handle()
            .ok_or_else(|| io::Error::other("spawned process has no handle"))?;
        if let Err(error) = start(&job, process) {
            let _ = child.start_kill();
            return Err(error);
        }
        Ok((child, Self { job, armed: true }))
    }

    /// Blocking variant for synchronous callers outside a runtime.
    pub fn spawn_std(
        command: &mut std::process::Command,
    ) -> io::Result<(std::process::Child, Self)> {
        use std::os::windows::process::CommandExt;
        let job = job()?;
        let mut child = command
            .creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED)
            .spawn()?;
        if let Err(error) = start(&job, child.as_raw_handle()) {
            let _ = child.kill();
            return Err(error);
        }
        Ok((child, Self { job, armed: true }))
    }

    /// Windows has no graceful group signal; the job is terminated.
    pub fn interrupt(&self) -> io::Result<()> {
        self.kill()
    }

    pub fn kill(&self) -> io::Result<()> {
        // SAFETY: the job handle is owned and valid.
        if unsafe { TerminateJobObject(self.job.as_raw_handle(), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub async fn terminate(&self, _: &mut Child) -> io::Result<()> {
        self.kill()
    }

    /// Call once the root has been reaped and the job must outlive this handle.
    pub fn disarm(&mut self) -> io::Result<()> {
        job_limits(&self.job, 0)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn jobs_terminate_on_owner_exit_until_explicitly_disarmed() {
        use windows_sys::Win32::System::JobObjects::QueryInformationJobObject;

        let mut tree = Tree {
            job: job().unwrap(),
            armed: true,
        };
        let flags = |tree: &Tree| {
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            // SAFETY: the owned handle and output buffer are valid for the query.
            assert_ne!(
                unsafe {
                    QueryInformationJobObject(
                        tree.job.as_raw_handle(),
                        JobObjectExtendedLimitInformation,
                        (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                        std::mem::size_of_val(&limits) as u32,
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            limits.BasicLimitInformation.LimitFlags
        };
        assert_ne!(flags(&tree) & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, 0);
        tree.disarm().unwrap();
        assert_eq!(flags(&tree) & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, 0);
    }

    #[test]
    fn bash_signal_status_keeps_normal_windows_exit_codes() {
        use std::os::windows::process::ExitStatusExt;
        for (code, expected) in [
            (0, None),
            (3, None),
            (255, None),
            (3840, Some(15)),
            (2304, Some(9)),
            (512, Some(2)),
            (64 << 8, Some(64)),
            (65 << 8, None),
            (0x101, None),
            (0xc000013a, None),
        ] {
            assert_eq!(signal(&ExitStatus::from_raw(code)), expected);
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn shell_resolution_prefers_override_then_path_then_git_layout() {
        let root = tempfile::tempdir().unwrap();
        let make = |relative: &str| {
            let path = root.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "").unwrap();
            path
        };
        let git = make(r"Git\cmd\git.exe");
        let git_bash = make(r"Git\bin\bash.exe");
        let wsl = make(r"Windows\System32\bash.exe");
        let path = std::env::join_paths([wsl.parent().unwrap(), git.parent().unwrap()]).unwrap();
        assert_eq!(
            resolve_shell_from(None, Some(&path), None),
            Some(git_bash.clone())
        );
        let custom = make(r"custom\bash.exe");
        let path = std::env::join_paths([
            wsl.parent().unwrap(),
            custom.parent().unwrap(),
            git.parent().unwrap(),
        ])
        .unwrap();
        assert_eq!(
            resolve_shell_from(None, Some(&path), None),
            Some(custom.clone())
        );
        assert_eq!(
            resolve_shell_from(Some(git_bash.as_os_str()), Some(&path), None),
            Some(git_bash.clone())
        );
        assert_eq!(
            resolve_shell_from(Some(OsStr::new(r"C:\missing\bash.exe")), Some(&path), None),
            None
        );
        let programs = root.path().as_os_str();
        assert_eq!(
            resolve_shell_from(None, None, Some(programs)),
            Some(git_bash)
        );
        assert_eq!(resolve_shell_from(None, None, None), None);
    }
}
