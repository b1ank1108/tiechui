use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tiechui_protocol::app::SessionWorktree;
use tiechui_protocol::workspace::{PostCreateResult, WorktreeEntry, WorktreeRemoveResult};

use crate::ports::{clean_path, resolve_existing_prefix};
use crate::{Result, fail, git, lock, worktree_carry, worktree_script};

const LOCK_FILENAME: &str = "tiechui-worktree.lock";
const REGISTRY_FILENAME: &str = "registry.json";
const REGISTRY_LOCK_FILENAME: &str = "registry.lock";

#[derive(Debug, Clone, Default)]
pub struct CreateWorktreeOptions {
    pub base_branch: Option<String>,
    pub carry_changes: bool,
    pub post_create_timeout: Option<Duration>,
}

#[derive(Clone)]
pub(crate) struct Record {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) project_root: String,
    pub(crate) branch: String,
    pub(crate) base_branch: String,
    pub(crate) created_at: u64,
    pub(crate) post_create: PostCreateResult,
}

impl Record {
    fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "path": self.path,
            "project_root": self.project_root,
            "branch": self.branch,
            "base_branch": self.base_branch,
            "created_at": self.created_at,
            "post_create": self.post_create,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        let post_create = value
            .get("post_create")
            .and_then(|raw| serde_json::from_value(raw.clone()).ok())
            .unwrap_or_default();
        Some(Self {
            name: str_field(value, "name")?,
            path: str_field(value, "path")?,
            project_root: str_field(value, "project_root")?,
            branch: str_field(value, "branch")?,
            base_branch: str_field(value, "base_branch")?,
            created_at: value.get("created_at")?.as_u64()?,
            post_create,
        })
    }

    pub(crate) fn live_branch(&self) -> String {
        read_head_branch(Path::new(&self.path)).unwrap_or_else(|| self.branch.clone())
    }
}

