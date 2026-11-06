#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use tiechui_exec::{StopReason, execute};
use tokio_util::sync::CancellationToken;

#[test]
fn shell_selection_ignores_shell_environment() {
    for shell in ["/bin/zsh", "/no-such-fish"] {
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "process_fixture", "--nocapture"])
            .env("TIECHUI_PROCESS_FIXTURE", "shell")
            .env(
                "TIECHUI_EXPECTED_SHELL",
                tiechui_exec::resolve_shell().unwrap(),
            )
            .env("SHELL", shell)
            .status()
            .unwrap();
        assert!(status.success());
    }
}

#[tokio::test]
async fn merged_stream_preserves_writes_and_reports_signal_and_binary_bytes() {
    let mut output = Vec::new();
    let result = execute(
        Path::new("/bin/bash"),
        "printf 'one\\n'; printf 'two\\n' >&2; printf '\\377'; kill -USR1 $$",
        Path::new("/"),
        None,
        CancellationToken::new(),
        || {},
        |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(output, b"one\ntwo\n\xff");
    assert_eq!(result.exit_code, None);
    assert_eq!(result.signal, Some(libc::SIGUSR1));
    assert_eq!(result.reason, StopReason::Exited);
}

#[tokio::test]
async fn timeout_and_precancel_are_distinct_and_prompt() {
    let start = tokio::time::Instant::now();
    let result = execute(
        Path::new("/bin/bash"),
        "sleep 30",
        Path::new("/"),
        Some(Duration::from_millis(40)),
        CancellationToken::new(),
        || {},
        |_| Ok(()),
    )
    .await
    .unwrap();
    assert_eq!(result.reason, StopReason::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(3));
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = execute(
        Path::new("/missing-shell"),
        "exit 99",
        Path::new("/"),
        None,
        cancel,
        || {},
        |_| panic!("must not spawn"),
    )
    .await
    .unwrap();
    assert_eq!(result.reason, StopReason::Cancelled);
}

#[tokio::test]
async fn spawn_failure_is_reported() {
    assert!(
        execute(
            Path::new("/no-such-tiechui-shell"),
            "true",
            Path::new("/"),
            None,
            CancellationToken::new(),
            || {},
            |_| Ok(())
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn buffered_output_is_drained_after_shell_exit() {
    let mut output = Vec::new();
    let result = execute(
        Path::new("/bin/bash"),
        "i=0; while [ $i -lt 12 ]; do printf chunk; i=$((i+1)); done; exit 0",
        Path::new("/"),
        Some(Duration::from_secs(3)),
        CancellationToken::new(),
        || {},
        |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(result.reason, StopReason::Exited);
    assert_eq!(output, b"chunk".repeat(12));
}

// A separate invocation of this test binary provides portable Unix process fixtures.
#[test]
fn process_fixture() {
    let Ok(role) = std::env::var("TIECHUI_PROCESS_FIXTURE") else {
        return;
    };
    if role == "shell" {
        assert_eq!(
            tiechui_exec::resolve_shell().unwrap(),
            std::path::PathBuf::from(std::env::var_os("TIECHUI_EXPECTED_SHELL").unwrap())
        );
        return;
    }
    let mode = std::env::var("TIECHUI_PROCESS_MODE").unwrap();
    if role == "grandchild" || (role == "child" && mode != "plain") {
        if mode == "detached" {
            assert!(unsafe { libc::setsid() } > 0);
        }
        if mode == "ignore" {
            assert_ne!(
                unsafe { libc::signal(libc::SIGTERM, libc::SIG_IGN) },
                libc::SIG_ERR
            );
        }
        println!("{role}:{}", std::process::id());
        std::io::stdout().flush().unwrap();
        if mode == "detached" {
            std::fs::write(std::env::var_os("TIECHUI_PROCESS_READY").unwrap(), "ready").unwrap();
            std::thread::sleep(Duration::from_secs(1));
            std::process::exit(0);
        }
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process_fixture", "--nocapture"])
        .env(
            "TIECHUI_PROCESS_FIXTURE",
            if role == "root" {
                "child"
            } else {
                "grandchild"
            },
        )
        .spawn()
        .unwrap();
    println!("{role}:{}", std::process::id());
    std::io::stdout().flush().unwrap();
    if mode == "detached" {
        let ready = std::env::var_os("TIECHUI_PROCESS_READY").unwrap();
        while !Path::new(&ready).exists() {
            std::thread::sleep(Duration::from_millis(1));
        }
        std::process::exit(0);
    }
    child.wait().unwrap();
}

async fn assert_tree_cancelled(mode: &str) {
    let binary = std::env::current_exe().unwrap();
    let command = format!(
        "TIECHUI_PROCESS_FIXTURE=root TIECHUI_PROCESS_MODE={mode} exec '{}' --exact process_fixture --nocapture",
        binary.display()
    );
    let cancel = CancellationToken::new();
    let mut output = String::new();
    let start = tokio::time::Instant::now();
    let mut cancelled_at = None;
    let result = execute(
        Path::new("/bin/bash"),
        &command,
        Path::new("/"),
        Some(Duration::from_secs(5)),
        cancel.clone(),
        || {},
        |bytes| {
            output.push_str(&String::from_utf8_lossy(bytes));
            let target = if mode == "plain" {
                "grandchild:"
            } else {
                "child:"
            };
            if cancelled_at.is_none()
                && output
                    .split_inclusive('\n')
                    .any(|line| line.ends_with('\n') && line.starts_with(target))
            {
                cancelled_at = Some(tokio::time::Instant::now());
                cancel.cancel();
            }
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(result.reason, StopReason::Cancelled, "{output}");
    assert!(start.elapsed() < Duration::from_secs(3));
    assert!(cancelled_at.unwrap().elapsed() >= Duration::from_millis(250));
    let pids: Vec<i32> = output
        .lines()
        .filter_map(|line| {
            ["root:", "child:", "grandchild:"]
                .into_iter()
                .find_map(|prefix| line.strip_prefix(prefix))
                .and_then(|pid| pid.parse().ok())
        })
        .collect();
    assert_eq!(pids.len(), if mode == "plain" { 3 } else { 2 }, "{output}");
    for pid in pids {
        assert_pid_exited(pid).await;
    }
}

async fn assert_pid_exited(pid: i32) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let missing = unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        // Linux may retain killed orphans as zombies until PID 1 reaps them.
        #[cfg(target_os = "linux")]
        let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            stat.rsplit_once(") ")
                .is_some_and(|(_, fields)| fields.starts_with("Z "))
        });
        #[cfg(not(target_os = "linux"))]
        let zombie = false;
        if missing || zombie {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "PID {pid} survived cancellation"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn cancellation_kills_grandchild_in_original_group() {
    assert_tree_cancelled("plain").await;
}

#[tokio::test]
async fn cancellation_kills_child_ignoring_sigterm_after_grace_period() {
    assert_tree_cancelled("ignore").await;
}

#[tokio::test]
async fn detached_descendant_holding_stdio_does_not_stall_completion() {
    let directory = tempfile::tempdir().unwrap();
    let ready = directory.path().join("ready");
    let binary = std::env::current_exe().unwrap();
    let command = format!(
        "TIECHUI_PROCESS_FIXTURE=root TIECHUI_PROCESS_MODE=detached TIECHUI_PROCESS_READY='{}' exec '{}' --exact process_fixture --nocapture",
        ready.display(),
        binary.display()
    );
    let mut output = String::new();
    let start = tokio::time::Instant::now();
    let result = execute(
        Path::new("/bin/bash"),
        &command,
        Path::new("/"),
        Some(Duration::from_secs(3)),
        CancellationToken::new(),
        || {},
        |bytes| {
            output.push_str(&String::from_utf8_lossy(bytes));
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(result.reason, StopReason::Exited);
    assert!(start.elapsed() < Duration::from_millis(500));
    let pid = output
        .lines()
        .find_map(|line| line.strip_prefix("child:"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        0,
        "stdio holder should still be alive"
    );
    assert_pid_exited(pid).await;
}

#[tokio::test]
async fn callback_error_cleans_up_root() {
    let mut pid = None;
    let result = execute(
        Path::new("/bin/bash"),
        "echo $$; sleep 30",
        Path::new("/"),
        None,
        CancellationToken::new(),
        || {},
        |bytes| {
            pid = String::from_utf8_lossy(bytes).trim().parse::<i32>().ok();
            Err(std::io::Error::other("sink failed"))
        },
    )
    .await;
    assert_eq!(result.unwrap_err().to_string(), "sink failed");
    let pid = pid.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
}
