#![allow(clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tiechui_protocol::workspace::PortStopParams;
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
    assert!(wt_main_path.starts_with(home.join("worktrees")));
    let bucket_name = wt_main_path
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy();
    assert!(
        bucket_name.starts_with("my-repo-"),
        "expected <project_dir_name>-<hash>, got {bucket_name}"
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
    let listener_cmd = SpawnedListener::start(&outside, Some(&wt_script));
    let listener_cwd = SpawnedListener::start(&wt_a_path, None);
    let listener_manual = SpawnedListener::start(&manual_wt, None);

    let listed = workspace
        .list_ports(&[a_str.clone(), b_str.clone()])
        .unwrap();
    assert!(
        listed.iter().any(|e| {
            e.root == a_str && e.pid == listener_cmd.pid && e.port == listener_cmd.port
        })
    );
    #[cfg(not(windows))]
    assert!(
        listed.iter().any(|e| {
            e.root == a_str && e.pid == listener_cwd.pid && e.port == listener_cwd.port
        })
    );
    assert!(listed.iter().all(|e| e.pid != listener_manual.pid));

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
    let _ = listener_cwd;
    workspace.close();
}