fn str_field(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

pub fn create_worktree_with_env(
    home: &Path,
    project_root: &Path,
    base_branch: Option<&str>,
    env: &BTreeMap<String, String>,
) -> Result<WorktreeEntry> {
    create_worktree_with_options(
        home,
        project_root,
        CreateWorktreeOptions {
            base_branch: base_branch.map(str::to_owned),
            carry_changes: false,
            post_create_timeout: None,
        },
        env,
    )
}

pub fn create_worktree_with_options(
    home: &Path,
    project_root: &Path,
    options: CreateWorktreeOptions,
    env: &BTreeMap<String, String>,
) -> Result<WorktreeEntry> {
    let clean_project = validate_project_dir(project_root)?;
    let common_dir = git_common_dir(&clean_project, env)?;
    let canonical_project = clean_project.canonicalize()?;
    let bucket_dir = worktrees_dir(home).join(bucket_name(&clean_project, &canonical_project));
    let mut record = with_file_lock(&common_dir.join(LOCK_FILENAME), || {
        create_locked(home, &clean_project, &bucket_dir, &options, env)
    })?;
    let wt_path = PathBuf::from(&record.path);
    let post_create = worktree_script::run_project_post_create(
        home,
        &clean_project,
        &wt_path,
        options.post_create_timeout,
        env,
    );
    if post_create != PostCreateResult::default() {
        record.post_create = post_create;
        let _ = update_record_post_create(home, &record);
    }
    let dirty = is_worktree_dirty(&wt_path, env).unwrap_or(false);
    let branch = record.live_branch();
    Ok(WorktreeEntry {
        name: record.name,
        path: record.path,
        project_root: record.project_root,
        branch,
        base_branch: record.base_branch,
        dirty,
        created_at: record.created_at,
        post_create: record.post_create,
    })
}

fn create_locked(
    home: &Path,
    clean_project: &Path,
    bucket_dir: &Path,
    options: &CreateWorktreeOptions,
    env: &BTreeMap<String, String>,
) -> Result<Record> {
    let (base_label, start_point) =
        resolve_base(clean_project, options.base_branch.as_deref(), env)?;
    let carried = capture_optional_changes(clean_project, options.carry_changes, env)?;
    ensure_private_dirs(home, bucket_dir)?;
    let created_at = now_ms();
    let (name, branch, wt_path) = pick_worktree_slot(clean_project, bucket_dir, created_at, env)?;
    let record = Record {
        name,
        path: path_str(&wt_path)?.to_owned(),
        project_root: clean_project.display().to_string(),
        branch,
        base_branch: base_label,
        created_at,
        post_create: PostCreateResult::default(),
    };
    checkout_and_register(home, &start_point, carried.as_ref(), &record, env)?;
    Ok(record)
}

fn capture_optional_changes(
    clean_project: &Path,
    carry_changes: bool,
    env: &BTreeMap<String, String>,
) -> Result<Option<worktree_carry::CarriedSnapshot>> {
    if !carry_changes {
        return Ok(None);
    }
    worktree_carry::capture_uncommitted(clean_project, env).map(Some)
}

fn checkout_and_register(
    home: &Path,
    start_point: &str,
    carried: Option<&worktree_carry::CarriedSnapshot>,
    record: &Record,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    let clean_project = Path::new(&record.project_root);
    let wt_path = Path::new(&record.path);
    git::git(
        clean_project,
        &[
            "worktree",
            "add",
            "-b",
            &record.branch,
            &record.path,
            start_point,
        ],
        env,
    )?;
    let result =
        finish_checkout(wt_path, carried, env).and_then(|()| register_record(home, record.clone()));
    if result.is_err() {
        rollback_checkout(clean_project, wt_path, &record.branch, env);
        if let Some(bucket_dir) = wt_path.parent() {
            let _ = fs::remove_dir(bucket_dir);
        }
    }
    result
}

fn finish_checkout(
    wt_path: &Path,
    carried: Option<&worktree_carry::CarriedSnapshot>,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    if let Some(snapshot) = carried {
        snapshot.replay(wt_path, env)?;
    }
    tighten_dirs_recursive(wt_path)
}

pub fn list_worktrees_with_env(
    home: &Path,
    project_root: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> Result<Vec<WorktreeEntry>> {
    let filter_key = match project_root {
        Some(root) => {
            if !root.is_absolute() {
                return fail(format!("{} is not an absolute path", root.display()));
            }
            Some(resolve_existing_prefix(root))
        }
        None => None,
    };
    let records = read_registry_locked(home)?;
    let mut entries = Vec::new();
    for record in records {
        if let Some(expected) = &filter_key
            && !crate::win_path::paths_equal(
                &resolve_existing_prefix(Path::new(&record.project_root)),
                expected,
            )
        {
            continue;
        }
        let wt_path = Path::new(&record.path);
        if !wt_path.is_dir() {
            continue;
        }
        let branch = record.live_branch();
        let dirty = is_worktree_dirty(wt_path, env)?;
        entries.push(WorktreeEntry {
            name: record.name,
            path: record.path,
            project_root: record.project_root,
            branch,
            base_branch: record.base_branch,
            dirty,
            created_at: record.created_at,
            post_create: record.post_create,
        });
    }
    Ok(entries)
}

pub fn remove_worktree_with_env(
    home: &Path,
    path: &Path,
    force: bool,
    env: &BTreeMap<String, String>,
) -> Result<WorktreeRemoveResult> {
    let (target_key, record) = find_managed_record(home, path)?;
    let wt_path = PathBuf::from(&record.path);
    let project_path = PathBuf::from(&record.project_root);
    let lock_path = locate_common_dir(&wt_path, &project_path, env).map(|d| d.join(LOCK_FILENAME));
    with_optional_lock(lock_path.as_deref(), || {
        remove_locked(home, &record, &target_key, force, env)
    })
}

fn find_managed_record(home: &Path, path: &Path) -> Result<(PathBuf, Record)> {
    if !path.is_absolute() {
        return fail(format!("{} is not an absolute path", path.display()));
    }
    let target_key = resolve_existing_prefix(path);
    let record = read_registry_locked(home)?
        .into_iter()
        .find(|item| {
            crate::win_path::paths_equal(
                &resolve_existing_prefix(Path::new(&item.path)),
                &target_key,
            )
        })
        .ok_or_else(|| crate::Error(format!("{} is not a managed worktree", path.display())))?;
    Ok((target_key, record))
}

fn remove_locked(
    home: &Path,
    record: &Record,
    target_key: &Path,
    force: bool,
    env: &BTreeMap<String, String>,
) -> Result<WorktreeRemoveResult> {
    let wt_path = Path::new(&record.path);
    let project_path = Path::new(&record.project_root);
    if wt_path.exists() {
        if !force && is_worktree_dirty(wt_path, env)? {
            return fail("worktree has uncommitted changes");
        }
        delete_checkout(wt_path, project_path, env)?;
    }
    if let Some(bucket) = wt_path.parent() {
        let _ = fs::remove_dir(bucket);
    }
    if !has_referencing_disk_session(home, wt_path) {
        unregister_record(home, target_key)?;
    }
    Ok(WorktreeRemoveResult {
        path: record.path.clone(),
        branch: record.branch.clone(),
    })
}

fn has_referencing_disk_session(home: &Path, wt_path: &Path) -> bool {
    let wt_norm = crate::worktree_isolation::normalize_dir(wt_path);
    let Ok(headers) = crate::worktree_isolation::list_disk_session_headers(home) else {
        return false;
    };
    headers.into_iter().any(|header| {
        let sess_norm = crate::worktree_isolation::normalize_dir(&header.cwd);
        crate::worktree_isolation::classify_overlap(&wt_norm, &sess_norm).is_some()
    })
}

pub fn prune_orphan_worktree_records(home: &Path) -> Result<()> {
    let wt_dir = worktrees_dir(home);
    if !wt_dir.exists() {
        return Ok(());
    }
    with_file_lock(&wt_dir.join(REGISTRY_LOCK_FILENAME), || {
        let reg_path = wt_dir.join(REGISTRY_FILENAME);
        let mut records = load_registry_file(&reg_path)?;
        let before = records.len();
        records.retain(|item| {
            let path = Path::new(&item.path);
            path.exists() || has_referencing_disk_session(home, path)
        });
        if records.len() != before {
            save_registry_file(&wt_dir, &records)?;
        }
        Ok(())
    })
}

pub fn worktree_project_root(home: &Path, cwd: &Path) -> Option<PathBuf> {
    let candidate = resolve_existing_prefix(&clean_path(cwd));
    read_registry_unlocked(home).into_iter().find_map(|item| {
        let wt_key = resolve_existing_prefix(Path::new(&item.path));
        crate::win_path::path_starts_with(&candidate, &wt_key)
            .then(|| resolve_existing_prefix(Path::new(&item.project_root)))
    })
}

pub fn ensure_worktree_checkout(
    home: &Path,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Result<Option<PathBuf>> {
    let cleaned = clean_path(cwd);
    let target_key = resolve_existing_prefix(&cleaned);
    let Some(record) = read_registry_locked(home)?.into_iter().find(|item| {
        crate::win_path::path_starts_with(
            &target_key,
            &resolve_existing_prefix(Path::new(&item.path)),
        )
    }) else {
        return Ok(None);
    };
    let wt_path = PathBuf::from(&record.path);
    if wt_path.is_dir() {
        return Ok(Some(resolve_restored_subdir(&wt_path, &cleaned)));
    }
    let project_root = PathBuf::from(&record.project_root);
    if !project_root.is_dir() {
        return Ok(None);
    }
    let lock_path = git_common_dir(&project_root, env)
        .ok()
        .map(|d| d.join(LOCK_FILENAME));
    with_optional_lock(lock_path.as_deref(), || {
        recheckout_locked(home, &record, &project_root, &cleaned, env)
    })
}

fn recheckout_locked(
    home: &Path,
    record: &Record,
    project_root: &Path,
    requested: &Path,
    env: &BTreeMap<String, String>,
) -> Result<Option<PathBuf>> {
    let wt_path = Path::new(&record.path);
    if wt_path.is_dir() {
        return Ok(Some(resolve_restored_subdir(wt_path, requested)));
    }
    if !branch_exists(project_root, &record.branch, env) {
        return Ok(Some(project_root.to_path_buf()));
    }
    if let Some(bucket) = wt_path.parent() {
        ensure_private_dirs(home, bucket)?;
    }
    let _ = git::git(project_root, &["worktree", "prune"], env);
    git::git(
        project_root,
        &["worktree", "add", &record.path, &record.branch],
        env,
    )?;
    tighten_dirs_recursive(wt_path)?;
    Ok(Some(resolve_restored_subdir(wt_path, requested)))
}

fn resolve_restored_subdir(wt_path: &Path, requested: &Path) -> PathBuf {
    let wt_resolved = resolve_existing_prefix(&clean_path(wt_path));
    let req_resolved = resolve_existing_prefix(requested);
    if let Some(rel) = crate::win_path::path_strip_prefix(&req_resolved, &wt_resolved)
        && !rel.as_os_str().is_empty()
    {
        let sub = wt_path.join(rel);
        if sub.is_dir() {
            return sub;
        }
    }
    wt_path.to_path_buf()
}

pub fn project_worktree_paths(home: &Path, project_root: &Path) -> Vec<PathBuf> {
    let expected = resolve_existing_prefix(project_root);
    read_registry_unlocked(home)
        .into_iter()
        .filter(|item| {
            crate::win_path::paths_equal(
                &resolve_existing_prefix(Path::new(&item.project_root)),
                &expected,
            )
        })
        .map(|item| PathBuf::from(item.path))
        .collect()
}

pub fn session_worktree(home: &Path, cwd: &Path) -> Option<SessionWorktree> {
    let expected = resolve_existing_prefix(&clean_path(cwd));
    let record = read_registry_unlocked(home).into_iter().find(|item| {
        crate::win_path::path_starts_with(
            &expected,
            &resolve_existing_prefix(Path::new(&item.path)),
        )
    })?;
    let branch = record.live_branch();
    Some(SessionWorktree {
        path: record.path,
        name: record.name,
        branch,
        base_branch: record.base_branch,
    })
}

pub fn managed_worktree_with_env(
    home: &Path,
    cwd: &Path,
    env: &BTreeMap<String, String>,
) -> Result<Option<WorktreeEntry>> {
    let expected = resolve_existing_prefix(&clean_path(cwd));
    let Some(record) = read_registry_locked(home)?.into_iter().find(|item| {
        crate::win_path::path_starts_with(
            &expected,
            &resolve_existing_prefix(Path::new(&item.path)),
        )
    }) else {
        return Ok(None);
    };
    let wt_path = Path::new(&record.path);
    let dirty = if wt_path.is_dir() {
        is_worktree_dirty(wt_path, env)?
    } else {
        false
    };
    let branch = record.live_branch();
    Ok(Some(WorktreeEntry {
        name: record.name,
        path: record.path,
        project_root: record.project_root,
        branch,
        base_branch: record.base_branch,
        dirty,
        created_at: record.created_at,
        post_create: record.post_create,
    }))
}

fn validate_project_dir(project_root: &Path) -> Result<PathBuf> {
    if !project_root.is_absolute() {
        return fail(format!(
            "{} is not an absolute path",
            project_root.display()
        ));
    }
    if !project_root.is_dir() {
        return fail(format!("{} is not a directory", project_root.display()));
    }
    Ok(clean_path(project_root))
}

fn git_common_dir(root: &Path, env: &BTreeMap<String, String>) -> Result<PathBuf> {
    let out = git::git(
        root,
        &["rev-parse", "--is-inside-work-tree", "--git-common-dir"],
        env,
    )
    .map_err(|_| crate::Error(format!("{} is not a Git repository", root.display())))?;
    let mut lines = out.lines();
    let inside = lines.next().unwrap_or_default().trim();
    let common = lines.next().unwrap_or_default().trim();
    if inside != "true" || common.is_empty() {
        return fail(format!("{} is not inside a Git work tree", root.display()));
    }
    let joined = if Path::new(common).is_absolute() {
        PathBuf::from(common)
    } else {
        root.join(common)
    };
    Ok(crate::win_path::strip_verbatim_path(
        &joined.canonicalize()?,
    ))
}

fn locate_common_dir(
    wt_path: &Path,
    project_path: &Path,
    env: &BTreeMap<String, String>,
) -> Option<PathBuf> {
    if wt_path.is_dir()
        && let Ok(dir) = git_common_dir(wt_path, env)
    {
        return Some(dir);
    }
    if project_path.is_dir()
        && let Ok(dir) = git_common_dir(project_path, env)
    {
        return Some(dir);
    }
    None
}

fn resolve_base(
    project_root: &Path,
    requested: Option<&str>,
    env: &BTreeMap<String, String>,
) -> Result<(String, String)> {
    if let Some(base) = requested.map(str::trim).filter(|s| !s.is_empty()) {
        if base.starts_with('-') {
            return fail(format!("invalid base branch {base}"));
        }
        let rev = format!("{base}^{{commit}}");
        git::git(
            project_root,
            &["rev-parse", "--verify", "--quiet", &rev],
            env,
        )
        .map_err(|_| crate::Error(format!("unknown base branch or revision: {base}")))?;
        return Ok((base.to_owned(), base.to_owned()));
    }
    git::git(
        project_root,
        &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        env,
    )
    .map_err(|_| crate::Error("repository has no commits".into()))?;
    let label = git::git(
        project_root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        env,
    )
    .ok()
    .map(|s| s.trim().to_owned())
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| "HEAD".into());
    Ok((label, "HEAD".into()))
}

fn worktrees_dir(home: &Path) -> PathBuf {
    clean_path(home).join("worktrees")
}

fn bucket_name(clean_project: &Path, canonical_project: &Path) -> String {
    let raw_name = clean_project
        .file_name()
        .or_else(|| canonical_project.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    let sanitized: String = raw_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches(|c| c == '-' || c == '.');
    let prefix = if trimmed.is_empty() { "repo" } else { trimmed };
    let mut hasher = Sha256::new();
    hasher.update(canonical_project.as_os_str().as_encoded_bytes());
    let digest = hasher.finalize();
    format!(
        "{prefix}-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

pub(crate) fn random_short_id(attempt: u32) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    hasher.update(seq.to_le_bytes());
    hasher.update(attempt.to_le_bytes());
    let digest = hasher.finalize();
    format!(
        "{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

fn pick_worktree_slot(
    project_root: &Path,
    bucket_dir: &Path,
    now_millis: u64,
    env: &BTreeMap<String, String>,
) -> Result<(String, String, PathBuf)> {
    let date = utc_yyyymmdd(now_millis);
    for attempt in 0..64_u32 {
        let short_id = random_short_id(attempt);
        let name = format!("{date}-{short_id}");
        let branch = format!("tiechui/{name}");
        let wt_path = bucket_dir.join(&name);
        if !wt_path.exists() && !branch_exists(project_root, &branch, env) {
            return Ok((name, branch, wt_path));
        }
    }
    fail("failed to allocate a unique worktree name")
}

fn branch_exists(project_root: &Path, branch: &str, env: &BTreeMap<String, String>) -> bool {
    let full_ref = format!("refs/heads/{branch}");
    git::git(
        project_root,
        &["show-ref", "--verify", "--quiet", &full_ref],
        env,
    )
    .is_ok()
}

fn is_worktree_dirty(wt_path: &Path, env: &BTreeMap<String, String>) -> Result<bool> {
    worktree_dirty_summary(wt_path, env).map(|summary| summary.is_some())
}

pub(crate) fn worktree_dirty_summary(
    wt_path: &Path,
    env: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    let out = git::git(
        wt_path,
        &["status", "--porcelain", "--untracked-files=all"],
        env,
    )?;
    let lines: Vec<&str> = out.lines().filter(|line| !line.trim().is_empty()).collect();
    if lines.is_empty() {
        return Ok(None);
    }
    let untracked = lines.iter().filter(|line| line.starts_with("?? ")).count();
    let tracked = lines.len().saturating_sub(untracked);
    Ok(Some(format_dirty_reason(tracked, untracked)))
}

fn format_dirty_reason(tracked: usize, untracked: usize) -> String {
    match (tracked > 0, untracked > 0) {
        (true, true) => {
            format!("存在未提交改动（{tracked} 处已跟踪修改，{untracked} 个未忽略新文件）")
        }
        (true, false) => format!("存在未提交改动（{tracked} 处已跟踪修改）"),
        (false, true) => format!("存在未提交改动（{untracked} 个未忽略新文件）"),
        (false, false) => "存在未提交改动".into(),
    }
}

fn delete_checkout(
    wt_path: &Path,
    project_path: &Path,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    #[cfg(windows)]
    {
        remove_worktree_dir_safely(wt_path)?;
        if project_path.is_dir() {
            let removed = wt_path.to_str().is_some_and(|wt_str| {
                git::git(
                    project_path,
                    &["worktree", "remove", "--force", "--force", "--", wt_str],
                    env,
                )
                .is_ok()
            });
            if !removed {
                let _ = git::git(project_path, &["worktree", "prune"], env);
            }
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let anchor = if project_path.is_dir() {
            project_path
        } else {
            wt_path
        };
        if let Some(wt_str) = wt_path.to_str() {
            let _ = git::git(anchor, &["worktree", "remove", "--force", wt_str], env);
        }
        if wt_path.exists() {
            remove_worktree_dir_safely(wt_path)?;
            if project_path.is_dir() {
                let _ = git::git(project_path, &["worktree", "prune"], env);
            }
        }
        Ok(())
    }
}

fn remove_worktree_dir_safely(wt_path: &Path) -> std::io::Result<()> {
    remove_tree_entry(wt_path, true)
}

fn remove_tree_entry(path: &Path, defer_dot_git: bool) -> std::io::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    if is_link_or_reparse(&meta) || !meta.is_dir() {
        return unlink_single_entry(path, &meta);
    }
    clear_readonly(path, &meta);
    unlink_dir_children(path, defer_dot_git)?;
    remove_with_readonly_retry(path, true)
}

fn unlink_dir_children(dir: &Path, defer_dot_git: bool) -> std::io::Result<()> {
    let mut deferred_dot_git = false;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if defer_dot_git && entry.file_name() == ".git" {
            deferred_dot_git = true;
            continue;
        }
        remove_tree_entry(&entry.path(), false)?;
    }
    if deferred_dot_git {
        remove_tree_entry(&dir.join(".git"), false)?;
    }
    Ok(())
}

fn is_link_or_reparse(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
        (meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT) != 0
            || meta.file_type().is_symlink()
    }
    #[cfg(not(windows))]
    {
        meta.file_type().is_symlink()
    }
}

fn unlink_single_entry(path: &Path, meta: &fs::Metadata) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
        let is_dir_entry = (meta.file_attributes() & FILE_ATTRIBUTE_DIRECTORY) != 0;
        remove_with_readonly_retry(path, is_dir_entry)
    }
    #[cfg(not(windows))]
    {
        let _ = meta;
        remove_with_readonly_retry(path, false)
    }
}

fn remove_with_readonly_retry(path: &Path, is_dir: bool) -> std::io::Result<()> {
    let remove = || {
        if is_dir {
            fs::remove_dir(path)
        } else {
            fs::remove_file(path)
        }
    };
    match remove() {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => {
            if let Ok(meta) = fs::symlink_metadata(path) {
                clear_readonly(path, &meta);
            }
            match remove() {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            }
        }
    }
}

fn clear_readonly(path: &Path, meta: &fs::Metadata) {
    #[cfg(windows)]
    if !is_link_or_reparse(meta) {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_READONLY, SetFileAttributesW,
        };
        let attrs = meta.file_attributes();
        if (attrs & FILE_ATTRIBUTE_READONLY) != 0 {
            let next = attrs & !FILE_ATTRIBUTE_READONLY;
            let next = if next == 0 {
                FILE_ATTRIBUTE_NORMAL
            } else {
                next
            };
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            // SAFETY: `wide` is a null-terminated UTF-16 buffer valid for the duration of the call.
            unsafe {
                let _ = SetFileAttributesW(wide.as_ptr(), next);
            }
        }
    }
    #[cfg(not(windows))]
    let _ = (path, meta);
}

fn rollback_checkout(
    project_root: &Path,
    wt_path: &Path,
    branch: &str,
    env: &BTreeMap<String, String>,
) {
    let _ = delete_checkout(wt_path, project_root, env);
    let _ = git::git(project_root, &["branch", "-D", branch], env);
}

fn ensure_private_dirs(home: &Path, bucket_dir: &Path) -> Result<()> {
    tiechui_config::private::create_dir_all(home)?;
    tiechui_config::private::create_dir_all(&worktrees_dir(home))?;
    tiechui_config::private::create_dir_all(bucket_dir)?;
    Ok(())
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn tighten_dirs_recursive(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::symlink_metadata(dir)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Ok(());
        }
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        for entry in fs::read_dir(dir)? {
            let child = entry?.path();
            tighten_dirs_recursive(&child)?;
        }
    }
    Ok(())
}

