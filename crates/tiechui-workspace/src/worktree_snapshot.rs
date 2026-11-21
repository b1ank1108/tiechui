use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use crate::worktree_script::{WaitOutcome, wait_for_child_until};
use crate::{Result, fail, git, worktree_carry};

pub const DEFAULT_SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const PIPE_RECV_GRACE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSnapshot {
    pub reference: String,
    pub tree: String,
}

pub fn checkpoint_ref_name(session_id: &str, anchor_id: &str) -> String {
    format!(
        "refs/tiechui/checkpoints/{}/{}",
        sanitize_ref_component(session_id),
        sanitize_ref_component(anchor_id)
    )
}

fn sanitize_ref_component(raw: &str) -> String {
    if raw.is_empty() {
        return "_".into();
    }
    let mut out = String::with_capacity(raw.len());
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || byte == b'-' {
            out.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "_{byte:02x}");
        }
    }
    out
}

pub fn capture_worktree_snapshot(
    worktree_root: &Path,
    session_id: &str,
    anchor_id: &str,
) -> Result<WorktreeSnapshot> {
    capture_worktree_snapshot_with_env(
        worktree_root,
        session_id,
        anchor_id,
        DEFAULT_SNAPSHOT_TIMEOUT,
        &BTreeMap::new(),
    )
}

pub fn capture_worktree_snapshot_with_env(
    worktree_root: &Path,
    session_id: &str,
    anchor_id: &str,
    timeout: Duration,
    env: &BTreeMap<String, String>,
) -> Result<WorktreeSnapshot> {
    capture_worktree_snapshot_cancellable(worktree_root, session_id, anchor_id, timeout, env, None)
}

pub fn capture_worktree_snapshot_cancellable(
    worktree_root: &Path,
    session_id: &str,
    anchor_id: &str,
    timeout: Duration,
    env: &BTreeMap<String, String>,
    is_cancelled: Option<&dyn Fn() -> bool>,
) -> Result<WorktreeSnapshot> {
    let runner = BoundedGit::new(timeout, is_cancelled);
    let toplevel = resolve_toplevel(&runner, worktree_root, env)?;
    let tree = build_snapshot_tree(&runner, &toplevel, env)?;
    let reference = checkpoint_ref_name(session_id, anchor_id);
    runner.run(&toplevel, &["update-ref", &reference, &tree], None, env)?;
    Ok(WorktreeSnapshot { reference, tree })
}

pub fn has_worktree_snapshot_tree(
    worktree_root: &Path,
    tree: &str,
    env: &BTreeMap<String, String>,
) -> bool {
    if !is_hex_oid(tree) {
        return false;
    }
    let runner = BoundedGit::new(DEFAULT_SNAPSHOT_TIMEOUT, None);
    let real_env = env_without_index(env);
    resolve_toplevel(&runner, worktree_root, &real_env)
        .and_then(|toplevel| verify_snapshot_tree(&runner, &toplevel, tree, &real_env))
        .is_ok()
}

pub fn restore_worktree_snapshot(worktree_root: &Path, tree: &str) -> Result<()> {
    restore_worktree_snapshot_with_env(
        worktree_root,
        tree,
        DEFAULT_SNAPSHOT_TIMEOUT,
        &BTreeMap::new(),
    )
}

pub fn restore_worktree_snapshot_with_env(
    worktree_root: &Path,
    tree: &str,
    timeout: Duration,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    restore_worktree_snapshot_cancellable(worktree_root, tree, timeout, env, None)
}

pub fn restore_worktree_snapshot_cancellable(
    worktree_root: &Path,
    tree: &str,
    timeout: Duration,
    env: &BTreeMap<String, String>,
    is_cancelled: Option<&dyn Fn() -> bool>,
) -> Result<()> {
    let runner = BoundedGit::new(timeout, is_cancelled);
    let real_env = env_without_index(env);
    let toplevel = resolve_toplevel(&runner, worktree_root, &real_env)?;
    verify_snapshot_tree(&runner, &toplevel, tree, &real_env)?;
    let stale_candidates = collect_tracked_and_cached_paths(&runner, &toplevel, &real_env)?;
    let (all_tree_paths, checkout_paths) =
        list_snapshot_tree_entries(&runner, &toplevel, tree, &real_env)?;
    checkout_snapshot_files(&runner, &toplevel, tree, &checkout_paths, &real_env)?;
    seed_index(&runner, &toplevel, &real_env)?;
    remove_paths_absent_from_tree(&toplevel, &stale_candidates, &all_tree_paths)?;
    prune_untracked_non_ignored(&runner, &toplevel, &all_tree_paths, &real_env)?;
    let _ = runner.run(
        &toplevel,
        &["update-index", "-q", "--refresh"],
        None,
        &real_env,
    );
    Ok(())
}

