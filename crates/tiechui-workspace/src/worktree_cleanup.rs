use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;
use tiechui_protocol::workspace::{SkippedWorktree, WorktreeCleanupStatus, WorktreeSkipReason};

use crate::Result;
use crate::worktree::{
    Record, now_ms, read_registry_locked, read_registry_unlocked, remove_worktree_with_env,
    worktree_dirty_summary,
};
use crate::worktree_isolation::{
    DiskSessionHeader, classify_overlap, list_disk_session_headers, normalize_dir, read_disk_meta,
};

const DAY_MS: u64 = 86_400_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorktreeCleanupPolicy {
    pub auto_clean_idle_days: Option<u32>,
    pub auto_clean_on_session_delete: bool,
}

impl WorktreeCleanupPolicy {
    pub fn is_enabled(self) -> bool {
        self.auto_clean_idle_days.is_some() || self.auto_clean_on_session_delete
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveWorktreeSession {
    pub session_id: String,
    pub cwd: PathBuf,
    pub path: Option<PathBuf>,
    pub title: Option<String>,
}

struct WorktreeState {
    norm_path: PathBuf,
    has_non_deleted_session: bool,
    last_active_at: u64,
    active_reason: Option<String>,
}

pub fn cleanup_worktrees(
    home: &Path,
    policy: WorktreeCleanupPolicy,
    live_sessions: &[LiveWorktreeSession],
    env: &BTreeMap<String, String>,
) -> Result<WorktreeCleanupStatus> {
    cleanup_worktrees_with_env(home, policy, live_sessions, now_ms(), env)
}

pub fn cleanup_worktrees_with_env(
    home: &Path,
    policy: WorktreeCleanupPolicy,
    live_sessions: &[LiveWorktreeSession],
    now_millis: u64,
    env: &BTreeMap<String, String>,
) -> Result<WorktreeCleanupStatus> {
    let mut status = WorktreeCleanupStatus {
        removed: Vec::new(),
        skipped: Vec::new(),
        last_checked_at: Some(now_millis),
    };
    if !policy.is_enabled() {
        return Ok(status);
    }
    let records: Vec<Record> = read_registry_locked(home)?
        .into_iter()
        .filter(|rec| Path::new(&rec.path).is_dir())
        .collect();
    if records.is_empty() {
        return Ok(status);
    }
    let mut states = init_states(&records);
    apply_disk_sessions(home, &mut states)?;
    apply_live_sessions(live_sessions, &mut states);
    for (record, state) in records.into_iter().zip(states) {
        if is_cleanup_candidate(policy, &state, now_millis) {
            settle_candidate(home, &record, &state, env, &mut status);
        }
    }
    Ok(status)
}

pub fn prune_cleanup_status(home: &Path, status: &mut WorktreeCleanupStatus) {
    let active_paths: BTreeSet<PathBuf> = read_registry_unlocked(home)
        .into_iter()
        .map(|rec| PathBuf::from(rec.path))
        .filter(|path| path.is_dir())
        .map(|path| crate::win_path::path_key(&normalize_dir(&path)))
        .collect();
    status.skipped.retain(|item| {
        active_paths.contains(&crate::win_path::path_key(&normalize_dir(Path::new(
            &item.path,
        ))))
    });
}

fn init_states(records: &[Record]) -> Vec<WorktreeState> {
    records
        .iter()
        .map(|rec| WorktreeState {
            norm_path: normalize_dir(Path::new(&rec.path)),
            has_non_deleted_session: false,
            last_active_at: rec.created_at,
            active_reason: None,
        })
        .collect()
}

fn apply_disk_sessions(home: &Path, states: &mut [WorktreeState]) -> Result<()> {
    for header in list_disk_session_headers(home)? {
        apply_one_disk_header(&header, states);
    }
    Ok(())
}

fn apply_one_disk_header(header: &DiskSessionHeader, states: &mut [WorktreeState]) {
    let sess_norm = normalize_dir(&header.cwd);
    let mut updated_at: Option<u64> = None;
    for state in states.iter_mut() {
        if classify_overlap(&state.norm_path, &sess_norm).is_some() {
            let ts = *updated_at.get_or_insert_with(|| {
                read_latest_entry_timestamp(&header.path, header.created_at)
            });
            state.has_non_deleted_session = true;
            state.last_active_at = state.last_active_at.max(ts);
        }
    }
}

fn apply_live_sessions(live_sessions: &[LiveWorktreeSession], states: &mut [WorktreeState]) {
    for live in live_sessions {
        let live_norm = normalize_dir(&live.cwd);
        for state in states.iter_mut() {
            if classify_overlap(&state.norm_path, &live_norm).is_some() {
                state.has_non_deleted_session = true;
                if state.active_reason.is_none() {
                    state.active_reason = Some(format_active_reason(live));
                }
            }
        }
    }
}

fn format_active_reason(live: &LiveWorktreeSession) -> String {
    let title = live
        .title
        .clone()
        .or_else(|| live.path.as_deref().and_then(|p| read_disk_meta(p).title));
    match title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(name) => format!("仍有运行中的会话（{name}）"),
        None => "仍有运行中的会话".into(),
    }
}

fn is_cleanup_candidate(
    policy: WorktreeCleanupPolicy,
    state: &WorktreeState,
    now_millis: u64,
) -> bool {
    let orphan = policy.auto_clean_on_session_delete && !state.has_non_deleted_session;
    let idle = policy.auto_clean_idle_days.is_some_and(|days| {
        now_millis.saturating_sub(state.last_active_at) >= u64::from(days).saturating_mul(DAY_MS)
    });
    orphan || idle
}

fn settle_candidate(
    home: &Path,
    record: &Record,
    state: &WorktreeState,
    env: &BTreeMap<String, String>,
    status: &mut WorktreeCleanupStatus,
) {
    if let Some(reason) = &state.active_reason {
        status.skipped.push(build_skipped(
            record,
            state.last_active_at,
            WorktreeSkipReason::ActiveSession,
            reason.clone(),
        ));
        return;
    }
    let wt_path = Path::new(&record.path);
    match worktree_dirty_summary(wt_path, env) {
        Ok(Some(dirty_reason)) => status.skipped.push(build_skipped(
            record,
            state.last_active_at,
            WorktreeSkipReason::Dirty,
            dirty_reason,
        )),
        Ok(None) => remove_clean_candidate(home, record, state.last_active_at, env, status),
        Err(_) => {}
    }
}

fn remove_clean_candidate(
    home: &Path,
    record: &Record,
    last_active_at: u64,
    env: &BTreeMap<String, String>,
    status: &mut WorktreeCleanupStatus,
) {
    match remove_worktree_with_env(home, Path::new(&record.path), false, env) {
        Ok(removed) => status.removed.push(removed),
        Err(err) if err.0.contains("uncommitted changes") => status.skipped.push(build_skipped(
            record,
            last_active_at,
            WorktreeSkipReason::Dirty,
            "存在未提交改动".into(),
        )),
        Err(_) => {}
    }
}

fn build_skipped(
    record: &Record,
    last_active_at: u64,
    reason_kind: WorktreeSkipReason,
    reason: String,
) -> SkippedWorktree {
    SkippedWorktree {
        name: record.name.clone(),
        path: record.path.clone(),
        project_root: record.project_root.clone(),
        branch: record.live_branch(),
        base_branch: record.base_branch.clone(),
        created_at: record.created_at,
        last_active_at,
        reason_kind,
        reason,
    }
}

fn read_latest_entry_timestamp(path: &Path, fallback: u64) -> u64 {
    let Ok(file) = File::open(path) else {
        return fallback;
    };
    BufReader::new(file)
        .lines()
        .skip(1)
        .map_while(std::result::Result::ok)
        .filter_map(|line| {
            serde_json::from_str::<Value>(&line)
                .ok()?
                .get("timestamp")?
                .as_u64()
        })
        .fold(fallback, u64::max)
}
