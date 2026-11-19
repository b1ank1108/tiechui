use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use tiechui_config::ProjectPostCreate;
use tiechui_protocol::workspace::{PostCreateResult, PostCreateStatus};

use crate::lock;

const DEFAULT_POST_CREATE_TIMEOUT: Duration = Duration::from_secs(60);
const POST_CREATE_OUTPUT_CAP: usize = 64 * 1024;
const PIPE_DRAIN_GRACE: Duration = Duration::from_millis(100);

pub(crate) fn run_project_post_create(
    home: &Path,
    project_root: &Path,
    wt_path: &Path,
    timeout: Option<Duration>,
    env: &BTreeMap<String, String>,
) -> PostCreateResult {
    match tiechui_config::project_post_create_script(home, project_root) {
        Ok(ProjectPostCreate::None) => PostCreateResult::default(),
        Ok(ProjectPostCreate::Untrusted(script)) => PostCreateResult {
            status: PostCreateStatus::SkippedUntrusted,
            command: Some(script),
            exit_code: None,
            output: String::new(),
            reason: Some(
                "project is not trusted; trust this project to run its post-create script".into(),
            ),
            truncated: false,
        },
        Ok(ProjectPostCreate::Invalid(err)) => {
            failed_without_run(None, format!("invalid project configuration: {err}"))
        }
        Err(err) => failed_without_run(None, err.to_string()),
        Ok(ProjectPostCreate::Trusted(script)) => execute_script(
            &script,
            wt_path,
            timeout.unwrap_or(DEFAULT_POST_CREATE_TIMEOUT),
            env,
        ),
    }
}

fn failed_without_run(command: Option<String>, reason: String) -> PostCreateResult {
    PostCreateResult {
        status: PostCreateStatus::Failed,
        command,
        exit_code: None,
        output: String::new(),
        reason: Some(reason),
        truncated: false,
    }
}

fn execute_script(
    script: &str,
    wt_path: &Path,
    timeout: Duration,
    env: &BTreeMap<String, String>,
) -> PostCreateResult {
    let (reader, writer) = match std::io::pipe() {
        Ok(pair) => pair,
        Err(err) => {
            return failed_without_run(
                Some(script.to_owned()),
                format!("failed to open output pipe: {err}"),
            );
        }
    };
    let writer_err = match writer.try_clone() {
        Ok(cloned) => cloned,
        Err(err) => {
            return failed_without_run(
                Some(script.to_owned()),
                format!("failed to clone output pipe: {err}"),
            );
        }
    };
    let mut cmd = match shell_command(script, wt_path, env) {
        Ok(cmd) => cmd,
        Err(err) => {
            return failed_without_run(
                Some(script.to_owned()),
                format!("failed to resolve post-create shell: {err}"),
            );
        }
    };
    cmd.stdout(Stdio::from(writer))
        .stderr(Stdio::from(writer_err));
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return failed_without_run(
                Some(script.to_owned()),
                format!("failed to spawn post-create script: {err}"),
            );
        }
    };
    drop(cmd);
    let collector = OutputCollector::spawn(reader);
    let outcome = wait_for_child(&mut child, timeout);
    let (output, truncated) = collector.finish(PIPE_DRAIN_GRACE);
    build_script_result(script, outcome, timeout, output, truncated)
}

fn shell_command(
    script: &str,
    wt_path: &Path,
    env: &BTreeMap<String, String>,
) -> std::io::Result<Command> {
    #[cfg(unix)]
    let mut cmd = {
        use std::os::unix::process::CommandExt;
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script]).process_group(0);
        c
    };
    #[cfg(windows)]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        let bash = tiechui_exec::resolve_git_bash_with_env(env)?;
        let mut c = Command::new(bash);
        c.args(["-lc", script])
            .creation_flags(CREATE_NO_WINDOW)
            .env("CHERE_INVOKING", "1");
        c
    };
    cmd.current_dir(wt_path).stdin(Stdio::null()).envs(env);
    #[cfg(windows)]
    if let Some(path_val) = crate::git::find_env_var(env, "PATH") {
        cmd.env("PATH", path_val);
    }
    for name in tiechui_config::scrubbed_secret_env_vars() {
        cmd.env_remove(name);
    }
    Ok(cmd)
}