/// Writes `paths` from `tree` with their Git modes, symlinks included, leaving the index and
/// every other file alone. Each path must be a file or symlink in `tree`.
pub fn checkout_tree_paths(worktree_root: &Path, tree: &str, paths: &[String]) -> Result<()> {
    let runner = BoundedGit::new(DEFAULT_SNAPSHOT_TIMEOUT, None);
    let env = env_without_index(&BTreeMap::new());
    let toplevel = resolve_toplevel(&runner, worktree_root, &env)?;
    checkout_snapshot_files(&runner, &toplevel, tree, paths, &env)
}

pub fn diff_worktree_snapshot_with_env(
    worktree_root: &Path,
    tree: &str,
    timeout: Duration,
    env: &BTreeMap<String, String>,
    is_cancelled: Option<&dyn Fn() -> bool>,
) -> Result<Vec<String>> {
    let runner = BoundedGit::new(timeout, is_cancelled);
    let real_env = env_without_index(env);
    let toplevel = resolve_toplevel(&runner, worktree_root, &real_env)?;
    verify_snapshot_tree(&runner, &toplevel, tree, &real_env)?;
    let temp_dir = worktree_carry::TempIndexDir::create()?;
    let idx_env = worktree_carry::env_with_index(&real_env, &temp_dir.path.join("diff.index"))?;
    runner.run(&toplevel, &["read-tree", tree], None, &idx_env)?;
    let porcelain = read_porcelain_status(&runner, &toplevel, &idx_env)?;
    let paths: BTreeSet<String> = porcelain
        .split('\0')
        .filter(|record| record.len() > 3 && record.as_bytes()[1] != b' ')
        .map(|record| record[3..].to_owned())
        .collect();
    Ok(paths.into_iter().collect())
}

fn is_hex_oid(raw: &str) -> bool {
    !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_hexdigit())
}

fn env_without_index(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut clean = env.clone();
    clean.remove("GIT_INDEX_FILE");
    clean
}

fn verify_snapshot_tree(
    runner: &BoundedGit<'_>,
    toplevel: &Path,
    tree: &str,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    if !is_hex_oid(tree) {
        return fail(format!("invalid snapshot tree oid: {tree}"));
    }
    let spec = format!("{tree}^{{tree}}");
    runner
        .run(toplevel, &["rev-parse", "--verify", "-q", &spec], None, env)
        .map(|_| ())
}

fn collect_tracked_and_cached_paths(
    runner: &BoundedGit<'_>,
    toplevel: &Path,
    env: &BTreeMap<String, String>,
) -> Result<BTreeSet<String>> {
    let cached = runner.run(toplevel, &["ls-files", "-z", "--cached"], None, env)?;
    let mut paths: BTreeSet<String> = cached
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    if runner
        .run(
            toplevel,
            &["rev-parse", "--verify", "-q", "HEAD^{tree}"],
            None,
            env,
        )
        .is_ok()
    {
        let head_out = runner.run(
            toplevel,
            &["ls-tree", "-r", "-z", "--name-only", "HEAD"],
            None,
            env,
        )?;
        paths.extend(
            head_out
                .split('\0')
                .filter(|entry| !entry.is_empty())
                .map(str::to_owned),
        );
    }
    Ok(paths)
}