fn register_record(home: &Path, record: Record) -> Result<()> {
    let wt_dir = worktrees_dir(home);
    tiechui_config::private::create_dir_all(&wt_dir)?;
    with_file_lock(&wt_dir.join(REGISTRY_LOCK_FILENAME), || {
        let mut records = load_registry_file(&wt_dir.join(REGISTRY_FILENAME))?;
        let key = resolve_existing_prefix(Path::new(&record.path));
        records.retain(|item| {
            !crate::win_path::paths_equal(&resolve_existing_prefix(Path::new(&item.path)), &key)
        });
        records.push(record);
        save_registry_file(&wt_dir, &records)
    })
}

fn update_record_post_create(home: &Path, updated: &Record) -> Result<()> {
    let wt_dir = worktrees_dir(home);
    if !wt_dir.exists() {
        return Ok(());
    }
    with_file_lock(&wt_dir.join(REGISTRY_LOCK_FILENAME), || {
        let mut records = load_registry_file(&wt_dir.join(REGISTRY_FILENAME))?;
        let key = resolve_existing_prefix(Path::new(&updated.path));
        if let Some(slot) = records.iter_mut().find(|item| {
            crate::win_path::paths_equal(&resolve_existing_prefix(Path::new(&item.path)), &key)
        }) {
            slot.post_create = updated.post_create.clone();
            save_registry_file(&wt_dir, &records)?;
        }
        Ok(())
    })
}

