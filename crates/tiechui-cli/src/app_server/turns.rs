//! `turn/*` and `session/rewind`: tracked file changes, single-turn patch undo, rewind preflight,
//! and conversation-first session rewind.

#[cfg(feature = "workspace")]
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tiechui_model::{Content, Role};
#[cfg(feature = "workspace")]
use tiechui_module_rewind::store::SnapshotRecord;
use tiechui_module_rewind::store::{Store, Turn, Version};
use tiechui_module_rewind::{prompts, storage};
use tiechui_protocol::BranchEntry;
#[cfg(feature = "workspace")]
use tiechui_protocol::ThreadId;
use tiechui_protocol::app::*;
use tiechui_session::{Entry, EntryKind, SessionStore};
use tokio_util::sync::CancellationToken;

use super::{starts_turn, user_entry};

#[cfg(feature = "workspace")]
pub(super) type LiveSessionList = Vec<tiechui_workspace::LiveSessionCwd>;
#[cfg(not(feature = "workspace"))]
pub(super) type LiveSessionList = Vec<()>;

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        "turn/changes" | "turn/check" | "turn/undo" | "turn/rewind" | "turn/rewind_check"
    )
}

/// The session's rewind storage, if it has any.
pub(super) fn storage_of(
    home: &Path,
    session: &Path,
    cancel: &CancellationToken,
) -> Option<PathBuf> {
    let store = SessionStore::open_read_only(session, cancel).ok()?;
    let header = store.header();
    let store = Store::new(&home.join("state/rewind"), &header.cwd, &header.id).ok()?;
    store.directory.exists().then_some(store.directory)
}

pub(super) struct Located {
    pub(super) store: Store,
    #[cfg(feature = "workspace")]
    pub(super) session_id: String,
    #[cfg(feature = "workspace")]
    pub(super) cwd: PathBuf,
    pub(super) external: bool,
    pub(super) branch_target: String,
    pub(super) own: Vec<String>,
    pub(super) later: Vec<String>,
    pub(super) turn_commands: Vec<String>,
    pub(super) rewind_commands: Vec<String>,
}

pub(super) enum PreparedRewind {
    Proceed {
        at: Box<Located>,
        discarded: Vec<String>,
        snapshot_tree: Option<String>,
    },
    PatchConflicts {
        conflicts: Vec<String>,
        commands: Vec<String>,
    },
}

pub(super) fn handle(
    home: &Path,
    session: &Path,
    method: &str,
    item: &str,
    live_sessions: &LiveSessionList,
    cancel: &CancellationToken,
) -> Result<Value, String> {
    if method == "turn/rewind_check" {
        return rewind_check(home, session, item, live_sessions, cancel).and_then(to_json);
    }
    let at = locate(home, session, item, cancel)?;
    dispatch_turn_op(method, at, cancel)
}

fn dispatch_turn_op(
    method: &str,
    at: Located,
    cancel: &CancellationToken,
) -> Result<Value, String> {
    if at.external {
        return snapshot_turn_op(method, &at, cancel);
    }
    let turn = at.store.turn(&at.own, &at.later, cancel).map_err(|e| e.0)?;
    match method {
        "turn/changes" => changes(&at.store, &turn).and_then(to_json),
        "turn/check" => check(&at, &turn, cancel).and_then(to_json),
        "turn/undo" => undo_turn(&at, &turn, cancel).and_then(to_json),
        _ => rewind_files(at, cancel).and_then(to_json),
    }
}

fn rewind_files(at: Located, cancel: &CancellationToken) -> Result<RewindResult, String> {
    let discarded = [at.own, at.later].concat();
    let conflicts = at.store.rewind(&discarded, cancel).map_err(|e| e.0)?;
    Ok(RewindResult { conflicts })
}

fn to_json(val: impl serde::Serialize) -> Result<Value, String> {
    serde_json::to_value(val).map_err(|e| e.to_string())
}

fn undo_turn(at: &Located, turn: &Turn, cancel: &CancellationToken) -> Result<TurnChanges, String> {
    let checked = check(at, turn, cancel)?;
    if !checked.r#unsafe.is_empty() {
        return Err(format!(
            "changed since the turn: {}",
            checked.r#unsafe.join(", ")
        ));
    }
    let toggled = at
        .store
        .toggle(&at.own, &at.later, cancel)
        .map_err(|e| e.0)?;
    changes(&at.store, &toggled)
}

