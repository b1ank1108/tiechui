#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tiechui_protocol::workspace::*;
use tiechui_workspace as ws;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout).unwrap()
}

fn write(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A repository on `main` with one commit of `a.txt` and `src/lib.rs`.
fn repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "Test"),
        ("user.email", "test@example.com"),
        ("commit.gpgsign", "false"),
        ("core.autocrlf", "false"),
    ] {
        git(root, &["config", key, value]);
    }
    write(root, ".gitignore", "target/\n");
    write(root, "a.txt", "one\ntwo\n");
    write(root, "src/lib.rs", "fn main() {}\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "first"]);
    dir
}

fn changes(root: &Path) -> Vec<(String, GitFileStatus, bool, u32, u32)> {
    ws::status_with_env(root, &BTreeMap::new())
        .unwrap()
        .unwrap()
        .changes
        .into_iter()
        .map(|c| (c.path, c.status, c.staged, c.added, c.removed))
        .collect()
}

#[test]
fn tree_marks_ignored_entries_without_descending() {
    let dir = repo();
    let root = dir.path();
    write(root, "target/debug/out", "x");
    let tree = ws::tree(root).unwrap();
    let entries: Vec<_> = tree
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.dir, e.ignored))
        .collect();
    assert_eq!(
        entries,
        [
            (".gitignore", false, false),
            ("a.txt", false, false),
            ("src", true, false),
            ("src/lib.rs", false, false),
            ("target", true, true),
        ]
    );
    assert!(!tree.truncated);
    let inside: Vec<_> = ws::list(root, "target")
        .unwrap()
        .into_iter()
        .map(|e| (e.path, e.dir, e.ignored))
        .collect();
    assert_eq!(inside, [("target/debug".to_owned(), true, true)]);
    assert!(ws::list(root, "../").is_err());
}

#[test]
fn writes_refuse_stale_hashes() {
    let dir = repo();
    let root = dir.path();
    let read = ws::read(root, "a.txt").unwrap();
    assert_eq!(read.text.as_deref(), Some("one\ntwo\n"));
    let written = ws::write(root, "a.txt", "three\n", Some(&read.hash)).unwrap();
    assert!(ws::write(root, "a.txt", "four\n", Some(&read.hash)).is_err());
    assert!(ws::write(root, "new.txt", "x", Some("")).is_ok());
    assert!(ws::write(root, "new.txt", "y", Some("")).is_err());
    assert_eq!(ws::read(root, "a.txt").unwrap().hash, written.hash);
    write(root, "bin", "a\0b");
    assert_eq!(ws::read(root, "bin").unwrap().text, None);
    ws::rename(root, "new.txt", "src/new.txt").unwrap();
    assert!(ws::rename(root, "src/new.txt", "a.txt").is_err());
}

#[test]
fn search_reports_utf16_ranges_and_filters_paths() {
    let dir = repo();
    let root = dir.path();
    write(root, "src/text.rs", "é hello Hello\nnone\n");
    write(root, "docs/hello.md", "hello\n");
    let search = |params: SearchParams| {
        let result = ws::search(
            root,
            &SearchParams {
                root: String::new(),
                ..params
            },
        )
        .unwrap();
        result
            .groups
            .into_iter()
            .map(|g| {
                (
                    g.path,
                    g.hits.into_iter().map(|h| (h.line, h.ranges)).collect(),
                )
            })
            .collect::<Vec<(String, Vec<_>)>>()
    };
    let query = |query: &str| SearchParams {
        query: query.into(),
        ..Default::default()
    };
    assert_eq!(
        search(SearchParams {
            include: vec!["src".into()],
            ..query("hello")
        }),
        [("src/text.rs".to_owned(), vec![(1, vec![(2, 7), (8, 13)])])]
    );
    assert_eq!(
        search(SearchParams {
            case_sensitive: true,
            exclude: vec!["*.md".into()],
            ..query("Hello")
        }),
        [("src/text.rs".to_owned(), vec![(1, vec![(8, 13)])])]
    );
    assert_eq!(
        search(SearchParams {
            regex: true,
            whole_word: true,
            include: vec!["docs/".into()],
            ..query("hel+o")
        }),
        [("docs/hello.md".to_owned(), vec![(1, vec![(0, 5)])])]
    );
    assert!(
        ws::search(
            root,
            &SearchParams {
                regex: true,
                ..query("(")
            }
        )
        .is_err()
    );
}