fn unregister_record(home: &Path, target_key: &Path) -> Result<()> {
    let wt_dir = worktrees_dir(home);
    with_file_lock(&wt_dir.join(REGISTRY_LOCK_FILENAME), || {
        let reg_path = wt_dir.join(REGISTRY_FILENAME);
        let mut records = load_registry_file(&reg_path)?;
        records.retain(|item| {
            !crate::win_path::paths_equal(
                &resolve_existing_prefix(Path::new(&item.path)),
                target_key,
            )
        });
        save_registry_file(&wt_dir, &records)
    })
}

pub(crate) fn read_registry_locked(home: &Path) -> Result<Vec<Record>> {
    let wt_dir = worktrees_dir(home);
    if !wt_dir.exists() {
        return Ok(Vec::new());
    }
    with_file_lock(&wt_dir.join(REGISTRY_LOCK_FILENAME), || {
        load_registry_file(&wt_dir.join(REGISTRY_FILENAME))
    })
}

pub(crate) fn read_registry_unlocked(home: &Path) -> Vec<Record> {
    load_registry_file(&worktrees_dir(home).join(REGISTRY_FILENAME)).unwrap_or_default()
}

fn load_registry_file(path: &Path) -> Result<Vec<Record>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let root: Value = serde_json::from_slice(&bytes)?;
    let items = root
        .get("worktrees")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Record::from_json).collect())
        .unwrap_or_default();
    Ok(items)
}

