use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Result, fail, git};

const MAX_CARRY_FILES: usize = 2_000;
const MAX_CARRY_BYTES: u64 = 200 * 1024 * 1024;

pub(crate) struct CarriedSnapshot {
    temp_dir: TempIndexDir,
    tree_oid: Option<String>,
    deleted: Vec<String>,
    non_deleted: Vec<String>,
}

struct TempIndexDir {
    path: PathBuf,
}

impl TempIndexDir {
    fn create() -> Result<Self> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();
        let pid = std::process::id();
        for _ in 0..32 {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let candidate = base.join(format!("tiechui-wt-idx-{pid}-{seq}-{nanos}"));
            if !candidate.exists() {
                tiechui_config::private::create_dir_all(&candidate)?;
                return Ok(Self { path: candidate });
            }
        }
        fail("failed to allocate temporary Git index directory")
    }
}

impl Drop for TempIndexDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn capture_uncommitted(
    project_root: &Path,
    env: &BTreeMap<String, String>,
) -> Result<CarriedSnapshot> {
    let toplevel = git_toplevel(project_root, env)?;
    let temp_dir = TempIndexDir::create()?;
    let capture_index = temp_dir.path.join("capture.index");
    let idx_env = env_with_index(env, &capture_index)?;
    git::git(&toplevel, &["read-tree", "HEAD"], &idx_env)?;
    let porcelain = git::git(
        &toplevel,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=all",
        ],
        &idx_env,
    )?;
    let (deleted, non_deleted) = collect_changed_paths(&toplevel, &porcelain)?;
    let tree_oid = stage_carried_tree(&toplevel, &idx_env, &deleted, &non_deleted)?;
    Ok(CarriedSnapshot {
        temp_dir,
        tree_oid,
        deleted,
        non_deleted,
    })
}

fn stage_carried_tree(
    toplevel: &Path,
    idx_env: &BTreeMap<String, String>,
    deleted: &[String],
    non_deleted: &[String],
) -> Result<Option<String>> {
    if non_deleted.is_empty() {
        return Ok(None);
    }
    let stdin_bytes = encode_nul_paths(deleted, non_deleted);
    git::git_with_stdin(
        toplevel,
        &["update-index", "--add", "--remove", "-z", "--stdin"],
        &stdin_bytes,
        idx_env,
    )?;
    let tree = git::git(toplevel, &["write-tree"], idx_env)?;
    Ok(Some(tree.trim().to_owned()))
}

impl CarriedSnapshot {
    pub(crate) fn replay(&self, wt_path: &Path, env: &BTreeMap<String, String>) -> Result<()> {
        for rel in &self.deleted {
            remove_worktree_path(wt_path, rel)?;
        }
        let Some(tree_oid) = &self.tree_oid else {
            return Ok(());
        };
        for rel in &self.non_deleted {
            prepare_checkout_target(wt_path, rel)?;
        }
        let replay_index = self.temp_dir.path.join("replay.index");
        let idx_env = env_with_index(env, &replay_index)?;
        git::git(wt_path, &["read-tree", tree_oid], &idx_env)?;
        let stdin_bytes = encode_nul_paths(&[], &self.non_deleted);
        git::git_with_stdin(
            wt_path,
            &["checkout-index", "-f", "-z", "--stdin"],
            &stdin_bytes,
            &idx_env,
        )?;
        Ok(())
    }
}

fn git_toplevel(project_root: &Path, env: &BTreeMap<String, String>) -> Result<PathBuf> {
    let raw = git::git(project_root, &["rev-parse", "--show-toplevel"], env)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return fail(format!(
            "{} is not inside a Git work tree",
            project_root.display()
        ));
    }
    Ok(PathBuf::from(trimmed))
}

fn env_with_index(
    base: &BTreeMap<String, String>,
    index_path: &Path,
) -> Result<BTreeMap<String, String>> {
    let raw = index_path
        .to_str()
        .ok_or_else(|| crate::Error(format!("non-UTF-8 path {}", index_path.display())))?;
    let mut next = base.clone();
    next.insert("GIT_INDEX_FILE".into(), raw.to_owned());
    Ok(next)
}