pub(crate) enum WaitOutcome {
    Exited(ExitStatus),
    TimedOut,
    Cancelled,
    Failed(std::io::Error),
}

fn wait_for_child(child: &mut Child, timeout: Duration) -> WaitOutcome {
    wait_for_child_until(child, Instant::now() + timeout, None)
}

pub(crate) fn wait_for_child_until(
    child: &mut Child,
    deadline: Instant,
    is_cancelled: Option<&dyn Fn() -> bool>,
) -> WaitOutcome {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return WaitOutcome::Exited(status),
            Ok(None) if is_cancelled.is_some_and(|cancelled| cancelled()) => {
                terminate_process_group(child);
                return WaitOutcome::Cancelled;
            }
            Ok(None) if Instant::now() >= deadline => {
                terminate_process_group(child);
                return WaitOutcome::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(err) => return WaitOutcome::Failed(err),
        }
    }
}

fn terminate_process_group(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        let pgid = -(pid as i32);
        // SAFETY: `child` was spawned with `process_group(0)`, so `-pid` addresses its own group.
        unsafe {
            kill(pgid, 15);
        }
        std::thread::sleep(Duration::from_millis(30));
        if matches!(child.try_wait(), Ok(None)) {
            unsafe {
                kill(pgid, 9);
            }
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.wait();
}

fn build_script_result(
    script: &str,
    outcome: WaitOutcome,
    timeout: Duration,
    output: String,
    truncated: bool,
) -> PostCreateResult {
    let command = Some(script.to_owned());
    match outcome {
        WaitOutcome::Exited(status) if status.success() => PostCreateResult {
            status: PostCreateStatus::Succeeded,
            command,
            exit_code: status.code().or(Some(0)),
            output,
            reason: None,
            truncated,
        },
        WaitOutcome::Exited(status) => {
            let exit_code = status.code();
            let reason = match exit_code {
                Some(code) => format!("post-create script exited with status {code}"),
                None => "post-create script terminated by signal".into(),
            };
            PostCreateResult {
                status: PostCreateStatus::Failed,
                command,
                exit_code,
                output,
                reason: Some(reason),
                truncated,
            }
        }
        WaitOutcome::TimedOut | WaitOutcome::Cancelled => PostCreateResult {
            status: PostCreateStatus::Failed,
            command,
            exit_code: None,
            output,
            reason: Some(format!(
                "post-create script timed out after {}ms",
                timeout.as_millis()
            )),
            truncated,
        },
        WaitOutcome::Failed(err) => PostCreateResult {
            status: PostCreateStatus::Failed,
            command,
            exit_code: None,
            output,
            reason: Some(format!("failed waiting on post-create script: {err}")),
            truncated,
        },
    }
}

#[derive(Default)]
struct CapturedBytes {
    bytes: Vec<u8>,
    truncated: bool,
    eof: bool,
}

struct OutputCollector {
    shared: Arc<(Mutex<CapturedBytes>, Condvar)>,
}

impl OutputCollector {
    fn spawn(mut reader: std::io::PipeReader) -> Self {
        let shared = Arc::new((Mutex::new(CapturedBytes::default()), Condvar::new()));
        let pair = shared.clone();
        std::thread::spawn(move || {
            let mut chunk = [0_u8; 4096];
            while let Ok(read) = reader.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                append_capped(&pair.0, &chunk[..read]);
            }
            lock(&pair.0).eof = true;
            pair.1.notify_all();
        });
        Self { shared }
    }

    fn finish(self, grace: Duration) -> (String, bool) {
        let guard = lock(&self.shared.0);
        let (guard, _) = self
            .shared
            .1
            .wait_timeout_while(guard, grace, |state| !state.eof)
            .unwrap_or_else(|e| e.into_inner());
        (
            String::from_utf8_lossy(&guard.bytes).into_owned(),
            guard.truncated,
        )
    }
}

fn append_capped(mutex: &Mutex<CapturedBytes>, incoming: &[u8]) {
    let mut state = lock(mutex);
    let remaining = POST_CREATE_OUTPUT_CAP.saturating_sub(state.bytes.len());
    let keep = remaining.min(incoming.len());
    if keep > 0 {
        state.bytes.extend_from_slice(&incoming[..keep]);
    }
    if keep < incoming.len() {
        state.truncated = true;
    }
}