fn save_registry_file(wt_dir: &Path, records: &[Record]) -> Result<()> {
    let payload = serde_json::to_vec_pretty(&json!({
        "version": 1,
        "worktrees": records.iter().map(Record::to_json).collect::<Vec<_>>(),
    }))?;
    let tmp_path = wt_dir.join(format!("{REGISTRY_FILENAME}.tmp.{}", std::process::id()));
    let _ = fs::remove_file(&tmp_path);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp_path)?;
    file.write_all(&payload)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp_path, wt_dir.join(REGISTRY_FILENAME))?;
    Ok(())
}

fn read_head_branch(wt_path: &Path) -> Option<String> {
    let dot_git = wt_path.join(".git");
    let git_dir = if dot_git.is_file() {
        let text = fs::read_to_string(&dot_git).ok()?;
        let raw = text.trim().strip_prefix("gitdir:")?.trim();
        let path = Path::new(raw);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            wt_path.join(path)
        }
    } else {
        dot_git
    };
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_owned)
}

fn with_optional_lock<T>(lock_path: Option<&Path>, work: impl FnOnce() -> Result<T>) -> Result<T> {
    match lock_path {
        Some(path) => with_file_lock(path, work),
        None => work(),
    }
}

fn with_file_lock<T>(lock_path: &Path, work: impl FnOnce() -> Result<T>) -> Result<T> {
    let key = crate::win_path::path_key(&resolve_existing_prefix(lock_path));
    let mutex = process_mutex(&key);
    let _guard = lock(&mutex);
    let file = tiechui_config::private::open_lock(lock_path)?;
    file.lock()?;
    let result = work();
    let _ = file.unlock();
    result
}