#[test]
fn search_caps_hits_within_a_single_file() {
    let dir = repo();
    let root = dir.path();
    write(root, "big.txt", &"needle\n".repeat(3000));
    let result = ws::search(
        root,
        &SearchParams {
            query: "needle".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!((result.total, result.truncated), (2000, true));
    assert_eq!(result.groups[0].hits.len(), 2000);
}

#[test]
fn status_splits_staged_and_unstaged_changes() {
    let dir = repo();
    let root = dir.path();
    write(root, "a.txt", "one\nTWO\nthree\n");
    git(root, &["add", "a.txt"]);
    write(root, "a.txt", "one\nTWO\nthree\nfour\n");
    write(root, "src/new.rs", "a\nb\n");
    git(root, &["mv", "src/lib.rs", "src/main.rs"]);
    let env = BTreeMap::new();
    let status = ws::status_with_env(root, &env).unwrap().unwrap();
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert!(status.head.is_some() && status.upstream.is_none() && status.operation.is_none());
    assert_eq!(
        changes(root),
        [
            ("a.txt".into(), GitFileStatus::Modified, true, 2, 1),
            ("a.txt".into(), GitFileStatus::Modified, false, 1, 0),
            ("src/main.rs".into(), GitFileStatus::Renamed, true, 0, 0),
            ("src/new.rs".into(), GitFileStatus::Untracked, false, 2, 0),
        ]
    );
    // A project inside the repository sees its own paths.
    assert_eq!(
        changes(&root.join("src")),
        [
            ("main.rs".into(), GitFileStatus::Renamed, true, 0, 0),
            ("new.rs".into(), GitFileStatus::Untracked, false, 2, 0),
        ]
    );
    assert!(
        ws::status_with_env(tempfile::tempdir().unwrap().path(), &env)
            .unwrap()
            .is_none()
    );

    ws::run_with_env(
        root,
        &GitOp::Unstage {
            paths: vec!["a.txt".into()],
        },
        &env,
    )
    .unwrap();
    ws::run_with_env(
        root,
        &GitOp::Discard {
            paths: vec!["a.txt".into(), "src/new.rs".into()],
        },
        &env,
    )
    .unwrap();
    assert_eq!(
        changes(root),
        [("src/main.rs".into(), GitFileStatus::Renamed, true, 0, 0)]
    );
    assert!(!root.join("src/new.rs").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "one\ntwo\n"
    );
}

#[test]
fn history_compare_and_show() {
    let dir = repo();
    let root = dir.path();
    let env = BTreeMap::new();
    ws::run_with_env(
        root,
        &GitOp::Checkout {
            branch: "feature".into(),
            create: true,
        },
        &env,
    )
    .unwrap();
    write(root, "a.txt", "one\n");
    write(root, "b.txt", "b\n");
    ws::run_with_env(
        root,
        &GitOp::Stage {
            paths: vec![".".into()],
        },
        &env,
    )
    .unwrap();
    ws::run_with_env(
        root,
        &GitOp::Commit {
            message: "second\n\nbody".into(),
        },
        &env,
    )
    .unwrap();
    let log = ws::log_with_env(root, 10, &env).unwrap();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].subject, "second");
    assert_eq!(log[0].refs, ["HEAD -> feature"]);
    assert_eq!(log[0].parents, [log[1].hash.clone()]);
    let files: Vec<_> = log[0]
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.status, f.added, f.removed))
        .collect();
    assert_eq!(
        files,
        [
            ("a.txt", GitFileStatus::Modified, 0, 1),
            ("b.txt", GitFileStatus::Added, 1, 0),
        ]
    );
    assert_eq!(log[1].refs, ["main"]);
    assert_eq!(log[1].files.len(), 3);
    let comparison = ws::compare_with_env(root, "main", &env).unwrap();
    assert_eq!(comparison.merge_base.as_deref(), Some(log[1].hash.as_str()));
    assert_eq!((comparison.ahead, comparison.behind), (1, 0));
    assert_eq!(comparison.files.len(), 2);
    assert_eq!(
        ws::compare_with_env(root, "missing", &env)
            .unwrap()
            .merge_base,
        None
    );
    assert_eq!(
        ws::show_with_env(root, "main", "a.txt", &env)
            .unwrap()
            .as_deref(),
        Some("one\ntwo\n")
    );
    assert_eq!(
        ws::show_with_env(root, "main", "b.txt", &env).unwrap(),
        None
    );
    let branches = ws::branches_with_env(root, &env).unwrap();
    assert_eq!(branches.local, ["feature", "main"]);
    assert!(ws::run_with_env(root, &GitOp::Abort, &env).is_err());
}

