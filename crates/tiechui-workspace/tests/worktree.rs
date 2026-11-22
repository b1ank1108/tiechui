#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tiechui_config::TrustStore;
use tiechui_protocol::workspace::{PortStopParams, PostCreateStatus, WorktreeSkipReason};
use tiechui_workspace as ws;

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tiechui")
        .env("GIT_AUTHOR_EMAIL", "tiechui@example.com")
        .env("GIT_COMMITTER_NAME", "Tiechui")
        .env("GIT_COMMITTER_EMAIL", "tiechui@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo(root: &Path) {
    std::fs::create_dir_all(root.join("src")).unwrap();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn v() -> u32 { 1 }\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "init"]);
    git(root, &["branch", "feature/alt"]);
}

#[test]
fn creates_lists_and_removes_managed_worktrees_preserving_branches_and_ignoring_manual_checkouts() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("my-repo");
    let non_git = temp.path().join("plain-dir");
    std::fs::create_dir_all(&non_git).unwrap();
    init_repo(&repo);

    git(&repo, &["checkout", "feature/alt"]);
    std::fs::write(repo.join("alt.txt"), "from-alt\n").unwrap();
    git(&repo, &["add", "alt.txt"]);
    git(&repo, &["commit", "-m", "alt commit"]);
    let alt_head = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&repo, &["checkout", "main"]);
    let main_head = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();

    let env = BTreeMap::new();

    assert!(ws::create_worktree_with_env(&home, &non_git, None, &env).is_err());
    assert!(ws::create_worktree_with_env(&home, &repo, Some("no-such-branch"), &env).is_err());

    let wt_main = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    let wt_alt = ws::create_worktree_with_env(&home, &repo, Some("feature/alt"), &env).unwrap();

    assert_ne!(wt_main.name, wt_alt.name);
    let (date_part, id_part) = wt_main.name.split_once('-').expect("YYYYMMDD-short_id");
    assert_eq!(date_part.len(), 8);
    assert!(date_part.bytes().all(|b| b.is_ascii_digit()));
    assert!(!id_part.is_empty());

    assert_eq!(wt_main.branch, format!("tiechui/{}", wt_main.name));
    assert_eq!(wt_main.base_branch, "main");
    assert!(!wt_main.dirty);

    assert_eq!(wt_alt.branch, format!("tiechui/{}", wt_alt.name));
    assert_eq!(wt_alt.base_branch, "feature/alt");
    assert!(!wt_alt.dirty);

    let wt_main_path = PathBuf::from(&wt_main.path);
    let wt_alt_path = PathBuf::from(&wt_alt.path);
    assert!(wt_main_path.is_dir());
    assert!(wt_alt_path.is_dir());
    assert_eq!(
        wt_main_path,
        repo.join(".tiechui/worktrees").join(&wt_main.name)
    );
    assert_eq!(
        wt_alt_path,
        repo.join(".tiechui/worktrees").join(&wt_alt.name)
    );
    assert!(!repo.join(".gitignore").exists());
    let exclude_text = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
    assert_eq!(
        exclude_text
            .lines()
            .filter(|line| line.trim() == "/.tiechui/worktrees/")
            .count(),
        1,
        "expected idempotent /.tiechui/worktrees/ in info/exclude, got:\n{exclude_text}"
    );

    assert_eq!(
        git(&wt_main_path, &["branch", "--show-current"]).trim(),
        wt_main.branch
    );
    assert_eq!(git(&wt_main_path, &["rev-parse", "HEAD"]).trim(), main_head);
    assert_eq!(git(&wt_alt_path, &["rev-parse", "HEAD"]).trim(), alt_head);
    assert!(wt_alt_path.join("alt.txt").is_file());
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for dir in [
            home.clone(),
            home.join("worktrees"),
            wt_main_path.parent().unwrap().to_path_buf(),
            wt_main_path.clone(),
            wt_main_path.join("src"),
        ] {
            let mode = std::fs::symlink_metadata(&dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777;
            assert_eq!(mode, 0o700, "expected 0700 on {}", dir.display());
        }
        let reg_mode = std::fs::symlink_metadata(home.join("worktrees/registry.json"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        assert_eq!(reg_mode, 0o600);
    }

    let manual_wt = temp.path().join("manual-wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "manual/branch",
            manual_wt.to_str().unwrap(),
            "main",
        ],
    );

    let canonical_repo = repo.canonicalize().unwrap();
    let listed_lexical = ws::list_worktrees_with_env(&home, Some(&repo), &env).unwrap();
    let listed_canonical = ws::list_worktrees_with_env(&home, Some(&canonical_repo), &env).unwrap();
    let listed_all = ws::list_worktrees_with_env(&home, None, &env).unwrap();
    assert_eq!(listed_lexical.len(), 2);
    assert_eq!(listed_lexical, listed_canonical);
    assert_eq!(listed_lexical, listed_all);
    assert!(
        listed_lexical
            .iter()
            .all(|e| Path::new(&e.path) != manual_wt)
    );

    assert!(ws::session_worktree(&home, &wt_main_path).is_some());
    assert!(ws::session_worktree(&home, &manual_wt).is_none());
    assert!(ws::session_worktree(&home, &repo).is_none());
    assert!(ws::remove_worktree_with_env(&home, &manual_wt, true, &env).is_err());

    std::fs::write(wt_main_path.join("README.md"), "modified\n").unwrap();
    std::fs::write(wt_alt_path.join("untracked.txt"), "new\n").unwrap();

    let dirty_list = ws::list_worktrees_with_env(&home, Some(&repo), &env).unwrap();
    assert!(dirty_list.iter().all(|e| e.dirty));

    assert!(ws::remove_worktree_with_env(&home, &wt_main_path, false, &env).is_err());
    assert!(wt_main_path.exists());

    git(&wt_main_path, &["checkout", "--", "README.md"]);
    let removed_main = ws::remove_worktree_with_env(&home, &wt_main_path, false, &env).unwrap();
    assert_eq!(removed_main.branch, wt_main.branch);
    assert!(!wt_main_path.exists());

    let removed_alt = ws::remove_worktree_with_env(&home, &wt_alt_path, true, &env).unwrap();
    assert_eq!(removed_alt.branch, wt_alt.branch);
    assert!(!wt_alt_path.exists());

    assert!(
        ws::list_worktrees_with_env(&home, Some(&repo), &env)
            .unwrap()
            .is_empty()
    );

    let branches = git(&repo, &["branch", "--list", "tiechui/*"]);
    assert!(branches.contains(&wt_main.branch), "branches: {branches}");
    assert!(branches.contains(&wt_alt.branch), "branches: {branches}");
}

#[test]
#[ignore = "invoked by eight_concurrent_creates_and_eight_concurrent_removes_across_processes"]
fn worktree_worker_helper() {
    let op = std::env::var("TC_WT_OP").unwrap();
    let home = PathBuf::from(std::env::var("TC_WT_HOME").unwrap());
    let env = BTreeMap::new();
    match op.as_str() {
        "create" => {
            let repo = PathBuf::from(std::env::var("TC_WT_REPO").unwrap());
            let entry = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "WT_CREATED={}\t{}", entry.name, entry.path).unwrap();
        }
        "remove" => {
            let path = PathBuf::from(std::env::var("TC_WT_PATH").unwrap());
            let res = ws::remove_worktree_with_env(&home, &path, false, &env).unwrap();
            let mut stdout = std::io::stdout().lock();
            writeln!(stdout, "WT_REMOVED={}", res.branch).unwrap();
        }
        other => panic!("unexpected op {other}"),
    }
}