enum EntryState {
    Skip,
    Deleted,
    Present(u64),
}

fn classify_entry(toplevel: &Path, status: &str, rel: &str) -> Result<EntryState> {
    if status.ends_with('D') {
        return Ok(EntryState::Deleted);
    }
    match fs::symlink_metadata(toplevel.join(rel)) {
        Ok(meta) if meta.is_dir() => Ok(EntryState::Skip),
        Ok(meta) => Ok(EntryState::Present(meta.len())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(EntryState::Deleted),
        Err(err) => Err(err.into()),
    }
}

fn check_carry_limits(file_count: usize, total_bytes: u64) -> Result<()> {
    if file_count > MAX_CARRY_FILES {
        return fail(format!(
            "uncommitted changes exceed the {MAX_CARRY_FILES}-file limit for worktree carry"
        ));
    }
    if total_bytes > MAX_CARRY_BYTES {
        return fail("uncommitted changes exceed the 200 MiB size limit for worktree carry");
    }
    Ok(())
}

fn collect_changed_paths(toplevel: &Path, porcelain: &str) -> Result<(Vec<String>, Vec<String>)> {
    let mut deleted = Vec::new();
    let mut non_deleted = Vec::new();
    let mut file_count = 0_usize;
    let mut total_bytes = 0_u64;
    for record in porcelain.split('\0').filter(|r| r.len() > 3) {
        let status = &record[..2];
        let rel = &record[3..];
        match classify_entry(toplevel, status, rel)? {
            EntryState::Skip => {}
            EntryState::Deleted => {
                file_count += 1;
                check_carry_limits(file_count, total_bytes)?;
                deleted.push(rel.to_owned());
            }
            EntryState::Present(len) => {
                file_count += 1;
                total_bytes = total_bytes.saturating_add(len);
                check_carry_limits(file_count, total_bytes)?;
                non_deleted.push(rel.to_owned());
            }
        }
    }
    Ok((deleted, non_deleted))
}

fn encode_nul_paths(first: &[String], second: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for path in first.iter().chain(second.iter()) {
        out.extend_from_slice(path.as_bytes());
        out.push(0);
    }
    out
}

fn safe_join_rel(wt_path: &Path, rel: &str) -> Result<PathBuf> {
    let mut joined = wt_path.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(part) => joined.push(part),
            _ => return fail(format!("unexpected relative path in Git status: {rel}")),
        }
    }
    Ok(joined)
}

fn remove_worktree_path(wt_path: &Path, rel: &str) -> Result<()> {
    let target = safe_join_rel(wt_path, rel)?;
    if !delete_existing_entry(&target)? {
        return Ok(());
    }
    prune_empty_ancestors(wt_path, target.parent());
    Ok(())
}

fn delete_existing_entry(target: &Path) -> Result<bool> {
    match fs::symlink_metadata(target) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(target)
            .map(|()| true)
            .map_err(Into::into),
        Ok(_) => fs::remove_file(target).map(|()| true).map_err(Into::into),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn prune_empty_ancestors(wt_path: &Path, mut current: Option<&Path>) {
    while let Some(dir) = current {
        if dir == wt_path || fs::remove_dir(dir).is_err() {
            break;
        }
        current = dir.parent();
    }
}

fn prepare_checkout_target(wt_path: &Path, rel: &str) -> Result<()> {
    let target = safe_join_rel(wt_path, rel)?;
    if let Some(parent) = Path::new(rel).parent() {
        clear_non_dir_ancestors(wt_path, parent)?;
    }
    clear_dir_if_exists(&target)
}

fn clear_non_dir_ancestors(wt_path: &Path, rel_parent: &Path) -> Result<()> {
    let mut cursor = wt_path.to_path_buf();
    for comp in rel_parent.components() {
        if let Component::Normal(part) = comp {
            cursor.push(part);
            clear_non_dir_if_exists(&cursor)?;
        }
    }
    Ok(())
}

fn clear_non_dir_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if !meta.is_dir() => fs::remove_file(path).map_err(Into::into),
        _ => Ok(()),
    }
}

fn clear_dir_if_exists(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path).map_err(Into::into),
        _ => Ok(()),
    }
}