fn process_mutex(key: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let map = LOCKS.get_or_init(Mutex::default);
    lock(map)
        .entry(key.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| crate::Error(format!("non-UTF-8 path {}", path.display())))
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(crate) fn utc_ymd(millis: u64) -> (i64, i64, i64) {
    let days = (millis / 86_400_000) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

fn utc_yyyymmdd(millis: u64) -> String {
    let (year, m, d) = utc_ymd(millis);
    format!("{year:04}{m:02}{d:02}")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn remove_worktree_dir_safely_unlinks_directory_links_and_readonly_files_without_entering_targets()
     {
        let temp = tempfile::tempdir().unwrap();
        let external = temp.path().join("external-shared");
        fs::create_dir_all(external.join("nested")).unwrap();
        fs::write(external.join("nested/keep.txt"), "do-not-delete\n").unwrap();

        let wt = temp.path().join("fake-wt");
        fs::create_dir_all(wt.join("node_modules")).unwrap();
        fs::write(wt.join(".git"), "gitdir: /tmp/nowhere\n").unwrap();
        let ro_file = wt.join("readonly.dat");
        fs::write(&ro_file, "ro").unwrap();
        let mut perms = fs::metadata(&ro_file).unwrap().permissions();
        perms.set_readonly(true);
        fs::set_permissions(&ro_file, perms).unwrap();

        let link_path = wt.join("node_modules/shared-pkg");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&external, &link_path).unwrap();
        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd")
                .args([
                    "/C",
                    "mklink",
                    "/J",
                    link_path.to_str().unwrap(),
                    external.to_str().unwrap(),
                ])
                .status()
                .unwrap();
            assert!(status.success());
        }

        remove_worktree_dir_safely(&wt).unwrap();
        assert!(!wt.exists());
        assert_eq!(
            fs::read_to_string(external.join("nested/keep.txt")).unwrap(),
            "do-not-delete\n"
        );
    }
}