#[test]
fn eight_concurrent_creates_and_eight_concurrent_removes_across_processes() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("concurrent-repo");
    init_repo(&repo);
    let exe = std::env::current_exe().unwrap();

    let spawned_creates: Vec<_> = (0..8)
        .map(|_| {
            Command::new(&exe)
                .args([
                    "--ignored",
                    "--exact",
                    "worktree_worker_helper",
                    "--nocapture",
                ])
                .env("TC_WT_OP", "create")
                .env("TC_WT_HOME", &home)
                .env("TC_WT_REPO", &repo)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();

    let mut created_paths = Vec::new();
    let mut created_names = BTreeSet::new();
    for child in spawned_creates {
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "concurrent create failed:\nstdout: {stdout}\nstderr: {stderr}"
        );
        assert!(!stderr.contains("index.lock") && !stderr.contains("cannot lock ref"));
        let payload = stdout
            .lines()
            .find_map(|l| l.strip_prefix("WT_CREATED="))
            .expect("missing WT_CREATED line");
        let (name, path) = payload.split_once('\t').unwrap();
        assert!(
            created_names.insert(name.to_owned()),
            "duplicate name {name}"
        );
        created_paths.push(PathBuf::from(path));
    }
    assert_eq!(created_paths.len(), 8);

    let env = BTreeMap::new();
    let listed = ws::list_worktrees_with_env(&home, Some(&repo), &env).unwrap();
    assert_eq!(listed.len(), 8);

    let spawned_removes: Vec<_> = created_paths
        .iter()
        .map(|path| {
            Command::new(&exe)
                .args([
                    "--ignored",
                    "--exact",
                    "worktree_worker_helper",
                    "--nocapture",
                ])
                .env("TC_WT_OP", "remove")
                .env("TC_WT_HOME", &home)
                .env("TC_WT_PATH", path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();

    for child in spawned_removes {
        let out = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "concurrent remove failed:\nstdout: {stdout}\nstderr: {stderr}"
        );
        assert!(!stderr.contains("index.lock") && !stderr.contains("cannot lock ref"));
        assert!(stdout.contains("WT_REMOVED=tiechui/"));
    }

    assert!(
        ws::list_worktrees_with_env(&home, Some(&repo), &env)
            .unwrap()
            .is_empty()
    );
    let kept_branches: Vec<_> = git(&repo, &["branch", "--list", "tiechui/*"])
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(kept_branches.len(), 8);
}

#[test]
#[ignore = "spawned by worktree port attribution tests"]
fn wt_tcp_listener_helper() {
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

struct SpawnedListener {
    pid: u32,
    port: u16,
    exited: Arc<std::sync::atomic::AtomicBool>,
}

impl SpawnedListener {
    fn start(cwd: &Path, extra_arg: Option<&str>) -> Self {
        use std::io::{BufRead, BufReader};
        use std::process::Stdio;
        use std::sync::atomic::{AtomicBool, Ordering};

        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.current_dir(cwd)
            .args(["--ignored", "--nocapture", "wt_tcp_listener_helper"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(arg) = extra_arg {
            cmd.arg(arg);
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
        let flag = exited.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            flag.store(true, Ordering::SeqCst);
        });
        Self { pid, port, exited }
    }

    fn is_exited(&self) -> bool {
        self.exited.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for SpawnedListener {
    fn drop(&mut self) {
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

#[test]
fn ports_in_managed_worktree_attribute_to_owning_project_and_stop_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project_a = temp.path().join("project-a");
    let project_b = temp.path().join("project-b");
    let outside = temp.path().join("outside");
    let manual_wt = temp.path().join("manual-wt");
    std::fs::create_dir_all(&outside).unwrap();
    init_repo(&project_a);
    init_repo(&project_b);

    let env = BTreeMap::new();
    let wt_a = ws::create_worktree_with_env(&home, &project_a, None, &env).unwrap();
    let wt_a_path = PathBuf::from(&wt_a.path);

    git(
        &project_a,
        &[
            "worktree",
            "add",
            "-b",
            "manual/ports",
            manual_wt.to_str().unwrap(),
            "main",
        ],
    );

    let notices = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    let sink = notices.clone();
    let workspace = ws::Workspace::with_home(
        home.clone(),
        Arc::new(move |method, params| {
            sink.lock().unwrap().push((method.to_owned(), params));
        }),
    );

    let a_str = project_a.display().to_string();
    let b_str = project_b.display().to_string();
    let initial = workspace
        .list_ports(&[a_str.clone(), b_str.clone()])
        .unwrap();
    assert!(initial.is_empty());

    let wt_script = wt_a_path.join("server.js").display().to_string();
    let main_script = project_a.join("main-server.js").display().to_string();
    let listener_main = SpawnedListener::start(&project_a, Some(&main_script));
    let listener_cmd = SpawnedListener::start(&outside, Some(&wt_script));
    let listener_cwd = SpawnedListener::start(&wt_a_path, None);
    let listener_manual = SpawnedListener::start(&manual_wt, None);

    let listed = workspace
        .list_ports(&[a_str.clone(), b_str.clone()])
        .unwrap();
    assert!(listed.iter().any(|e| {
        e.root == a_str
            && e.pid == listener_main.pid
            && e.port == listener_main.port
            && e.worktree_path.is_none()
            && e.worktree_branch.is_none()
    }));
    assert!(
        listed.iter().any(|e| {
            e.root == a_str
                && e.pid == listener_cmd.pid
                && e.port == listener_cmd.port
                && e.worktree_path.as_deref() == Some(wt_a.path.as_str())
                && e.worktree_branch.as_deref() == Some(wt_a.branch.as_str())
        }),
        "expected worktree metadata on listener_cmd, got: {listed:?}"
    );
    #[cfg(not(windows))]
    assert!(listed.iter().any(|e| {
        e.root == a_str
            && e.pid == listener_cwd.pid
            && e.port == listener_cwd.port
            && e.worktree_path.as_deref() == Some(wt_a.path.as_str())
            && e.worktree_branch.as_deref() == Some(wt_a.branch.as_str())
    }));
    assert!(listed.iter().all(|e| e.pid != listener_manual.pid));

    let listed_with_wt_root = workspace
        .list_ports(&[a_str.clone(), wt_a.path.clone(), b_str.clone()])
        .unwrap();
    assert!(
        listed_with_wt_root.iter().any(|e| {
            e.root == a_str
                && e.pid == listener_cmd.pid
                && e.port == listener_cmd.port
                && e.worktree_path.as_deref() == Some(wt_a.path.as_str())
                && e.worktree_branch.as_deref() == Some(wt_a.branch.as_str())
        }),
        "expected managed worktree listener to remain attributed to project root with worktree metadata, got: {listed_with_wt_root:?}"
    );

    assert!(
        workspace
            .stop_port(&PortStopParams {
                root: b_str,
                pid: listener_cmd.pid,
                port: Some(listener_cmd.port),
            })
            .is_err()
    );
    assert!(!listener_cmd.is_exited());

    notices.lock().unwrap().clear();
    workspace
        .stop_port(&PortStopParams {
            root: a_str,
            pid: listener_cmd.pid,
            port: Some(listener_cmd.port),
        })
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(4);
    while !listener_cmd.is_exited() {
        assert!(Instant::now() < deadline, "listener_cmd did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = listener_main;
    let _ = listener_cwd;
    workspace.close();
}

#[test]
fn post_create_script_respects_project_trust_captures_output_and_keeps_worktree_on_failure_or_timeout()
 {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("script-repo");
    init_repo(&repo);
    let env = BTreeMap::new();

    let wt_none = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    assert_eq!(wt_none.post_create.status, PostCreateStatus::None);
    assert!(wt_none.post_create.output.is_empty());

    std::fs::create_dir_all(repo.join(".agents")).unwrap();
    let setup_script = if cfg!(windows) {
        "echo setup-out && echo setup-err >&2 && touch .setup-done && mkdir -p generated"
    } else {
        "echo setup-out && echo setup-err >&2 && touch .setup-done && mkdir -m 755 generated && ln -s README.md linked-readme"
    };
    std::fs::write(
        repo.join(".agents/config.toml"),
        format!("[worktrees]\npost_create_script = {setup_script:?}\n"),
    )
    .unwrap();
    git(&repo, &["add", ".agents/config.toml"]);
    git(&repo, &["commit", "-m", "add post_create_script"]);

    let wt_untrusted = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    assert_eq!(
        wt_untrusted.post_create.status,
        PostCreateStatus::SkippedUntrusted
    );
    assert!(wt_untrusted.post_create.reason.is_some());
    assert!(!Path::new(&wt_untrusted.path).join(".setup-done").exists());

    TrustStore::new(&home)
        .set_blocking(&repo, Some(true))
        .unwrap();

    let wt_ok = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    assert_eq!(wt_ok.post_create.status, PostCreateStatus::Succeeded);
    assert_eq!(wt_ok.post_create.exit_code, Some(0));
    assert!(wt_ok.post_create.output.contains("setup-out"));
    assert!(wt_ok.post_create.output.contains("setup-err"));
    assert!(Path::new(&wt_ok.path).join(".setup-done").exists());
    assert!(!repo.join(".setup-done").exists());
    assert!(wt_ok.dirty);

    let listed = ws::list_worktrees_with_env(&home, Some(&repo), &env).unwrap();
    let listed_ok = listed.iter().find(|e| e.name == wt_ok.name).unwrap();
    assert_eq!(listed_ok.post_create, wt_ok.post_create);

    let fail_script = "echo step-failed >&2 && exit 1";
    std::fs::write(
        repo.join(".agents/config.toml"),
        format!("[worktrees]\npost_create_script = {fail_script:?}\n"),
    )
    .unwrap();
    let wt_fail = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    assert_eq!(wt_fail.post_create.status, PostCreateStatus::Failed);
    assert_eq!(wt_fail.post_create.exit_code, Some(1));
    assert!(wt_fail.post_create.output.contains("step-failed"));
    assert!(wt_fail.post_create.reason.is_some());
    assert!(Path::new(&wt_fail.path).is_dir());
    assert!(ws::session_worktree(&home, Path::new(&wt_fail.path)).is_some());

    {
        let hang_script = "echo before-timeout && sleep 10";
        std::fs::write(
            repo.join(".agents/config.toml"),
            format!("[worktrees]\npost_create_script = {hang_script:?}\n"),
        )
        .unwrap();
        // Git Bash on a cold Windows runner can take longer than 200 ms to print anything.
        let timeout = Duration::from_millis(if cfg!(windows) { 2000 } else { 200 });
        let started = Instant::now();
        let wt_timeout = ws::create_worktree_with_options(
            &home,
            &repo,
            ws::CreateWorktreeOptions {
                base_branch: None,
                carry_changes: false,
                post_create_timeout: Some(timeout),
            },
            &env,
        )
        .unwrap();
        assert!(started.elapsed() < Duration::from_secs(6));
        assert_eq!(wt_timeout.post_create.status, PostCreateStatus::Failed);
        assert!(wt_timeout.post_create.output.contains("before-timeout"));
        assert!(
            wt_timeout
                .post_create
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("timed out")),
            "reason: {:?}",
            wt_timeout.post_create.reason
        );
        assert!(Path::new(&wt_timeout.path).is_dir());
    }
}

#[test]
fn carry_changes_replays_tracked_and_untracked_changes_without_touching_main_index() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("carry-repo");
    init_repo(&repo);
    std::fs::write(repo.join(".gitignore"), "ignored.log\nbuild/\n").unwrap();
    git(&repo, &["add", ".gitignore"]);
    git(&repo, &["commit", "-m", "add gitignore"]);

    std::fs::write(repo.join("staged.txt"), "staged payload\n").unwrap();
    git(&repo, &["add", "staged.txt"]);
    std::fs::write(repo.join("README.md"), "modified readme\n").unwrap();
    std::fs::remove_file(repo.join("src/lib.rs")).unwrap();
    std::fs::create_dir_all(repo.join("notes")).unwrap();
    std::fs::write(repo.join("notes/todo.txt"), "untracked note\n").unwrap();
    std::fs::write(repo.join("ignored.log"), "do not carry\n").unwrap();
    std::fs::create_dir_all(repo.join("build")).unwrap();
    std::fs::write(repo.join("build/out.bin"), "ignored dir\n").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        std::fs::create_dir_all(repo.join("bin")).unwrap();
        let script_path = repo.join("bin/tool.sh");
        std::fs::write(&script_path, "#!/bin/sh\necho ok\n").unwrap();
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink("README.md", repo.join("readme-link")).unwrap();
    }

    let status_before = git(
        &repo,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain",
            "--untracked-files=all",
        ],
    );
    let cached_before = git(&repo, &["--no-optional-locks", "diff", "--cached"]);
    assert!(!cached_before.is_empty());
    let index_path = repo.join(".git/index");
    let index_hash_before = Sha256::digest(std::fs::read(&index_path).unwrap());
    let index_mtime_before = std::fs::metadata(&index_path).unwrap().modified().unwrap();

    std::thread::sleep(Duration::from_millis(20));

    let env = BTreeMap::new();
    let wt_clean = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    assert!(!wt_clean.dirty);
    assert_eq!(
        std::fs::read_to_string(Path::new(&wt_clean.path).join("README.md")).unwrap(),
        "hello\n"
    );
    assert!(!Path::new(&wt_clean.path).join("notes/todo.txt").exists());

    let main_index_lock = repo.join(".git/index.lock");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&main_index_lock)
        .unwrap();

    let wt_carried = ws::create_worktree_with_options(
        &home,
        &repo,
        ws::CreateWorktreeOptions {
            base_branch: None,
            carry_changes: true,
            post_create_timeout: None,
        },
        &env,
    )
    .unwrap();
    std::fs::remove_file(&main_index_lock).unwrap();
    let wt_path = PathBuf::from(&wt_carried.path);

    assert_eq!(
        Sha256::digest(std::fs::read(&index_path).unwrap()),
        index_hash_before
    );
    assert_eq!(
        std::fs::metadata(&index_path).unwrap().modified().unwrap(),
        index_mtime_before
    );
    assert_eq!(
        git(
            &repo,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain",
                "--untracked-files=all",
            ],
        ),
        status_before
    );
    assert_eq!(
        git(&repo, &["--no-optional-locks", "diff", "--cached"]),
        cached_before
    );
    assert!(!main_index_lock.exists());

    assert!(wt_carried.dirty);
    assert_eq!(
        std::fs::read_to_string(wt_path.join("README.md")).unwrap(),
        "modified readme\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt_path.join("staged.txt")).unwrap(),
        "staged payload\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt_path.join("notes/todo.txt")).unwrap(),
        "untracked note\n"
    );
    assert!(!wt_path.join("src/lib.rs").exists());
    assert!(!wt_path.join("ignored.log").exists());
    assert!(!wt_path.join("build").exists());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::symlink_metadata(wt_path.join("bin/tool.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "expected executable mode, got {mode:04o}");
        let link_meta = std::fs::symlink_metadata(wt_path.join("readme-link")).unwrap();
        assert!(link_meta.file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(wt_path.join("readme-link")).unwrap(),
            Path::new("README.md")
        );
    }

    assert_eq!(git(&wt_path, &["diff", "--cached"]), "");
    let wt_status = git(
        &wt_path,
        &["status", "--porcelain", "--untracked-files=all"],
    );
    assert!(wt_status.contains(" M README.md"), "wt_status: {wt_status}");
    assert!(
        wt_status.contains(" D src/lib.rs"),
        "wt_status: {wt_status}"
    );
    assert!(
        wt_status.contains("?? notes/todo.txt"),
        "wt_status: {wt_status}"
    );
    assert!(
        wt_status.contains("?? staged.txt"),
        "wt_status: {wt_status}"
    );
}

#[test]
fn carry_changes_enforces_2000_file_and_200_mib_limits_without_side_effects() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo_many = temp.path().join("many-repo");
    init_repo(&repo_many);
    let bulk_dir = repo_many.join("bulk");
    std::fs::create_dir_all(&bulk_dir).unwrap();
    for i in 0..2_001 {
        std::fs::write(bulk_dir.join(format!("f-{i:04}.txt")), "x").unwrap();
    }

    let env = BTreeMap::new();
    let err_many = ws::create_worktree_with_options(
        &home,
        &repo_many,
        ws::CreateWorktreeOptions {
            base_branch: None,
            carry_changes: true,
            post_create_timeout: None,
        },
        &env,
    )
    .unwrap_err()
    .0;
    assert!(
        err_many.contains("2000") || err_many.contains("2,000"),
        "unexpected error: {err_many}"
    );
    assert!(!home.join("worktrees").exists());
    assert_eq!(git(&repo_many, &["branch", "--list", "tiechui/*"]), "");
    assert!(
        ws::list_worktrees_with_env(&home, Some(&repo_many), &env)
            .unwrap()
            .is_empty()
    );

    let repo_large = temp.path().join("large-repo");
    init_repo(&repo_large);
    std::fs::write(repo_large.join(".gitignore"), "ignored-huge.bin\n").unwrap();
    git(&repo_large, &["add", ".gitignore"]);
    git(&repo_large, &["commit", "-m", "ignore huge"]);

    let ignored_file = std::fs::File::create(repo_large.join("ignored-huge.bin")).unwrap();
    ignored_file.set_len(200 * 1024 * 1024 + 1024).unwrap();
    drop(ignored_file);

    let wt_with_ignored = ws::create_worktree_with_options(
        &home,
        &repo_large,
        ws::CreateWorktreeOptions {
            base_branch: None,
            carry_changes: true,
            post_create_timeout: None,
        },
        &env,
    )
    .unwrap();
    ws::remove_worktree_with_env(&home, Path::new(&wt_with_ignored.path), true, &env).unwrap();
    let branches_before = git(&repo_large, &["branch", "--list", "tiechui/*"]);

    let huge_file = std::fs::File::create(repo_large.join("untracked-huge.bin")).unwrap();
    huge_file.set_len(200 * 1024 * 1024 + 1).unwrap();
    drop(huge_file);

    let err_large = ws::create_worktree_with_options(
        &home,
        &repo_large,
        ws::CreateWorktreeOptions {
            base_branch: None,
            carry_changes: true,
            post_create_timeout: None,
        },
        &env,
    )
    .unwrap_err()
    .0;
    assert!(
        err_large.contains("200 MiB"),
        "unexpected error: {err_large}"
    );
    assert_eq!(
        git(&repo_large, &["branch", "--list", "tiechui/*"]),
        branches_before
    );
    assert!(
        ws::list_worktrees_with_env(&home, Some(&repo_large), &env)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn captures_hidden_tree_snapshot_without_touching_index_status_branches_log_or_gc() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("snapshot-repo");
    init_repo(&repo);
    std::fs::write(repo.join("later-ignored.txt"), "tracked-v1\n").unwrap();
    git(&repo, &["add", "later-ignored.txt"]);
    git(&repo, &["commit", "-m", "add later-ignored"]);
    std::fs::write(
        repo.join(".gitignore"),
        "node_modules/\nignored.log\nlater-ignored.txt\n",
    )
    .unwrap();
    git(&repo, &["add", ".gitignore"]);
    git(&repo, &["commit", "-m", "add gitignore"]);

    let env = BTreeMap::new();
    let wt = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    let wt_path = PathBuf::from(&wt.path);

    std::fs::write(wt_path.join("README.md"), "staged-readme\n").unwrap();
    git(&wt_path, &["add", "README.md"]);
    std::fs::write(wt_path.join("README.md"), "working-readme\n").unwrap();
    std::fs::write(wt_path.join("staged-only.txt"), "staged-new\n").unwrap();
    git(&wt_path, &["add", "staged-only.txt"]);
    std::fs::write(wt_path.join("later-ignored.txt"), "tracked-v2\n").unwrap();
    std::fs::remove_file(wt_path.join("src/lib.rs")).unwrap();
    std::fs::create_dir_all(wt_path.join("notes")).unwrap();
    std::fs::write(wt_path.join("notes/todo.txt"), "untracked-note\n").unwrap();
    std::fs::create_dir_all(wt_path.join("node_modules/pkg")).unwrap();
    std::fs::write(wt_path.join("node_modules/pkg/index.js"), "ignored\n").unwrap();
    std::fs::write(wt_path.join("ignored.log"), "ignored-log\n").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = wt_path.join("run.sh");
        std::fs::write(&script, "#!/bin/sh\necho ok\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("README.md", wt_path.join("readme-link")).unwrap();
    }

    let git_index_rel = git(&wt_path, &["rev-parse", "--git-path", "index"]);
    let git_index_path = wt_path.join(git_index_rel.trim());
    let status_before = git(&wt_path, &["status", "--porcelain=v1"]);
    let cached_before = git(&wt_path, &["diff", "--cached"]);
    let branches_before = git(&wt_path, &["branch", "-a"]);
    let log_before = git(&wt_path, &["log", "--all", "--oneline"]);
    let index_bytes_before = std::fs::read(&git_index_path).unwrap();

    let snap = ws::capture_worktree_snapshot_with_env(
        &wt_path,
        "sess-1",
        "turn/1..anchor",
        Duration::from_secs(10),
        &env,
    )
    .unwrap();
    let index_bytes_after = std::fs::read(&git_index_path).unwrap();

    assert!(snap.reference.starts_with("refs/tiechui/checkpoints/"));
    git(&wt_path, &["check-ref-format", &snap.reference]);
    assert_eq!(
        snap.reference,
        ws::checkpoint_ref_name("sess-1", "turn/1..anchor")
    );
    assert!(!git_index_path.with_extension("index.lock").exists());
    assert_eq!(index_bytes_after, index_bytes_before);
    assert_eq!(git(&wt_path, &["status", "--porcelain=v1"]), status_before);
    assert_eq!(git(&wt_path, &["diff", "--cached"]), cached_before);
    assert_eq!(git(&wt_path, &["branch", "-a"]), branches_before);
    assert_eq!(git(&wt_path, &["log", "--all", "--oneline"]), log_before);

    let refs_out = git(
        &wt_path,
        &[
            "for-each-ref",
            "--format=%(refname) %(objecttype) %(objectname)",
            "refs/tiechui/checkpoints/",
        ],
    );
    assert_eq!(
        refs_out.trim(),
        format!("{} tree {}", snap.reference, snap.tree)
    );

    git(&repo, &["gc", "--prune=now", "-q"]);
    git(&repo, &["fsck", "--full"]);

    assert_eq!(
        git(&wt_path, &["show", &format!("{}:README.md", snap.tree)]),
        "working-readme\n"
    );
    assert_eq!(
        git(
            &wt_path,
            &["show", &format!("{}:staged-only.txt", snap.tree)]
        ),
        "staged-new\n"
    );
    assert_eq!(
        git(
            &wt_path,
            &["show", &format!("{}:later-ignored.txt", snap.tree)]
        ),
        "tracked-v2\n"
    );
    assert_eq!(
        git(
            &wt_path,
            &["show", &format!("{}:notes/todo.txt", snap.tree)]
        ),
        "untracked-note\n"
    );
    let tree_files = git(&wt_path, &["ls-tree", "-r", "--name-only", &snap.tree]);
    let names: BTreeSet<&str> = tree_files.lines().collect();
    assert!(!names.contains("src/lib.rs"));
    assert!(!names.contains("node_modules/pkg/index.js"));
    assert!(!names.contains("ignored.log"));
    #[cfg(unix)]
    {
        assert!(names.contains("run.sh"));
        assert!(names.contains("readme-link"));
    }
}

#[test]
fn snapshot_capture_enforces_timeout_reaps_process_group_and_fails_on_broken_repo() {
    let temp = tempfile::tempdir().unwrap();
    let plain = temp.path().join("not-a-repo");
    std::fs::create_dir_all(&plain).unwrap();
    let empty_env = BTreeMap::new();
    assert!(
        ws::capture_worktree_snapshot_with_env(
            &plain,
            "sess",
            "anchor",
            Duration::from_secs(2),
            &empty_env,
        )
        .is_err()
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let home = temp.path().join("home");
        let repo = temp.path().join("slow-repo");
        init_repo(&repo);
        let wt = ws::create_worktree_with_env(&home, &repo, None, &empty_env).unwrap();
        let wt_path = PathBuf::from(&wt.path);

        let fake_bin = temp.path().join("fake-bin");
        std::fs::create_dir_all(&fake_bin).unwrap();
        let pid_file = temp.path().join("sleep.pid");
        let fake_git = fake_bin.join("git");
        std::fs::write(
            &fake_git,
            format!(
                "#!/bin/sh\nsleep 30 &\necho $! > '{}'\nwait\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake_git, std::fs::Permissions::from_mode(0o755)).unwrap();

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let slow_env =
            BTreeMap::from([("PATH".into(), format!("{}:{orig_path}", fake_bin.display()))]);
        let started = Instant::now();
        let err = ws::capture_worktree_snapshot_with_env(
            &wt_path,
            "sess",
            "anchor",
            Duration::from_millis(120),
            &slow_env,
        )
        .unwrap_err()
        .0;
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(err.contains("timed out"), "unexpected error: {err}");

        if let Ok(raw_pid) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = raw_pid.trim().parse::<i32>()
        {
            unsafe extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            for _ in 0..20 {
                if unsafe { kill(pid, 0) } != 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_ne!(unsafe { kill(pid, 0) }, 0, "lingering child pid {pid}");
        }
    }
}

#[test]
fn restores_whole_worktree_to_hidden_snapshot_preserving_ignored_files_and_resetting_index() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("restore-repo");
    init_repo(&repo);
    std::fs::write(repo.join("a.txt"), "initial-a\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "node_modules/\nignored.log\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "add fixture files"]);

    let env = BTreeMap::new();
    let wt = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    let wt_path = PathBuf::from(&wt.path);

    std::fs::write(wt_path.join("README.md"), "turn1-readme\n").unwrap();
    std::fs::write(wt_path.join("untouched_dirty.txt"), "turn1-untouched\n").unwrap();
    std::fs::remove_file(wt_path.join("src/lib.rs")).unwrap();
    std::fs::create_dir_all(wt_path.join("notes")).unwrap();
    std::fs::write(wt_path.join("notes/todo.txt"), "turn1-note\n").unwrap();
    std::fs::create_dir_all(wt_path.join("node_modules/pkg")).unwrap();
    std::fs::write(wt_path.join("node_modules/pkg/index.js"), "cached-v1\n").unwrap();
    std::fs::write(wt_path.join("ignored.log"), "keep-log\n").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = wt_path.join("run.sh");
        std::fs::write(&script, "#!/bin/sh\necho turn1\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("README.md", wt_path.join("readme-link")).unwrap();
    }

    let snap = ws::capture_worktree_snapshot_with_env(
        &wt_path,
        "sess-restore",
        "turn-2",
        Duration::from_secs(10),
        &env,
    )
    .unwrap();
    assert!(ws::has_worktree_snapshot_tree(&wt_path, &snap.tree, &env));
    assert!(!ws::has_worktree_snapshot_tree(
        &wt_path,
        "0000000000000000000000000000000000000000",
        &env
    ));

    let branches_before = git(&wt_path, &["branch", "-a"]);
    let log_before = git(&wt_path, &["log", "--all", "--oneline"]);

    std::fs::remove_file(wt_path.join("a.txt")).unwrap();
    std::fs::write(wt_path.join("b.txt"), "x\n").unwrap();
    git(&wt_path, &["add", "b.txt"]);
    std::fs::create_dir_all(wt_path.join("deep/nested")).unwrap();
    std::fs::write(wt_path.join("deep/nested/extra.txt"), "extra\n").unwrap();
    std::fs::create_dir_all(wt_path.join("src")).unwrap();
    std::fs::write(wt_path.join("src/lib.rs"), "recreated-in-turn2\n").unwrap();
    std::fs::write(wt_path.join("README.md"), "manual-edit\n").unwrap();
    std::fs::write(wt_path.join("notes/todo.txt"), "mutated-note\n").unwrap();
    std::fs::write(wt_path.join(".gitignore"), "node_modules/\n").unwrap();
    std::fs::create_dir_all(wt_path.join("sub")).unwrap();
    std::fs::write(wt_path.join("sub/.gitignore"), "*.gen\n").unwrap();
    std::fs::write(wt_path.join("sub/hidden.gen"), "hidden-by-turn2-ignore\n").unwrap();
    std::fs::write(wt_path.join("node_modules/pkg/index.js"), "cached-v2\n").unwrap();
    std::fs::write(wt_path.join("node_modules/pkg/extra.js"), "keep-ignored\n").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            wt_path.join("run.sh"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        std::fs::remove_file(wt_path.join("readme-link")).unwrap();
        std::fs::create_dir_all(wt_path.join("readme-link")).unwrap();
        std::fs::write(wt_path.join("readme-link/inside.txt"), "replaced-link\n").unwrap();
    }

    let diff_paths = ws::diff_worktree_snapshot_with_env(
        &wt_path,
        &snap.tree,
        Duration::from_secs(10),
        &env,
        None,
    )
    .unwrap();
    assert!(diff_paths.contains(&"a.txt".to_owned()), "{diff_paths:?}");
    assert!(diff_paths.contains(&"b.txt".to_owned()), "{diff_paths:?}");
    assert!(
        !diff_paths.contains(&"untouched_dirty.txt".to_owned()),
        "{diff_paths:?}"
    );
    assert!(
        !diff_paths.iter().any(|p| p.starts_with("node_modules/")),
        "{diff_paths:?}"
    );

    ws::restore_worktree_snapshot_with_env(&wt_path, &snap.tree, Duration::from_secs(10), &env)
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(wt_path.join("a.txt")).unwrap(),
        "initial-a\n"
    );
    assert!(!wt_path.join("b.txt").exists());
    assert!(!wt_path.join("deep").exists());
    assert!(!wt_path.join("sub").exists());
    assert!(!wt_path.join("src/lib.rs").exists());
    assert_eq!(
        std::fs::read_to_string(wt_path.join("README.md")).unwrap(),
        "turn1-readme\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt_path.join("notes/todo.txt")).unwrap(),
        "turn1-note\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt_path.join(".gitignore")).unwrap(),
        "node_modules/\nignored.log\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt_path.join("node_modules/pkg/index.js")).unwrap(),
        "cached-v2\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt_path.join("node_modules/pkg/extra.js")).unwrap(),
        "keep-ignored\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt_path.join("ignored.log")).unwrap(),
        "keep-log\n"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::symlink_metadata(wt_path.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "expected executable mode, got {mode:04o}");
        assert!(
            std::fs::symlink_metadata(wt_path.join("readme-link"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_link(wt_path.join("readme-link")).unwrap(),
            Path::new("README.md")
        );
    }

    assert_eq!(git(&wt_path, &["diff", "--cached"]), "");
    let status_out = git(&wt_path, &["status"]);
    assert!(!status_out.contains("checkpoints"), "{status_out}");
    assert_eq!(git(&wt_path, &["branch", "-a"]), branches_before);
    assert_eq!(git(&wt_path, &["log", "--all", "--oneline"]), log_before);
}

#[test]
fn worktree_isolation_detects_equal_ancestor_descendant_archived_and_ignores_trashed_or_sibling() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("iso-repo");
    init_repo(&repo);
    let env = BTreeMap::new();
    let wt1 = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    let wt2 = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    let wt1_path = PathBuf::from(&wt1.path);
    let wt2_path = PathBuf::from(&wt2.path);

    let write_session = |group: &str, id: &str, cwd: &Path, meta_json: Option<&str>| -> PathBuf {
        let dir = home.join(group).join("bucket");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let header = serde_json::json!({
            "format_version": 1,
            "id": id,
            "cwd": cwd,
            "created_at": 1,
        });
        std::fs::write(&path, format!("{header}\n")).unwrap();
        if let Some(raw_meta) = meta_json {
            std::fs::write(path.with_extension("meta.json"), raw_meta).unwrap();
        }
        path
    };

    write_session("sessions", "self-sess", &wt1_path, None);
    write_session("sessions", "sibling-sess", &wt2_path, None);
    write_session("trash", "trashed-sess", &wt1_path, None);
    assert_eq!(
        ws::check_worktree_isolation(&home, &wt1_path, "self-sess", &[]).unwrap(),
        None
    );

    let eq_file = write_session(
        "sessions",
        "archived-peer",
        &wt1_path,
        Some(r#"{"title":"Archived Peer","archived_at":1700000000000}"#),
    );
    let conflict = ws::check_worktree_isolation(&home, &wt1_path, "self-sess", &[])
        .unwrap()
        .expect("archived equal-cwd session must block isolation");
    assert_eq!(conflict.session_id, "archived-peer");
    assert_eq!(conflict.title.as_deref(), Some("Archived Peer"));
    assert!(conflict.archived);
    assert_eq!(conflict.overlap, ws::PathOverlap::Equal);
    assert!(conflict.reason.contains("Archived Peer"));
    std::fs::remove_file(&eq_file).unwrap();
    std::fs::remove_file(eq_file.with_extension("meta.json")).unwrap();

    let repo_sess = write_session("sessions", "main-repo-sess", &repo, None);
    let outer_sess = write_session("sessions", "outer-sess", temp.path(), None);
    assert_eq!(
        ws::check_worktree_isolation(&home, &wt1_path, "self-sess", &[]).unwrap(),
        None,
        "main-repo and outer ancestor sessions across .tiechui/worktrees boundary must not block worktree isolation"
    );
    std::fs::remove_file(&repo_sess).unwrap();
    std::fs::remove_file(&outer_sess).unwrap();

    let anc_file = write_session(
        "sessions",
        "ancestor-sess",
        wt1_path.parent().unwrap(),
        None,
    );
    let conflict = ws::check_worktree_isolation(&home, &wt1_path, "self-sess", &[])
        .unwrap()
        .expect("direct .tiechui/worktrees container session must block isolation");
    assert_eq!(conflict.session_id, "ancestor-sess");
    assert_eq!(conflict.overlap, ws::PathOverlap::ContainsWorktree);
    std::fs::remove_file(&anc_file).unwrap();

    #[cfg(unix)]
    let descendant_cwd = {
        let alias = temp.path().join("wt-alias");
        std::os::unix::fs::symlink(&wt1_path, &alias).unwrap();
        alias.join("missing/subdir")
    };
    #[cfg(not(unix))]
    let descendant_cwd = wt1_path.join("missing/subdir");

    let conflict = ws::check_worktree_isolation(
        &home,
        &wt1_path,
        "self-sess",
        &[ws::LiveSessionCwd {
            session_id: "live-descendant".into(),
            cwd: descendant_cwd,
            path: None,
        }],
    )
    .unwrap()
    .expect("descendant live session must block isolation");
    assert_eq!(conflict.session_id, "live-descendant");
    assert_eq!(conflict.overlap, ws::PathOverlap::InsideWorktree);
}

#[test]
fn worktree_cleanup_enforces_default_off_idle_days_deleted_sessions_dirty_skip_active_skip_and_branch_preservation()
 {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("cleanup-repo");
    init_repo(&repo);
    let env = BTreeMap::new();

    let wt_idle_clean = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    let wt_dirty = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    let wt_live = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    let wt_recent = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();

    std::fs::write(Path::new(&wt_dirty.path).join("untracked.txt"), "dirty\n").unwrap();

    let write_session = |group: &str,
                         id: &str,
                         cwd: &Path,
                         created_at: u64,
                         entry_ts: Option<u64>,
                         meta: Option<&str>| {
        let dir = home.join(group).join("bucket");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let header = serde_json::json!({
            "format_version": 1,
            "id": id,
            "cwd": cwd,
            "created_at": created_at,
        });
        let mut body = format!("{header}\n");
        if let Some(ts) = entry_ts {
            let entry = serde_json::json!({
                "id": "e1",
                "parent_id": null,
                "timestamp": ts,
                "kind": "message",
                "message": { "role": "user", "content": [{ "kind": "text", "text": "hi" }] }
            });
            body.push_str(&format!("{entry}\n"));
        }
        std::fs::write(&path, body).unwrap();
        if let Some(raw_meta) = meta {
            std::fs::write(path.with_extension("meta.json"), raw_meta).unwrap();
        }
        path
    };

    let day_ms: u64 = 86_400_000;
    let now = wt_recent.created_at.saturating_add(10 * day_ms);
    let recent_ts = now - 2 * day_ms;
    write_session(
        "sessions",
        "recent-sess",
        Path::new(&wt_recent.path),
        wt_recent.created_at,
        Some(recent_ts),
        Some(r#"{"archived_at":1700000000000}"#),
    );
    let live_path = write_session(
        "sessions",
        "live-sess",
        Path::new(&wt_live.path),
        wt_live.created_at,
        None,
        Some(r#"{"title":"Active Investigation"}"#),
    );

    let default_status =
        ws::cleanup_worktrees_with_env(&home, ws::WorktreeCleanupPolicy::default(), &[], now, &env)
            .unwrap();
    assert!(default_status.removed.is_empty());
    assert!(default_status.skipped.is_empty());
    assert_eq!(default_status.last_checked_at, Some(now));
    assert!(Path::new(&wt_idle_clean.path).is_dir());

    let seven_day_status = ws::cleanup_worktrees_with_env(
        &home,
        ws::WorktreeCleanupPolicy {
            auto_clean_idle_days: Some(7),
            auto_clean_on_session_delete: false,
        },
        &[ws::LiveWorktreeSession {
            session_id: "live-sess".into(),
            cwd: PathBuf::from(&wt_live.path),
            path: Some(live_path),
            title: Some("Active Investigation".into()),
        }],
        now,
        &env,
    )
    .unwrap();

    assert_eq!(seven_day_status.removed.len(), 1);
    assert_eq!(seven_day_status.removed[0].path, wt_idle_clean.path);
    assert_eq!(seven_day_status.removed[0].branch, wt_idle_clean.branch);
    assert!(!Path::new(&wt_idle_clean.path).exists());
    assert!(Path::new(&wt_recent.path).is_dir());
    assert!(Path::new(&wt_dirty.path).is_dir());
    assert!(Path::new(&wt_live.path).is_dir());

    assert_eq!(seven_day_status.skipped.len(), 2);
    let skipped_dirty = seven_day_status
        .skipped
        .iter()
        .find(|s| s.path == wt_dirty.path)
        .expect("missing dirty skipped worktree");
    assert_eq!(skipped_dirty.reason_kind, WorktreeSkipReason::Dirty);
    assert!(skipped_dirty.reason.contains("存在未提交改动"));

    let skipped_live = seven_day_status
        .skipped
        .iter()
        .find(|s| s.path == wt_live.path)
        .expect("missing active_session skipped worktree");
    assert_eq!(skipped_live.reason_kind, WorktreeSkipReason::ActiveSession);
    assert!(skipped_live.reason.contains("仍有运行中的会话"));
    assert!(skipped_live.reason.contains("Active Investigation"));

    let orphan_clean = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    write_session(
        "trash",
        "trashed-sess",
        Path::new(&orphan_clean.path),
        orphan_clean.created_at,
        None,
        None,
    );
    let delete_policy_status = ws::cleanup_worktrees_with_env(
        &home,
        ws::WorktreeCleanupPolicy {
            auto_clean_idle_days: None,
            auto_clean_on_session_delete: true,
        },
        &[],
        orphan_clean.created_at,
        &env,
    )
    .unwrap();
    assert_eq!(delete_policy_status.removed.len(), 1);
    assert_eq!(delete_policy_status.removed[0].path, orphan_clean.path);
    assert!(!Path::new(&orphan_clean.path).exists());
    assert!(Path::new(&wt_recent.path).is_dir());
    assert!(
        delete_policy_status
            .skipped
            .iter()
            .any(|s| s.path == wt_dirty.path && s.reason_kind == WorktreeSkipReason::Dirty)
    );

    let zero_day_status = ws::cleanup_worktrees_with_env(
        &home,
        ws::WorktreeCleanupPolicy {
            auto_clean_idle_days: Some(0),
            auto_clean_on_session_delete: false,
        },
        &[],
        now,
        &env,
    )
    .unwrap();
    assert!(
        zero_day_status
            .removed
            .iter()
            .any(|r| r.path == wt_recent.path)
    );
    assert!(!Path::new(&wt_recent.path).exists());

    let branches = git(&repo, &["branch", "--list", "tiechui/*"]);
    for expected in [
        &wt_idle_clean.branch,
        &orphan_clean.branch,
        &wt_recent.branch,
    ] {
        assert!(
            branches.contains(expected),
            "missing {expected} in {branches}"
        );
    }
}

#[test]
fn remove_worktree_preserves_external_junction_or_symlink_target_files() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("junction-repo");
    let shared = temp.path().join("external-pnpm-store");
    std::fs::create_dir_all(shared.join("pkg")).unwrap();
    std::fs::write(shared.join("pkg/index.js"), "module.exports = 42;\n").unwrap();
    init_repo(&repo);
    std::fs::write(repo.join(".gitignore"), "node_modules/\n").unwrap();
    git(&repo, &["add", ".gitignore"]);
    git(&repo, &["commit", "-m", "ignore node_modules"]);

    let env = BTreeMap::new();
    let wt = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    let wt_path = PathBuf::from(&wt.path);
    std::fs::create_dir_all(wt_path.join("node_modules")).unwrap();
    let linked = wt_path.join("node_modules").join("shared-pkg");

    #[cfg(unix)]
    std::os::unix::fs::symlink(&shared, &linked).unwrap();
    #[cfg(windows)]
    {
        let status = Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                linked.to_str().unwrap(),
                shared.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }

    assert!(linked.join("pkg/index.js").is_file());
    ws::remove_worktree_with_env(&home, &wt_path, false, &env).unwrap();
    assert!(!wt_path.exists());
    assert_eq!(
        std::fs::read_to_string(shared.join("pkg/index.js")).unwrap(),
        "module.exports = 42;\n"
    );
    assert!(
        ws::list_worktrees_with_env(&home, Some(&repo), &env)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn in_project_worktree_isolates_main_tree_search_watch_carry_and_preserves_legacy_registry_entries()
{
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("iso-fs-repo");
    init_repo(&repo);
    let env = BTreeMap::new();

    let wt1 = ws::create_worktree_with_env(&home, &repo, None, &env).unwrap();
    let wt1_path = PathBuf::from(&wt1.path);
    assert_eq!(wt1_path, repo.join(".tiechui/worktrees").join(&wt1.name));
    std::fs::write(
        wt1_path.join("src/wt_secret.rs"),
        "pub const TOKEN: &str = \"WT_ONLY_NEEDLE_99\";\n",
    )
    .unwrap();
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");

    let main_tree = ws::tree(&repo).unwrap();
    assert!(
        main_tree
            .entries
            .iter()
            .all(|e| !e.path.starts_with(".tiechui")),
        "expected zero .tiechui entries when .tiechui holds only worktrees, got: {:?}",
        main_tree.entries
    );
    std::fs::write(repo.join(".tiechui/config.toml"), "thinking = \"low\"\n").unwrap();
    let main_tree_with_cfg = ws::tree(&repo).unwrap();
    assert!(
        main_tree_with_cfg
            .entries
            .iter()
            .any(|e| e.path == ".tiechui/config.toml")
    );
    assert!(
        main_tree_with_cfg
            .entries
            .iter()
            .all(|e| !e.path.starts_with(".tiechui/worktrees")),
        "expected .tiechui/worktrees hidden even when .tiechui has other files: {:?}",
        main_tree_with_cfg.entries
    );
    std::fs::remove_file(repo.join(".tiechui/config.toml")).unwrap();

    let wt_tree = ws::tree(&wt1_path).unwrap();
    let wt_paths: Vec<&str> = wt_tree.entries.iter().map(|e| e.path.as_str()).collect();
    assert!(
        wt_paths.contains(&"README.md") && wt_paths.contains(&"src/wt_secret.rs"),
        "expected worktree fs/tree to list files inside worktree, got: {wt_paths:?}"
    );

    for include in [Vec::new(), vec!["*.rs".to_owned()]] {
        let res = ws::search(
            &repo,
            &tiechui_protocol::workspace::SearchParams {
                root: repo.display().to_string(),
                query: "WT_ONLY_NEEDLE_99".into(),
                include,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            res.total, 0,
            "main repo search leaked worktree hit: {res:?}"
        );
    }

    let wt_search = ws::search(
        &wt1_path,
        &tiechui_protocol::workspace::SearchParams {
            root: wt1.path.clone(),
            query: "WT_ONLY_NEEDLE_99".into(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(wt_search.total, 1);
    assert_eq!(wt_search.groups[0].path, "src/wt_secret.rs");

    let notices = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    let sink = notices.clone();
    let workspace = ws::Workspace::with_home(
        home.clone(),
        Arc::new(move |method, params| {
            sink.lock().unwrap().push((method.to_owned(), params));
        }),
    );
    let canon_repo = repo.canonicalize().unwrap();
    let canon_wt1 = wt1_path.canonicalize().unwrap();
    workspace.watch(&canon_repo).unwrap();
    workspace.watch(&canon_wt1).unwrap();

    std::fs::write(wt1_path.join("src/watched_in_wt.rs"), "// wt watch\n").unwrap();
    std::fs::write(repo.join("src/watched_in_main.rs"), "// main watch\n").unwrap();

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let guard = notices.lock().unwrap();
        let got_wt = guard.iter().any(|(m, p)| {
            m == "fs/changed"
                && p["root"] == canon_wt1.display().to_string()
                && p["paths"]
                    .as_array()
                    .is_some_and(|arr| arr.contains(&Value::from("src/watched_in_wt.rs")))
        });
        let got_main = guard.iter().any(|(m, p)| {
            m == "fs/changed"
                && p["root"] == canon_repo.display().to_string()
                && p["paths"]
                    .as_array()
                    .is_some_and(|arr| arr.contains(&Value::from("src/watched_in_main.rs")))
        });
        drop(guard);
        if got_wt && got_main {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for watch events: {:?}",
            *notices.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let leaked_main: Vec<_> = notices
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, p)| {
            m == "fs/changed"
                && p["root"] == canon_repo.display().to_string()
                && p["paths"].as_array().is_some_and(|arr| {
                    arr.iter()
                        .any(|v| v.as_str().unwrap_or_default().starts_with(".tiechui"))
                })
        })
        .cloned()
        .collect();
    assert!(
        leaked_main.is_empty(),
        "main repo watch leaked .tiechui events: {leaked_main:?}"
    );
    workspace.unwatch(&canon_repo);
    workspace.unwatch(&canon_wt1);
    std::fs::remove_file(repo.join("src/watched_in_main.rs")).unwrap();

    std::fs::write(repo.join("carried.txt"), "carry-me\n").unwrap();
    let wt2 = ws::create_worktree_with_options(
        &home,
        &repo,
        ws::CreateWorktreeOptions {
            base_branch: None,
            carry_changes: true,
            post_create_timeout: None,
        },
        &env,
    )
    .unwrap();
    let wt2_path = PathBuf::from(&wt2.path);
    assert_eq!(
        std::fs::read_to_string(wt2_path.join("carried.txt")).unwrap(),
        "carry-me\n"
    );
    assert!(!wt2_path.join(".tiechui/worktrees").exists());
    std::fs::remove_file(repo.join("carried.txt")).unwrap();

    let legacy_bucket = home.join("worktrees/iso-fs-repo-legacy");
    std::fs::create_dir_all(&legacy_bucket).unwrap();
    let legacy_path = legacy_bucket.join("20261101-deadbeef");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "tiechui/20261101-deadbeef",
            legacy_path.to_str().unwrap(),
            "main",
        ],
    );
    std::fs::write(legacy_path.join("legacy.txt"), "legacy-committed\n").unwrap();
    git(&legacy_path, &["add", "legacy.txt"]);
    git(&legacy_path, &["commit", "-m", "legacy commit"]);

    let reg_path = home.join("worktrees/registry.json");
    let mut reg: Value = serde_json::from_slice(&std::fs::read(&reg_path).unwrap()).unwrap();
    reg["worktrees"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "name": "20261101-deadbeef",
            "path": legacy_path.display().to_string(),
            "project_root": repo.display().to_string(),
            "branch": "tiechui/20261101-deadbeef",
            "base_branch": "main",
            "created_at": 1_000,
            "post_create": { "status": "none", "output": "" }
        }));
    std::fs::write(&reg_path, serde_json::to_vec_pretty(&reg).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&reg_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    assert_eq!(
        ws::session_worktree(&home, &legacy_path)
            .as_ref()
            .map(|w| w.branch.as_str()),
        Some("tiechui/20261101-deadbeef")
    );
    assert_eq!(
        ws::worktree_project_root(&home, &legacy_path),
        Some(repo.canonicalize().unwrap())
    );

    let legacy_listener = SpawnedListener::start(
        &outside_dir(&temp),
        Some(&legacy_path.join("srv.js").display().to_string()),
    );
    let legacy_ports = workspace.list_ports(&[repo.display().to_string()]).unwrap();
    assert!(
        legacy_ports.iter().any(|p| {
            p.pid == legacy_listener.pid
                && p.root == repo.display().to_string()
                && p.worktree_path.as_deref() == Some(legacy_path.to_str().unwrap())
                && p.worktree_branch.as_deref() == Some("tiechui/20261101-deadbeef")
        }),
        "expected legacy worktree port metadata, got: {legacy_ports:?}"
    );
    drop(legacy_listener);

    let snap = ws::capture_worktree_snapshot_with_env(
        &legacy_path,
        "legacy-sess",
        "t1",
        Duration::from_secs(10),
        &env,
    )
    .unwrap();
    std::fs::write(legacy_path.join("legacy.txt"), "mutated\n").unwrap();
    ws::restore_worktree_snapshot_with_env(&legacy_path, &snap.tree, Duration::from_secs(10), &env)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(legacy_path.join("legacy.txt")).unwrap(),
        "legacy-committed\n"
    );

    let sess_dir = home.join("sessions/legacy-group");
    std::fs::create_dir_all(&sess_dir).unwrap();
    let sess_file = sess_dir.join("legacy-sess.jsonl");
    std::fs::write(
        &sess_file,
        format!(
            "{}\n",
            serde_json::json!({
                "format_version": 1,
                "id": "legacy-sess",
                "cwd": legacy_path,
                "created_at": 1_000,
            })
        ),
    )
    .unwrap();

    let cleaned = ws::cleanup_worktrees_with_env(
        &home,
        ws::WorktreeCleanupPolicy {
            auto_clean_idle_days: Some(1),
            auto_clean_on_session_delete: false,
        },
        &[],
        1_000 + 2 * 86_400_000,
        &env,
    )
    .unwrap();
    assert!(
        cleaned
            .removed
            .iter()
            .any(|r| Path::new(&r.path) == legacy_path)
    );
    assert!(!legacy_path.exists());

    let restored = ws::ensure_worktree_checkout(&home, &legacy_path, &env)
        .unwrap()
        .expect("expected legacy worktree checkout restored");
    assert_eq!(restored, legacy_path);
    assert_eq!(
        std::fs::read_to_string(legacy_path.join("legacy.txt")).unwrap(),
        "legacy-committed\n"
    );

    std::fs::remove_file(&sess_file).unwrap();
    ws::remove_worktree_with_env(&home, &legacy_path, true, &env).unwrap();
    ws::remove_worktree_with_env(&home, &wt1_path, true, &env).unwrap();
    ws::remove_worktree_with_env(&home, &wt2_path, true, &env).unwrap();
    assert!(!repo.join(".tiechui/worktrees").exists());
    workspace.close();
}

fn outside_dir(temp: &tempfile::TempDir) -> PathBuf {
    let dir = temp.path().join("outside-legacy-port");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
