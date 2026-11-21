use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tiechui_plugin_api::{
    CancellationToken, LifecycleContext, ToolError, model, protocol::BranchEntry, serde_json::Value,
};

use crate::store::{self, SnapshotRecord};

#[derive(Debug, Clone)]
pub struct SnapshotOptions {
    pub timeout: Duration,
    pub env: BTreeMap<String, String>,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            timeout: tiechui_workspace::DEFAULT_SNAPSHOT_TIMEOUT,
            env: BTreeMap::new(),
        }
    }
}

pub fn tiechui_home_from_rewind_dir(directory: &Path) -> Option<PathBuf> {
    let parent = directory.parent().filter(|p| !p.as_os_str().is_empty())?;
    if directory.file_name().is_some_and(|name| name == "rewind")
        && parent.file_name().is_some_and(|name| name == "state")
        && !parent.join("worktrees/registry.json").is_file()
    {
        return parent
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf);
    }
    Some(parent.to_path_buf())
}

pub(crate) async fn capture_before_request(
    directory: &Path,
    options: &SnapshotOptions,
    ctx: &LifecycleContext,
    cancel: CancellationToken,
) {
    let Some(transcript_path) = ctx.transcript_path.clone() else {
        return;
    };
    let directory = directory.to_path_buf();
    let options = options.clone();
    let cwd = ctx.cwd.clone();
    let session_id = ctx.session_id.clone();
    let _ = tokio::task::spawn_blocking(move || {
        capture_before_request_sync(
            &directory,
            &options,
            &cwd,
            &session_id,
            &transcript_path,
            &cancel,
        );
    })
    .await;
}

fn capture_before_request_sync(
    directory: &Path,
    options: &SnapshotOptions,
    cwd: &Path,
    session_id: &str,
    transcript_path: &Path,
    cancel: &CancellationToken,
) {
    if cancel.is_cancelled() {
        return;
    }
    let Some(home) = tiechui_home_from_rewind_dir(directory) else {
        return;
    };
    if tiechui_workspace::session_worktree(&home, cwd).is_none() {
        return;
    }
    let Ok((parent_session, entries)) = read_transcript_branch(transcript_path, cancel) else {
        return;
    };
    let Some((anchor, _)) = crate::prompts(&entries).last().cloned() else {
        return;
    };
    let Ok(store) = crate::storage(
        directory,
        cwd,
        session_id,
        parent_session.as_deref(),
        &entries,
        cancel,
    ) else {
        return;
    };
    record_turn_snapshot(&store, options, cwd, session_id, &anchor.id, cancel);
}

/// Captures the worktree and records it under `anchor_id`, unless the turn has a record already.
/// A failed capture is recorded as missing, with its reason; nothing here fails the caller.
pub fn record_turn_snapshot(
    store: &store::Store,
    options: &SnapshotOptions,
    cwd: &Path,
    session_id: &str,
    anchor_id: &str,
    cancel: &CancellationToken,
) {
    if store.snapshot(anchor_id).ok().flatten().is_some() {
        return;
    }
    let is_cancelled = || cancel.is_cancelled();
    let outcome = tiechui_workspace::capture_worktree_snapshot_cancellable(
        cwd,
        session_id,
        anchor_id,
        options.timeout,
        &options.env,
        Some(&is_cancelled),
    );
    if cancel.is_cancelled() {
        return;
    }
    let record = match outcome {
        Ok(snap) => SnapshotRecord::Ready {
            reference: snap.reference,
            tree: snap.tree,
        },
        Err(err) => SnapshotRecord::Missing { reason: err.0 },
    };
    let _ = store.record_snapshot(anchor_id, record, cancel);
}

#[derive(Deserialize)]
struct TranscriptHeader {
    #[serde(default)]
    parent_session: Option<PathBuf>,
}

#[derive(Deserialize)]
struct TranscriptLine {
    id: String,
    parent_id: Option<String>,
    timestamp: u64,
    kind: String,
    #[serde(default)]
    message: Option<model::Message>,
    #[serde(default)]
    custom_kind: Option<String>,
    #[serde(default)]
    data: Value,
}

fn read_transcript_branch(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<(Option<PathBuf>, Vec<BranchEntry>), ToolError> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut buf = Vec::new();
    if reader.read_until(b'\n', &mut buf)? == 0 || !buf.ends_with(b"\n") {
        return Err(ToolError("Incomplete session header".into()));
    }
    let header: TranscriptHeader =
        tiechui_plugin_api::serde_json::from_slice(&buf).map_err(|e| ToolError(e.to_string()))?;
    let entries = read_transcript_entries(&mut reader, cancel)?;
    Ok((header.parent_session, active_branch(entries)))
}

fn read_transcript_entries(
    reader: &mut BufReader<File>,
    cancel: &CancellationToken,
) -> Result<Vec<BranchEntry>, ToolError> {
    let mut entries = Vec::new();
    let mut buf = Vec::new();
    loop {
        store::check(cancel)?;
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        match tiechui_plugin_api::serde_json::from_slice::<TranscriptLine>(&buf) {
            Ok(line) => entries.push(to_branch_entry(line)),
            Err(_) if !buf.ends_with(b"\n") => break,
            Err(err) => return Err(ToolError(err.to_string())),
        }
    }
    Ok(entries)
}

fn to_branch_entry(line: TranscriptLine) -> BranchEntry {
    let is_user = line.kind == "message"
        && line
            .message
            .as_ref()
            .is_some_and(|msg| msg.role == model::Role::User);
    let custom = (line.kind == "custom")
        .then_some(line.custom_kind)
        .flatten()
        .map(|kind| (kind, line.data));
    BranchEntry {
        id: line.id,
        parent_id: line.parent_id,
        timestamp: line.timestamp,
        content: is_user.then(Vec::new),
        custom,
    }
}

fn active_branch(entries: Vec<BranchEntry>) -> Vec<BranchEntry> {
    let Some(mut cursor) = entries.last().map(|entry| entry.id.clone()) else {
        return Vec::new();
    };
    let by_id: HashMap<String, usize> = entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| (entry.id.clone(), idx))
        .collect();
    let mut branch = Vec::new();
    for _ in 0..=entries.len() {
        let Some(&idx) = by_id.get(&cursor) else {
            break;
        };
        branch.push(entries[idx].clone());
        let Some(parent) = entries[idx].parent_id.clone() else {
            break;
        };
        cursor = parent;
    }
    branch.reverse();
    branch
}