fn list_snapshot_tree_entries(
    runner: &BoundedGit<'_>,
    toplevel: &Path,
    tree: &str,
    env: &BTreeMap<String, String>,
) -> Result<(BTreeSet<String>, Vec<String>)> {
    let raw = runner.run(toplevel, &["ls-tree", "-r", "-z", tree], None, env)?;
    let mut all_paths = BTreeSet::new();
    let mut checkout_paths = Vec::new();
    for record in raw.split('\0').filter(|entry| !entry.is_empty()) {
        let Some((meta, path)) = record.split_once('\t') else {
            continue;
        };
        all_paths.insert(path.to_owned());
        if meta.split_whitespace().next() != Some("160000") {
            checkout_paths.push(path.to_owned());
        }
    }
    Ok((all_paths, checkout_paths))
}

fn checkout_snapshot_files(
    runner: &BoundedGit<'_>,
    toplevel: &Path,
    tree: &str,
    checkout_paths: &[String],
    env: &BTreeMap<String, String>,
) -> Result<()> {
    if checkout_paths.is_empty() {
        return Ok(());
    }
    let temp_dir = worktree_carry::TempIndexDir::create()?;
    let idx_env = worktree_carry::env_with_index(env, &temp_dir.path.join("restore.index"))?;
    runner.run(toplevel, &["read-tree", tree], None, &idx_env)?;
    for rel in checkout_paths {
        worktree_carry::prepare_checkout_target(toplevel, rel)?;
    }
    let stdin_bytes = worktree_carry::encode_nul_paths(&[], checkout_paths);
    runner.run(
        toplevel,
        &["checkout-index", "-f", "-z", "--stdin"],
        Some(&stdin_bytes),
        &idx_env,
    )?;
    Ok(())
}

fn remove_paths_absent_from_tree(
    toplevel: &Path,
    candidates: &BTreeSet<String>,
    all_tree_paths: &BTreeSet<String>,
) -> Result<()> {
    for rel in candidates {
        if !all_tree_paths.contains(rel) {
            worktree_carry::remove_worktree_path(toplevel, rel)?;
        }
    }
    Ok(())
}

fn prune_untracked_non_ignored(
    runner: &BoundedGit<'_>,
    toplevel: &Path,
    all_tree_paths: &BTreeSet<String>,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    if remove_other_files_once(runner, toplevel, all_tree_paths, env)? {
        let _ = remove_other_files_once(runner, toplevel, all_tree_paths, env)?;
    }
    Ok(())
}

fn remove_other_files_once(
    runner: &BoundedGit<'_>,
    toplevel: &Path,
    all_tree_paths: &BTreeSet<String>,
    env: &BTreeMap<String, String>,
) -> Result<bool> {
    let others = runner.run(
        toplevel,
        &["ls-files", "-z", "--others", "--exclude-standard"],
        None,
        env,
    )?;
    let mut removed_gitignore = false;
    for rel in others.split('\0').filter(|entry| !entry.is_empty()) {
        if all_tree_paths.contains(rel) {
            continue;
        }
        if Path::new(rel)
            .file_name()
            .is_some_and(|n| n == ".gitignore")
        {
            removed_gitignore = true;
        }
        worktree_carry::remove_worktree_path(toplevel, rel)?;
    }
    Ok(removed_gitignore)
}

fn build_snapshot_tree(
    runner: &BoundedGit<'_>,
    toplevel: &Path,
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let temp_dir = worktree_carry::TempIndexDir::create()?;
    let idx_env = worktree_carry::env_with_index(env, &temp_dir.path.join("capture.index"))?;
    seed_index(runner, toplevel, &idx_env)?;
    let porcelain = read_porcelain_status(runner, toplevel, &idx_env)?;
    let (deleted, non_deleted) = worktree_carry::collect_status_paths(toplevel, &porcelain, false)?;
    stage_changes(runner, toplevel, &idx_env, &deleted, &non_deleted)?;
    Ok(runner
        .run(toplevel, &["write-tree"], None, &idx_env)?
        .trim()
        .to_owned())
}

fn read_porcelain_status(
    runner: &BoundedGit<'_>,
    toplevel: &Path,
    idx_env: &BTreeMap<String, String>,
) -> Result<String> {
    runner.run(
        toplevel,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--no-renames",
            "--untracked-files=all",
            "--ignore-submodules=all",
        ],
        None,
        idx_env,
    )
}

fn resolve_toplevel(
    runner: &BoundedGit<'_>,
    worktree_root: &Path,
    env: &BTreeMap<String, String>,
) -> Result<PathBuf> {
    let raw = runner.run(worktree_root, &["rev-parse", "--show-toplevel"], None, env)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return fail(format!(
            "{} is not inside a Git work tree",
            worktree_root.display()
        ));
    }
    Ok(PathBuf::from(trimmed))
}