fn rewind_check(
    home: &Path,
    session: &Path,
    item: &str,
    live_sessions: &LiveSessionList,
    cancel: &CancellationToken,
) -> Result<RewindCheck, String> {
    let at = locate(home, session, item, cancel)?;
    let discarded = [at.own.clone(), at.later.clone()].concat();
    #[cfg(feature = "workspace")]
    if tiechui_workspace::session_worktree(home, &at.cwd).is_some() {
        return worktree_rewind_check(home, at, &discarded, live_sessions, cancel);
    }
    let _ = live_sessions;
    #[cfg(feature = "workspace")]
    if at.external
        && let Ok(Ok((reference, _))) = resolve_ready_snapshot(&at)
    {
        return Ok(RewindCheck {
            mode: RewindMode::Snapshot,
            reason: None,
            snapshot_ref: Some(reference),
            conflicting_session: None,
            commands: at.rewind_commands,
            files: vec![],
            safe: vec![],
            r#unsafe: vec![],
        });
    }
    build_patch_rewind_check(&at.store, &discarded, None, at.rewind_commands, cancel)
}

#[cfg(feature = "workspace")]
fn worktree_rewind_check(
    home: &Path,
    at: Located,
    discarded: &[String],
    live_sessions: &LiveSessionList,
    cancel: &CancellationToken,
) -> Result<RewindCheck, String> {
    if let Some(conflict) =
        tiechui_workspace::check_worktree_isolation(home, &at.cwd, &at.session_id, live_sessions)
            .map_err(|e| e.0)?
    {
        return Ok(RewindCheck {
            mode: RewindMode::ChatOnly,
            reason: Some(conflict.reason),
            snapshot_ref: None,
            conflicting_session: Some(RewindConflictSession {
                thread_id: ThreadId(conflict.session_id),
                cwd: conflict.cwd,
                title: conflict.title,
                archived: conflict.archived,
            }),
            commands: at.rewind_commands,
            files: vec![],
            safe: vec![],
            r#unsafe: vec![],
        });
    }
    let fallback_reason = match resolve_ready_snapshot(&at)? {
        Ok((reference, _tree)) => {
            return Ok(RewindCheck {
                mode: RewindMode::Snapshot,
                reason: None,
                snapshot_ref: Some(reference),
                conflicting_session: None,
                commands: at.rewind_commands,
                files: vec![],
                safe: vec![],
                r#unsafe: vec![],
            });
        }
        Err(reason) => Some(reason),
    };
    build_patch_rewind_check(
        &at.store,
        discarded,
        fallback_reason,
        at.rewind_commands,
        cancel,
    )
}

#[cfg(feature = "workspace")]
fn resolve_ready_snapshot(at: &Located) -> Result<Result<(String, String), String>, String> {
    let Some(anchor) = at.own.first() else {
        return Ok(Err("no checkpoint anchor for this turn".into()));
    };
    ready_snapshot(at, anchor)
}

#[cfg(feature = "workspace")]
fn ready_snapshot(at: &Located, anchor: &str) -> Result<Result<(String, String), String>, String> {
    match at.store.snapshot(anchor).map_err(|e| e.0)? {
        Some(SnapshotRecord::Ready { reference, tree }) => {
            if tiechui_workspace::has_worktree_snapshot_tree(&at.cwd, &tree, &BTreeMap::new()) {
                Ok(Ok((reference, tree)))
            } else {
                Ok(Err(format!("snapshot tree {tree} is missing in Git")))
            }
        }
        Some(SnapshotRecord::Missing { reason }) => Ok(Err(reason)),
        None => Ok(Err("no worktree snapshot recorded for this turn".into())),
    }
}

fn build_patch_rewind_check(
    store: &Store,
    discarded: &[String],
    reason: Option<String>,
    commands: Vec<String>,
    cancel: &CancellationToken,
) -> Result<RewindCheck, String> {
    let (safe, unsafe_files) = store.patch_preview(discarded, cancel).map_err(|e| e.0)?;
    let mut files: Vec<String> = safe.iter().chain(unsafe_files.iter()).cloned().collect();
    files.sort();
    Ok(RewindCheck {
        mode: RewindMode::Patch,
        reason,
        snapshot_ref: None,
        conflicting_session: None,
        commands,
        files,
        safe,
        r#unsafe: unsafe_files,
    })
}