#[test]
fn terminals_replay_output_after_attach() {
    let dir = tempfile::tempdir().unwrap();
    let notices = Arc::new(Mutex::new(Vec::<(&str, Value)>::new()));
    let sink = notices.clone();
    let workspace = ws::Workspace::new(Arc::new(move |method, params| {
        sink.lock().unwrap().push((method, params));
    }));
    let create = TerminalCreateParams {
        id: "t".into(),
        cwd: dir.path().display().to_string(),
        cols: 80,
        rows: 24,
        env: None,
    };
    workspace.create_terminal(create.clone()).unwrap();
    assert!(workspace.create_terminal(create).is_err());
    let initial = workspace.attach_terminal("t").unwrap();
    let launched_shell = initial
        .shell
        .as_deref()
        .expect("attached terminal reports shell name");
    assert!(!launched_shell.is_empty());
    let command = match launched_shell {
        "pwsh" | "powershell" => "Write-Output \"tiechui-$(40 + 2)\"\r\n",
        "cmd" => "set /a tc=40+2\r\necho tiechui-%tc%\r\n",
        _ if cfg!(windows) => "echo tiechui-$((40 + 2))\r\n",
        _ => "echo tiechui-$((40 + 2))\n",
    };
    workspace.write_terminal("t", command).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    #[cfg(windows)]
    let mut cursor_reported = false;
    loop {
        let attached = workspace.attach_terminal("t").unwrap();
        if attached.buffer.contains("tiechui-42") {
            break;
        }
        #[cfg(windows)]
        if !cursor_reported && attached.buffer.contains("\x1b[6n") {
            workspace.write_terminal("t", "\x1b[1;1R").unwrap();
            cursor_reported = true;
        }
        assert!(Instant::now() < deadline, "no terminal output");
        std::thread::sleep(Duration::from_millis(50));
    }
    let attached = workspace.attach_terminal("t").unwrap();
    let last = notices
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, _)| *method == "terminal/output")
        .map(|(_, params)| params["seq"].as_u64().unwrap())
        .max();
    assert!(last <= Some(attached.seq) && attached.exit.is_none());
    workspace
        .resize_terminal(&TerminalResizeParams {
            id: "t".into(),
            cols: 100,
            rows: 30,
        })
        .unwrap();
    workspace
        .write_terminal(
            "t",
            if cfg!(windows) {
                "exit 3\r\n"
            } else {
                "exit 3\n"
            },
        )
        .unwrap();
    while workspace.attach_terminal("t").unwrap().exit.is_none() {
        assert!(Instant::now() < deadline, "the shell did not exit");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(workspace.attach_terminal("t").unwrap().exit, Some(3));
    workspace
        .resize_terminal(&TerminalResizeParams {
            id: "t".into(),
            cols: 120,
            rows: 40,
        })
        .unwrap();
    workspace.kill_terminal("t");
    assert!(workspace.attach_terminal("t").is_err());
}

#[test]
fn terminal_shells_and_home_config_switch_shell_for_new_terminals() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let work = temp.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work).unwrap();

    let workspace = ws::Workspace::with_home(home.clone(), Arc::new(|_, _| {}));
    let discovered = workspace.terminal_shells();
    assert!(!discovered.shells.is_empty());
    assert!(
        discovered
            .shells
            .iter()
            .any(|entry| entry.id == discovered.default_shell),
        "default_shell {} missing from {:?}",
        discovered.default_shell,
        discovered.shells
    );

    let explicit_id = if cfg!(windows) { "cmd" } else { "sh" };
    std::fs::write(
        home.join("config.toml"),
        format!("[terminal]\nshell = \"{explicit_id}\"\n"),
    )
    .unwrap();
    workspace
        .create_terminal(TerminalCreateParams {
            id: "t-explicit".into(),
            cwd: work.display().to_string(),
            cols: 80,
            rows: 24,
            env: None,
        })
        .unwrap();
    assert_eq!(
        workspace.attach_terminal("t-explicit").unwrap().shell,
        Some(explicit_id.into())
    );
    workspace.kill_terminal("t-explicit");

    std::fs::write(
        home.join("config.toml"),
        "[terminal]\nshell = \"/nonexistent/missing-shell-binary\"\n",
    )
    .unwrap();
    workspace
        .create_terminal(TerminalCreateParams {
            id: "t-fallback".into(),
            cwd: work.display().to_string(),
            cols: 80,
            rows: 24,
            env: None,
        })
        .unwrap();
    assert_eq!(
        workspace.attach_terminal("t-fallback").unwrap().shell,
        Some(discovered.default_shell)
    );
    workspace.kill_terminal("t-fallback");
    workspace.close();
}