fn seed_index(
    runner: &BoundedGit<'_>,
    toplevel: &Path,
    idx_env: &BTreeMap<String, String>,
) -> Result<()> {
    match runner.run(
        toplevel,
        &["rev-parse", "--verify", "-q", "HEAD^{tree}"],
        None,
        idx_env,
    ) {
        Ok(head_tree) => {
            runner.run(toplevel, &["read-tree", head_tree.trim()], None, idx_env)?;
        }
        Err(_) => {
            runner.run(toplevel, &["read-tree", "--empty"], None, idx_env)?;
        }
    }
    Ok(())
}

fn stage_changes(
    runner: &BoundedGit<'_>,
    toplevel: &Path,
    idx_env: &BTreeMap<String, String>,
    deleted: &[String],
    non_deleted: &[String],
) -> Result<()> {
    if deleted.is_empty() && non_deleted.is_empty() {
        return Ok(());
    }
    let stdin_bytes = worktree_carry::encode_nul_paths(deleted, non_deleted);
    runner.run(
        toplevel,
        &["update-index", "--add", "--remove", "-z", "--stdin"],
        Some(&stdin_bytes),
        idx_env,
    )?;
    Ok(())
}

struct BoundedGit<'a> {
    deadline: Instant,
    timeout: Duration,
    is_cancelled: Option<&'a dyn Fn() -> bool>,
}

impl<'a> BoundedGit<'a> {
    fn new(timeout: Duration, is_cancelled: Option<&'a dyn Fn() -> bool>) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            timeout,
            is_cancelled,
        }
    }

    fn run(
        &self,
        root: &Path,
        args: &[&str],
        stdin_bytes: Option<&[u8]>,
        env: &BTreeMap<String, String>,
    ) -> Result<String> {
        if self.is_cancelled.is_some_and(|cancelled| cancelled()) {
            return fail("git snapshot cancelled");
        }
        if Instant::now() >= self.deadline {
            return fail(format!(
                "git snapshot timed out after {}ms",
                self.timeout.as_millis()
            ));
        }
        let mut child = spawn_git_process(root, args, stdin_bytes.is_some(), env)?;
        let (stdout_rx, stderr_rx) = attach_child_io(&mut child, stdin_bytes);
        match wait_for_child_until(&mut child, self.deadline, self.is_cancelled) {
            WaitOutcome::Exited(status) => {
                let stdout = recv_pipe(stdout_rx);
                let stderr = recv_pipe(stderr_rx);
                Ok(git::decode_status_output(status, &stdout, &stderr)?.0)
            }
            WaitOutcome::TimedOut => fail(format!(
                "git snapshot timed out after {}ms",
                self.timeout.as_millis()
            )),
            WaitOutcome::Cancelled => fail("git snapshot cancelled"),
            WaitOutcome::Failed(err) => Err(err.into()),
        }
    }
}

fn spawn_git_process(
    root: &Path,
    args: &[&str],
    piped_stdin: bool,
    env: &BTreeMap<String, String>,
) -> Result<Child> {
    let mut cmd = git::command("git", root, args, env);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    if piped_stdin {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    Ok(cmd.spawn()?)
}

type PipeRx = Option<Receiver<Vec<u8>>>;

fn attach_child_io(child: &mut Child, stdin_bytes: Option<&[u8]>) -> (PipeRx, PipeRx) {
    if let (Some(mut stdin_pipe), Some(bytes)) = (child.stdin.take(), stdin_bytes) {
        let payload = bytes.to_vec();
        std::thread::spawn(move || {
            let _ = stdin_pipe.write_all(&payload);
        });
    }
    (
        child.stdout.take().map(spawn_pipe_reader),
        child.stderr.take().map(spawn_pipe_reader),
    )
}

fn spawn_pipe_reader(mut pipe: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx
}

fn recv_pipe(rx: PipeRx) -> Vec<u8> {
    rx.and_then(|ch| ch.recv_timeout(PIPE_RECV_GRACE).ok())
        .unwrap_or_default()
}