pub(super) fn prepare_session_rewind(
    home: &Path,
    session: &Path,
    item: &str,
    mode: RewindMode,
    live_sessions: &LiveSessionList,
    cancel: &CancellationToken,
) -> Result<PreparedRewind, String> {
    let at = locate(home, session, item, cancel)?;
    let discarded = [at.own.clone(), at.later.clone()].concat();
    match mode {
        RewindMode::ChatOnly => Ok(PreparedRewind::Proceed {
            at: Box::new(at),
            discarded,
            snapshot_tree: None,
        }),
        RewindMode::Patch => prepare_patch_rewind(at, discarded, cancel),
        RewindMode::Snapshot => prepare_snapshot_rewind(home, at, discarded, live_sessions, cancel),
    }
}

fn prepare_patch_rewind(
    at: Located,
    discarded: Vec<String>,
    cancel: &CancellationToken,
) -> Result<PreparedRewind, String> {
    at.store.ensure_ready_for_restore(cancel).map_err(|e| e.0)?;
    let (_safe, conflicts) = at
        .store
        .patch_preview(&discarded, cancel)
        .map_err(|e| e.0)?;
    if !conflicts.is_empty() {
        return Ok(PreparedRewind::PatchConflicts {
            conflicts,
            commands: at.rewind_commands,
        });
    }
    Ok(PreparedRewind::Proceed {
        at: Box::new(at),
        discarded,
        snapshot_tree: None,
    })
}

#[cfg(feature = "workspace")]
fn prepare_snapshot_rewind(
    home: &Path,
    at: Located,
    discarded: Vec<String>,
    live_sessions: &LiveSessionList,
    cancel: &CancellationToken,
) -> Result<PreparedRewind, String> {
    if tiechui_workspace::session_worktree(home, &at.cwd).is_none() {
        return Err("whole-directory snapshot restore requires a managed worktree session".into());
    }
    if let Some(conflict) =
        tiechui_workspace::check_worktree_isolation(home, &at.cwd, &at.session_id, live_sessions)
            .map_err(|e| e.0)?
    {
        return Err(conflict.reason);
    }
    at.store.ensure_ready_for_restore(cancel).map_err(|e| e.0)?;
    let (_reference, tree) = resolve_ready_snapshot(&at)?
        .map_err(|reason| format!("whole-directory snapshot restore unavailable: {reason}"))?;
    Ok(PreparedRewind::Proceed {
        at: Box::new(at),
        discarded,
        snapshot_tree: Some(tree),
    })
}

#[cfg(not(feature = "workspace"))]
fn prepare_snapshot_rewind(
    _home: &Path,
    _at: Located,
    _discarded: Vec<String>,
    _live_sessions: &LiveSessionList,
    _cancel: &CancellationToken,
) -> Result<PreparedRewind, String> {
    Err("workspace support is disabled".into())
}