#[test]
fn watches_report_files_and_git_state() {
    let dir = repo();
    let root = dir.path().canonicalize().unwrap();
    let notices = Arc::new(Mutex::new(Vec::<(&str, Value)>::new()));
    let sink = notices.clone();
    let workspace = ws::Workspace::new(Arc::new(move |method, params| {
        sink.lock().unwrap().push((method, params));
    }));
    workspace.watch(&root).unwrap();
    // Events from before the watch may still arrive; wait for the ones this test causes.
    let wait = |matches: &dyn Fn(&str, &Value) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !notices.lock().unwrap().iter().any(|(m, p)| matches(m, p)) {
            assert!(Instant::now() < deadline, "no notification");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    write(&root, "src/watched.rs", "x");
    write(&root, "target/ignored", "x");
    wait(&|method, params| {
        method == "fs/changed"
            && params["paths"]
                .as_array()
                .unwrap()
                .contains(&"src/watched.rs".into())
    });
    git(&root, &["add", "src/watched.rs"]);
    wait(&|method, _| method == "git/changed");
    assert!(notices.lock().unwrap().iter().all(|(method, params)| {
        params["root"] == root.display().to_string()
            && (*method != "fs/changed"
                || params["paths"].as_array().unwrap().iter().all(|p| {
                    !p.as_str().unwrap().starts_with("target")
                        && !p.as_str().unwrap().starts_with(".git")
                }))
    }));
    workspace.unwatch(&root);
}

#[test]
#[ignore]
fn tcp_listener_helper() {
    use std::io::Write;
    use std::net::TcpListener;

    #[cfg(unix)]
    if std::env::var_os("TC_TEST_IGNORE_SIGTERM").is_some() {
        const SIGTERM: i32 = 15;
        const SIG_IGN: usize = 1;
        unsafe extern "C" {
            fn signal(sig: i32, handler: usize) -> usize;
        }
        unsafe {
            signal(SIGTERM, SIG_IGN);
        }
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "LISTEN_PORT={port}").unwrap();
    stdout.flush().unwrap();
    drop(stdout);

    for stream in listener.incoming() {
        let _ = stream;
    }
}

/// Each Windows scan runs netstat and a PowerShell process query, far slower than lsof or /proc.
const SCAN_SLACK: Duration = if cfg!(windows) {
    Duration::from_secs(3)
} else {
    Duration::ZERO
};

struct SpawnedListener {
    pid: u32,
    port: u16,
    exited: Arc<std::sync::atomic::AtomicBool>,
}

impl SpawnedListener {
    fn start(cwd: &Path, extra_arg: Option<&str>, ignore_sigterm: bool) -> Self {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.current_dir(cwd)
            .args(["--ignored", "--nocapture", "tcp_listener_helper"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(arg) = extra_arg {
            cmd.arg(arg);
        }
        if ignore_sigterm {
            cmd.env("TC_TEST_IGNORE_SIGTERM", "1");
        }
        let mut child = cmd.spawn().unwrap();
        let pid = child.id();
        let stdout = child.stdout.take().unwrap();
        let port: u16 = BufReader::new(stdout)
            .lines()
            .find_map(|line| {
                line.ok()?
                    .trim()
                    .strip_prefix("LISTEN_PORT=")
                    .and_then(|p| p.parse().ok())
            })
            .expect("helper did not print LISTEN_PORT");
        let exited = Arc::new(AtomicBool::new(false));
        let exited_flag = exited.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            exited_flag.store(true, Ordering::SeqCst);
        });
        Self { pid, port, exited }
    }

    fn is_exited(&self) -> bool {
        self.exited.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn stop_now(&self) {
        if self.is_exited() {
            return;
        }
        #[cfg(unix)]
        {
            unsafe extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            unsafe {
                kill(self.pid as i32, 9);
            }
        }
        #[cfg(windows)]
        {
            let _ = Command::new("taskkill")
                .args(["/PID", &self.pid.to_string(), "/T", "/F"])
                .output();
        }
    }
}

impl Drop for SpawnedListener {
    fn drop(&mut self) {
        self.stop_now();
    }
}

#[test]
fn ports_attribute_by_longest_prefix_respect_boundaries_and_symlinks() {
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("repo");
    let nested = repo.join("packages/web");
    let sibling = base.path().join("repo-extra");
    let outside = base.path().join("outside");
    let real_target = base.path().join("real target");
    for dir in [&repo, &nested, &sibling, &outside, &real_target] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let listener_parent = SpawnedListener::start(&repo, None, false);
    let listener_nested = SpawnedListener::start(&nested, None, false);
    let nested_script = nested.join("server.js").display().to_string();
    let listener_cmdline = SpawnedListener::start(&outside, Some(&nested_script), false);
    let listener_sibling = SpawnedListener::start(&sibling, None, false);

    let workspace = ws::Workspace::new(Arc::new(|_, _| {}));
    let repo_str = repo.display().to_string();
    let nested_str = nested.display().to_string();
    let ports = workspace
        .list_ports(&[repo_str.clone(), nested_str.clone()])
        .unwrap();

    let relevant: Vec<_> = ports
        .iter()
        .filter(|e| {
            [
                listener_parent.pid,
                listener_nested.pid,
                listener_cmdline.pid,
                listener_sibling.pid,
            ]
            .contains(&e.pid)
        })
        .map(|e| (e.root.as_str(), e.port, e.pid, !e.process.is_empty()))
        .collect();
    let mut expected = vec![(
        nested_str.as_str(),
        listener_cmdline.port,
        listener_cmdline.pid,
        true,
    )];
    // Windows does not expose another process's working directory, so only the command line counts.
    #[cfg(not(windows))]
    expected.extend([
        (
            repo_str.as_str(),
            listener_parent.port,
            listener_parent.pid,
            true,
        ),
        (
            nested_str.as_str(),
            listener_nested.port,
            listener_nested.pid,
            true,
        ),
    ]);
    expected.sort_by_key(|&(root, port, pid, _)| (root, port, pid));
    assert_eq!(relevant, expected);

    #[cfg(unix)]
    {
        let link = base.path().join("linked-project");
        std::os::unix::fs::symlink(&real_target, &link).unwrap();
        let listener_real = SpawnedListener::start(&real_target, None, false);
        let link_str = link.display().to_string();
        let linked_ports = workspace
            .list_ports(std::slice::from_ref(&link_str))
            .unwrap();
        assert!(
            linked_ports.iter().any(|e| e.root == link_str
                && e.pid == listener_real.pid
                && e.port == listener_real.port),
            "expected symlinked root attribution in {linked_ports:?}"
        );
    }
    workspace.close();
}

#[test]
fn ports_notify_changes_and_stop_verifies_ownership_and_rescans() {
    let base = tempfile::tempdir().unwrap();
    let repo = base.path().join("project-a");
    let other_repo = base.path().join("project-b");
    let outside = base.path().join("outside");
    for dir in [&repo, &other_repo, &outside] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let notices = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    let sink = notices.clone();
    let workspace = ws::Workspace::new(Arc::new(move |method, params| {
        sink.lock().unwrap().push((method.to_owned(), params));
    }));

    let repo_str = repo.display().to_string();
    let other_str = other_repo.display().to_string();
    let initial = workspace
        .list_ports(&[repo_str.clone(), other_str.clone()])
        .unwrap();
    assert!(initial.is_empty(), "unexpected initial ports: {initial:?}");

    let unrelated = SpawnedListener::start(&outside, None, false);
    let owned_script = repo.join("server.js").display().to_string();
    let owned = SpawnedListener::start(&repo, Some(&owned_script), false);

    let appear_deadline = Instant::now() + Duration::from_secs(3) + SCAN_SLACK;
    loop {
        let got = notices.lock().unwrap().iter().any(|(method, params)| {
            method == "ports/changed"
                && serde_json::from_value::<PortsChanged>(params.clone())
                    .map(|p| {
                        p.ports.iter().any(|e| {
                            e.root == repo_str && e.pid == owned.pid && e.port == owned.port
                        }) && p.ports.iter().all(|e| e.pid != unrelated.pid)
                    })
                    .unwrap_or(false)
        });
        if got {
            break;
        }
        assert!(
            Instant::now() < appear_deadline,
            "ports/changed did not report spawned listener in time: {:?}",
            *notices.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(40));
    }

    assert!(
        workspace
            .stop_port(&PortStopParams {
                root: repo_str.clone(),
                pid: std::process::id(),
                port: None,
            })
            .is_err()
    );
    assert!(
        workspace
            .stop_port(&PortStopParams {
                root: repo_str.clone(),
                pid: unrelated.pid,
                port: Some(unrelated.port),
            })
            .is_err()
    );
    assert!(!unrelated.is_exited());
    assert!(
        workspace
            .stop_port(&PortStopParams {
                root: other_str,
                pid: owned.pid,
                port: Some(owned.port),
            })
            .is_err()
    );
    assert!(!owned.is_exited());

    notices.lock().unwrap().clear();
    let stop_started = Instant::now();
    workspace
        .stop_port(&PortStopParams {
            root: repo_str.clone(),
            pid: owned.pid,
            port: Some(owned.port),
        })
        .unwrap();

    let stop_deadline = stop_started + Duration::from_secs(1) + SCAN_SLACK;
    loop {
        let cleared = notices.lock().unwrap().iter().any(|(method, params)| {
            method == "ports/changed"
                && serde_json::from_value::<PortsChanged>(params.clone())
                    .map(|p| p.ports.iter().all(|e| e.pid != owned.pid))
                    .unwrap_or(false)
        });
        if cleared && owned.is_exited() {
            break;
        }
        assert!(
            Instant::now() < stop_deadline,
            "ports/changed did not converge in time after stop: {:?}",
            *notices.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(std::net::TcpListener::bind(("127.0.0.1", owned.port)).is_ok());

    #[cfg(unix)]
    {
        let stubborn = SpawnedListener::start(&repo, None, true);
        let wait_stubborn = Instant::now() + Duration::from_secs(3);
        while !workspace
            .list_ports(std::slice::from_ref(&repo_str))
            .unwrap()
            .iter()
            .any(|e| e.pid == stubborn.pid)
        {
            assert!(Instant::now() < wait_stubborn);
            std::thread::sleep(Duration::from_millis(30));
        }
        notices.lock().unwrap().clear();
        let started = Instant::now();
        workspace
            .stop_port(&PortStopParams {
                root: repo_str,
                pid: stubborn.pid,
                port: Some(stubborn.port),
            })
            .unwrap();
        let deadline = started + Duration::from_secs(1);
        while !stubborn.is_exited() {
            assert!(
                Instant::now() < deadline,
                "stubborn listener was not SIGKILLed"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    workspace.close();
}

fn parse_zip_entries(zip_bytes: &[u8]) -> BTreeMap<String, (bool, Vec<u8>)> {
    use std::io::Read;

    assert!(zip_bytes.len() >= 22, "zip too short: {}", zip_bytes.len());
    let eocd_pos = (0..=zip_bytes.len() - 22)
        .rev()
        .find(|&i| zip_bytes[i..i + 4] == [0x50, 0x4b, 0x05, 0x06])
        .expect("missing ZIP EOCD signature");
    let total_entries =
        u16::from_le_bytes(zip_bytes[eocd_pos + 10..eocd_pos + 12].try_into().unwrap()) as usize;
    let cd_size =
        u32::from_le_bytes(zip_bytes[eocd_pos + 12..eocd_pos + 16].try_into().unwrap()) as usize;
    let cd_offset =
        u32::from_le_bytes(zip_bytes[eocd_pos + 16..eocd_pos + 20].try_into().unwrap()) as usize;
    assert_eq!(cd_offset + cd_size, eocd_pos);

    let mut out = BTreeMap::new();
    let mut cursor = cd_offset;
    for _ in 0..total_entries {
        assert_eq!(&zip_bytes[cursor..cursor + 4], &[0x50, 0x4b, 0x01, 0x02]);
        let method = u16::from_le_bytes(zip_bytes[cursor + 10..cursor + 12].try_into().unwrap());
        let expected_crc =
            u32::from_le_bytes(zip_bytes[cursor + 16..cursor + 20].try_into().unwrap());
        let comp_size =
            u32::from_le_bytes(zip_bytes[cursor + 20..cursor + 24].try_into().unwrap()) as usize;
        let uncomp_size =
            u32::from_le_bytes(zip_bytes[cursor + 24..cursor + 28].try_into().unwrap()) as usize;
        let name_len =
            u16::from_le_bytes(zip_bytes[cursor + 28..cursor + 30].try_into().unwrap()) as usize;
        let extra_len =
            u16::from_le_bytes(zip_bytes[cursor + 30..cursor + 32].try_into().unwrap()) as usize;
        let comment_len =
            u16::from_le_bytes(zip_bytes[cursor + 32..cursor + 34].try_into().unwrap()) as usize;
        let local_offset =
            u32::from_le_bytes(zip_bytes[cursor + 42..cursor + 46].try_into().unwrap()) as usize;
        let name = std::str::from_utf8(&zip_bytes[cursor + 46..cursor + 46 + name_len])
            .unwrap()
            .to_owned();
        cursor += 46 + name_len + extra_len + comment_len;

        assert_eq!(
            &zip_bytes[local_offset..local_offset + 4],
            &[0x50, 0x4b, 0x03, 0x04]
        );
        let l_name_len = u16::from_le_bytes(
            zip_bytes[local_offset + 26..local_offset + 28]
                .try_into()
                .unwrap(),
        ) as usize;
        let l_extra_len = u16::from_le_bytes(
            zip_bytes[local_offset + 28..local_offset + 30]
                .try_into()
                .unwrap(),
        ) as usize;
        let data_start = local_offset + 30 + l_name_len + l_extra_len;
        let raw_slice = &zip_bytes[data_start..data_start + comp_size];

        let is_dir = name.ends_with('/');
        let decoded = match method {
            0 => raw_slice.to_vec(),
            8 => {
                let mut decoder = flate2::read::DeflateDecoder::new(raw_slice);
                let mut buf = Vec::with_capacity(uncomp_size);
                decoder.read_to_end(&mut buf).unwrap();
                buf
            }
            other => panic!("unsupported zip method {other} for {name}"),
        };
        assert_eq!(decoded.len(), uncomp_size, "size mismatch for {name}");
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(&decoded);
        assert_eq!(hasher.finalize(), expected_crc, "crc32 mismatch for {name}");
        out.insert(name, (is_dir, decoded));
    }
    out
}

#[test]
fn chunked_binary_upload_download_zip_archive_conflicts_and_cleanup() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use sha2::{Digest, Sha256};

    let dir = repo();
    let root = dir.path();
    let root_str = root.display().to_string();
    let workspace = ws::Workspace::new(Arc::new(|_, _| {}));

    let total_len = 13 * 1024 * 1024 + 321;
    let mut big_binary = vec![0_u8; total_len];
    for (idx, byte) in big_binary.iter_mut().enumerate() {
        *byte = match idx % 7 {
            0 => 0x00,
            1 => 0xff,
            2 => 0xfe,
            3 => 0x80,
            _ => ((idx.wrapping_mul(131) ^ (idx >> 9)) & 0xff) as u8,
        };
    }
    let expected_sha = format!("{:x}", Sha256::digest(&big_binary));

    let stats = ws::stat_many(
        root,
        &[
            "a.txt".into(),
            "src".into(),
            "uploads/deep/large.bin".into(),
        ],
    )
    .unwrap();
    assert_eq!(
        stats,
        vec![
            FileStat {
                path: "a.txt".into(),
                exists: true,
                dir: false,
                size: 8,
            },
            FileStat {
                path: "src".into(),
                exists: true,
                dir: true,
                size: 0,
            },
            FileStat {
                path: "uploads/deep/large.bin".into(),
                exists: false,
                dir: false,
                size: 0,
            },
        ]
    );
    assert!(ws::stat_many(root, &["../escape.txt".into()]).is_err());

    let started = workspace
        .upload_start(
            1,
            &UploadStartParams {
                root: root_str.clone(),
                path: "uploads/deep/large.bin".into(),
                overwrite: false,
                size: Some(total_len as u64),
            },
        )
        .unwrap();
    for chunk in big_binary.chunks(512 * 1024) {
        workspace
            .upload_write(
                1,
                &UploadWriteParams {
                    transfer_id: started.transfer_id.clone(),
                    data: STANDARD.encode(chunk),
                },
            )
            .unwrap();
    }
    let finished = workspace.upload_finish(1, &started.transfer_id).unwrap();
    assert_eq!(finished.path, "uploads/deep/large.bin");
    assert_eq!(finished.size, total_len as u64);
    assert_eq!(finished.sha256, expected_sha);
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(std::fs::read(root.join("uploads/deep/large.bin")).unwrap())
        ),
        expected_sha
    );

    let conflict_err = workspace
        .upload_start(
            1,
            &UploadStartParams {
                root: root_str.clone(),
                path: "uploads/deep/large.bin".into(),
                overwrite: false,
                size: Some(4),
            },
        )
        .unwrap_err()
        .0;
    assert!(
        conflict_err.contains("exists"),
        "expected conflict error, got: {conflict_err}"
    );

    let aborted_overwrite = workspace
        .upload_start(
            1,
            &UploadStartParams {
                root: root_str.clone(),
                path: "uploads/deep/large.bin".into(),
                overwrite: true,
                size: None,
            },
        )
        .unwrap();
    workspace
        .upload_write(
            1,
            &UploadWriteParams {
                transfer_id: aborted_overwrite.transfer_id.clone(),
                data: STANDARD.encode(b"partial-garbage"),
            },
        )
        .unwrap();
    workspace
        .upload_abort(1, &aborted_overwrite.transfer_id)
        .unwrap();
    assert_eq!(
        std::fs::metadata(root.join("uploads/deep/large.bin"))
            .unwrap()
            .len(),
        total_len as u64,
        "aborted overwrite must preserve original file"
    );
    let deep_entries: Vec<_> = std::fs::read_dir(root.join("uploads/deep"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(deep_entries, vec!["large.bin".to_owned()]);

    let aborted_fresh = workspace
        .upload_start(
            1,
            &UploadStartParams {
                root: root_str.clone(),
                path: "aborted-dir/nested/half.bin".into(),
                overwrite: false,
                size: None,
            },
        )
        .unwrap();
    workspace
        .upload_write(
            1,
            &UploadWriteParams {
                transfer_id: aborted_fresh.transfer_id.clone(),
                data: STANDARD.encode(b"half"),
            },
        )
        .unwrap();
    workspace
        .upload_abort(1, &aborted_fresh.transfer_id)
        .unwrap();
    assert!(
        !root.join("aborted-dir").exists(),
        "aborted upload should prune newly created empty parent directories"
    );

    let dl_file = workspace
        .download_start(1, &root_str, "uploads/deep/large.bin")
        .unwrap();
    assert_eq!(dl_file.name, "large.bin");
    assert!(!dl_file.archive);
    assert_eq!(dl_file.total_bytes, total_len as u64);
    let mut downloaded_bytes = Vec::with_capacity(total_len);
    loop {
        let chunk = workspace
            .download_read(
                1,
                &DownloadReadParams {
                    transfer_id: dl_file.transfer_id.clone(),
                    chunk_size: Some(600_000),
                },
            )
            .unwrap();
        let raw = STANDARD.decode(&chunk.data).unwrap();
        assert_eq!(raw.len(), chunk.bytes_read as usize);
        downloaded_bytes.extend_from_slice(&raw);
        if chunk.eof {
            break;
        }
    }
    assert_eq!(downloaded_bytes.len(), total_len);
    assert_eq!(
        format!("{:x}", Sha256::digest(&downloaded_bytes)),
        expected_sha
    );
    assert!(
        workspace
            .download_read(
                1,
                &DownloadReadParams {
                    transfer_id: dl_file.transfer_id,
                    chunk_size: None,
                },
            )
            .is_err()
    );

    std::fs::create_dir_all(root.join("bundle/empty-sub")).unwrap();
    std::fs::create_dir_all(root.join("bundle/inner")).unwrap();
    std::fs::write(root.join("bundle/inner/数据.bin"), b"\x00\xffzip-payload\n").unwrap();
    std::fs::create_dir_all(root.join(".tiechui/worktrees/wt-skip")).unwrap();
    std::fs::write(
        root.join(".tiechui/worktrees/wt-skip/secret.txt"),
        "do-not-zip\n",
    )
    .unwrap();

    let dl_dir = workspace.download_start(1, &root_str, "bundle").unwrap();
    assert_eq!(dl_dir.name, "bundle.zip");
    assert!(dl_dir.archive);
    assert!(dl_dir.total_bytes > 22);
    let mut zip_buf = Vec::new();
    loop {
        let chunk = workspace
            .download_read(
                1,
                &DownloadReadParams {
                    transfer_id: dl_dir.transfer_id.clone(),
                    chunk_size: Some(4096),
                },
            )
            .unwrap();
        zip_buf.extend_from_slice(&STANDARD.decode(&chunk.data).unwrap());
        if chunk.eof {
            break;
        }
    }
    assert_eq!(zip_buf.len() as u64, dl_dir.total_bytes);
    let bundle_entries = parse_zip_entries(&zip_buf);
    assert_eq!(bundle_entries.get("empty-sub/"), Some(&(true, Vec::new())));
    assert_eq!(
        bundle_entries.get("inner/数据.bin"),
        Some(&(false, b"\x00\xffzip-payload\n".to_vec()))
    );

    let dl_root = workspace.download_start(1, &root_str, "").unwrap();
    assert!(dl_root.archive);
    assert!(dl_root.name.ends_with(".zip"));
    let mut root_zip = Vec::new();
    loop {
        let chunk = workspace
            .download_read(
                1,
                &DownloadReadParams {
                    transfer_id: dl_root.transfer_id.clone(),
                    chunk_size: Some(1024 * 1024),
                },
            )
            .unwrap();
        root_zip.extend_from_slice(&STANDARD.decode(&chunk.data).unwrap());
        if chunk.eof {
            break;
        }
    }
    let root_entries = parse_zip_entries(&root_zip);
    assert!(root_entries.contains_key("a.txt"));
    assert!(root_entries.contains_key("bundle/empty-sub/"));
    assert!(root_entries.contains_key("bundle/inner/数据.bin"));
    assert!(root_entries.contains_key(".gitignore"));
    assert!(
        root_entries
            .keys()
            .all(|k| k != ".git/" && !k.starts_with(".git/") && !k.starts_with(".tiechui")),
        "unexpected entries in root zip: {:?}",
        root_entries.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(&root_entries.get("uploads/deep/large.bin").unwrap().1)
        ),
        expected_sha
    );

    let orphan_up = workspace
        .upload_start(
            77,
            &UploadStartParams {
                root: root_str.clone(),
                path: "disconnect-dir/orphan.bin".into(),
                overwrite: false,
                size: None,
            },
        )
        .unwrap();
    let orphan_dl = workspace.download_start(77, &root_str, "bundle").unwrap();
    workspace.remove_client(77);
    assert!(!root.join("disconnect-dir").exists());
    assert!(
        workspace
            .upload_write(
                77,
                &UploadWriteParams {
                    transfer_id: orphan_up.transfer_id,
                    data: STANDARD.encode(b"x"),
                },
            )
            .is_err()
    );
    assert!(
        workspace
            .download_read(
                77,
                &DownloadReadParams {
                    transfer_id: orphan_dl.transfer_id,
                    chunk_size: None,
                },
            )
            .is_err()
    );

    assert!(
        workspace
            .upload_start(
                1,
                &UploadStartParams {
                    root: root_str.clone(),
                    path: "../escape.bin".into(),
                    overwrite: true,
                    size: None,
                },
            )
            .is_err()
    );
    assert!(
        workspace
            .download_start(1, &root_str, "../escape.bin")
            .is_err()
    );

    #[cfg(unix)]
    {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "top-secret\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("sym-out")).unwrap();
        assert!(
            workspace
                .upload_start(
                    1,
                    &UploadStartParams {
                        root: root_str.clone(),
                        path: "sym-out/pwn.bin".into(),
                        overwrite: true,
                        size: None,
                    },
                )
                .is_err()
        );
        assert!(
            workspace
                .download_start(1, &root_str, "sym-out/secret.txt")
                .is_err()
        );
    }

    workspace.close();
}