pub(super) fn apply_session_rewind_files(
    mode: RewindMode,
    at: &Located,
    discarded: &[String],
    snapshot_tree: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Vec<String>, String> {
    match mode {
        RewindMode::ChatOnly => Ok(vec![]),
        RewindMode::Patch => at.store.rewind(discarded, cancel).map_err(|e| e.0),
        RewindMode::Snapshot => apply_snapshot_restore(at, discarded, snapshot_tree, cancel),
    }
}

#[cfg(feature = "workspace")]
fn apply_snapshot_restore(
    at: &Located,
    discarded: &[String],
    snapshot_tree: Option<&str>,
    cancel: &CancellationToken,
) -> Result<Vec<String>, String> {
    let tree = snapshot_tree.ok_or_else(|| "missing snapshot tree".to_owned())?;
    let is_cancelled = || cancel.is_cancelled();
    tiechui_workspace::restore_worktree_snapshot_cancellable(
        &at.cwd,
        tree,
        tiechui_workspace::DEFAULT_SNAPSHOT_TIMEOUT,
        &BTreeMap::new(),
        Some(&is_cancelled),
    )
    .map_err(|e| e.0)?;
    at.store
        .settle_snapshot_restore(discarded, cancel)
        .map_err(|e| e.0)?;
    Ok(vec![])
}

#[cfg(not(feature = "workspace"))]
fn apply_snapshot_restore(
    _at: &Located,
    _discarded: &[String],
    _snapshot_tree: Option<&str>,
    _cancel: &CancellationToken,
) -> Result<Vec<String>, String> {
    Err("workspace support is disabled".into())
}

fn locate(
    home: &Path,
    session: &Path,
    item: &str,
    cancel: &CancellationToken,
) -> Result<Located, String> {
    let store = SessionStore::open_read_only(session, cancel).map_err(|e| e.to_string())?;
    let target_entry = user_entry(&store, item)?;
    let start = target_entry.id.clone();
    let header = store.header();
    let branch_target = target_entry
        .parent_id
        .clone()
        .unwrap_or_else(|| header.id.clone());
    let branch = store
        .path_to_root(store.current_leaf().unwrap_or(&header.id))
        .map_err(|e| e.to_string())?;
    let index = branch
        .iter()
        .position(|entry| entry.id == start)
        .ok_or_else(|| format!("{item} is not on the current branch"))?;
    let end = branch[index + 1..]
        .iter()
        .position(|entry| starts_turn(entry))
        .map_or(branch.len(), |n| index + 1 + n);
    let entries: Vec<_> = branch
        .iter()
        .map(|entry| BranchEntry {
            id: entry.id.clone(),
            parent_id: entry.parent_id.clone(),
            timestamp: entry.timestamp,
            content: starts_turn(entry).then(Vec::new),
            custom: match &entry.kind {
                EntryKind::Custom { kind, data } => Some((kind.clone(), data.clone())),
                _ => None,
            },
        })
        .collect();
    let turn_commands = extract_bash_commands(&branch[index..end]);
    let rewind_commands = extract_bash_commands(&branch[index..]);
    let rewind_store = storage(
        &home.join("state/rewind"),
        &header.cwd,
        &header.id,
        header.parent_session.as_deref(),
        &entries,
        cancel,
    )
    .map_err(|e| e.0)?;
    Ok(Located {
        external: header
            .runtime
            .as_deref()
            .is_some_and(|runtime| runtime != "chui"),
        store: rewind_store,
        #[cfg(feature = "workspace")]
        session_id: header.id.clone(),
        #[cfg(feature = "workspace")]
        cwd: header.cwd.clone(),
        branch_target,
        own: anchor_ids(&entries[index..end]),
        later: anchor_ids(&entries[end..]),
        turn_commands,
        rewind_commands,
    })
}

fn anchor_ids(entries: &[BranchEntry]) -> Vec<String> {
    prompts(entries)
        .into_iter()
        .map(|(entry, _)| entry.id)
        .collect()
}

fn extract_bash_commands(entries: &[&Entry]) -> Vec<String> {
    entries
        .iter()
        .flat_map(|entry| match &entry.kind {
            EntryKind::Message { message, .. } if message.role == Role::Assistant => {
                message.content.as_slice()
            }
            _ => &[][..],
        })
        .filter_map(|part| match part {
            Content::ToolCall(call) if call.name == "bash" => {
                call.arguments["command"].as_str().map(str::to_owned)
            }
            _ => None,
        })
        .collect()
}

fn changes(store: &Store, turn: &Turn) -> Result<TurnChanges, String> {
    let text = |version: &Option<Version>| {
        version
            .as_ref()
            .map(|_| {
                store
                    .bytes(version)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            })
            .transpose()
            .map_err(|e| e.0)
    };
    Ok(TurnChanges {
        changes: turn
            .files
            .iter()
            .map(|(path, (before, after))| {
                Ok(FileChange {
                    path: path.clone(),
                    before: text(before)?,
                    after: text(after)?,
                })
            })
            .collect::<Result<_, String>>()?,
        reverted: turn.reverted,
    })
}

fn check(at: &Located, turn: &Turn, cancel: &CancellationToken) -> Result<UndoCheck, String> {
    let mut check = UndoCheck::default();
    for (file, (before, after)) in &turn.files {
        let expected = if turn.reverted { before } else { after };
        let now = at.store.capture(file, cancel).map_err(|e| e.0)?;
        if now == *expected {
            check.safe.push(file.clone());
        } else {
            check.r#unsafe.push(file.clone());
        }
    }
    if !turn.reverted {
        check.ignored = at.turn_commands.clone();
    }
    Ok(check)
}

#[cfg(feature = "workspace")]
fn snapshot_turn_op(
    method: &str,
    at: &Located,
    cancel: &CancellationToken,
) -> Result<Value, String> {
    let anchor = at.own.first().ok_or("no checkpoint anchor for this turn")?;
    let (_, start) = resolve_ready_snapshot(at)?
        .map_err(|reason| format!("no worktree snapshot for this turn: {reason}"))?;
    let (_, end) = ready_snapshot(at, &tiechui_module_rewind::end_anchor(anchor))?
        .map_err(|reason| format!("no end-of-turn snapshot for this turn: {reason}"))?;
    let reverted = at.store.snapshot_reverted(anchor).map_err(|e| e.0)?;
    let top = git_toplevel(&at.cwd)?;
    let files = changed_between(&top, &start, &end)?;
    let (before, after) = (
        |path: &str| tree_file(&top, &start, path),
        |path: &str| tree_file(&top, &end, path),
    );
    let text =
        |file: Option<Vec<u8>>| file.map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    let changes = |reverted| TurnChanges {
        changes: files
            .iter()
            .map(|path| FileChange {
                path: path.clone(),
                before: text(before(path)),
                after: text(after(path)),
            })
            .collect(),
        reverted,
    };
    let mut unsafe_files = vec![];
    let mut safe = vec![];
    for path in &files {
        let expected = if reverted { before(path) } else { after(path) };
        if expected == current_file(&top, path) {
            safe.push(path.clone());
        } else {
            unsafe_files.push(path.clone());
        }
    }
    match method {
        "turn/changes" => to_json(changes(reverted)),
        "turn/check" => to_json(UndoCheck {
            safe,
            r#unsafe: unsafe_files,
            ignored: if reverted {
                vec![]
            } else {
                at.turn_commands.clone()
            },
        }),
        _ => {
            if !unsafe_files.is_empty() {
                return Err(format!(
                    "changed since the turn: {}",
                    unsafe_files.join(", ")
                ));
            }
            if files.is_empty() {
                return Err("the turn changed no files".into());
            }
            for path in &files {
                write_file(
                    &top,
                    path,
                    if reverted { after(path) } else { before(path) },
                )?;
            }
            at.store
                .set_snapshot_reverted(anchor, !reverted, cancel)
                .map_err(|e| e.0)?;
            to_json(changes(!reverted))
        }
    }
}

#[cfg(not(feature = "workspace"))]
fn snapshot_turn_op(
    _method: &str,
    _at: &Located,
    _cancel: &CancellationToken,
) -> Result<Value, String> {
    Err("workspace support is disabled".into())
}

#[cfg(feature = "workspace")]
fn git_toplevel(cwd: &Path) -> Result<PathBuf, String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err("not a Git worktree".into());
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

#[cfg(feature = "workspace")]
fn changed_between(top: &Path, start: &str, end: &str) -> Result<Vec<String>, String> {
    let output = std::process::Command::new("git")
        .args(["diff-tree", "-r", "-z", "--raw", "--no-renames", start, end])
        .current_dir(top)
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    // `:<mode> <mode> <oid> <oid> <status>` NUL `<path>` NUL, per file.
    let text = String::from_utf8_lossy(&output.stdout);
    let mut fields = text.split('\0').filter(|field| !field.is_empty());
    let mut files = vec![];
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        if !meta.contains("160000") {
            files.push(path.to_owned());
        }
    }
    Ok(files)
}

#[cfg(feature = "workspace")]
fn tree_file(top: &Path, tree: &str, path: &str) -> Option<Vec<u8>> {
    let output = std::process::Command::new("git")
        .args(["cat-file", "blob", &format!("{tree}:{path}")])
        .current_dir(top)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

#[cfg(feature = "workspace")]
fn current_file(top: &Path, path: &str) -> Option<Vec<u8>> {
    std::fs::read(top.join(path)).ok()
}

#[cfg(feature = "workspace")]
fn write_file(top: &Path, path: &str, content: Option<Vec<u8>>) -> Result<(), String> {
    let target = top.join(path);
    let result = match content {
        Some(bytes) => target
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&target, bytes)),
        None => match std::fs::remove_file(&target) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        },
    };
    result.map_err(|e| format!("could not restore {path}: {e}"))
}
