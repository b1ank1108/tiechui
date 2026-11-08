//! `turn/*`: a turn's tracked file changes, undoing them, and rewinding files to before it.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tiechui_model::{Content, Role};
use tiechui_module_rewind::store::{Store, Turn, Version};
use tiechui_module_rewind::{prompts, storage};
use tiechui_protocol::BranchEntry;
use tiechui_protocol::app::*;
use tiechui_session::{EntryKind, SessionStore};
use tokio_util::sync::CancellationToken;

use super::{starts_turn, user_entry};

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        "turn/changes" | "turn/check" | "turn/undo" | "turn/rewind"
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

/// A turn on the current branch: its rewind anchors, those of the turns after it, and the
/// shell commands it ran.
struct Located {
    store: Store,
    own: Vec<String>,
    later: Vec<String>,
    commands: Vec<String>,
}

pub(super) fn handle(
    home: &Path,
    session: &Path,
    method: &str,
    item: &str,
    cancel: &CancellationToken,
) -> Result<Value, String> {
    let at = locate(home, session, item, cancel)?;
    let turn = at.store.turn(&at.own, &at.later, cancel).map_err(|e| e.0)?;
    let result = match method {
        "turn/changes" => serde_json::to_value(changes(&at.store, &turn)?),
        "turn/check" => serde_json::to_value(check(&at, &turn, cancel)?),
        "turn/undo" => {
            let check = check(&at, &turn, cancel)?;
            if !check.r#unsafe.is_empty() {
                return Err(format!(
                    "changed since the turn: {}",
                    check.r#unsafe.join(", ")
                ));
            }
            let turn = at
                .store
                .toggle(&at.own, &at.later, cancel)
                .map_err(|e| e.0)?;
            serde_json::to_value(changes(&at.store, &turn)?)
        }
        _ => serde_json::to_value(RewindResult {
            conflicts: at
                .store
                .rewind(&[at.own, at.later].concat(), cancel)
                .map_err(|e| e.0)?,
        }),
    };
    result.map_err(|e| e.to_string())
}

fn locate(
    home: &Path,
    session: &Path,
    item: &str,
    cancel: &CancellationToken,
) -> Result<Located, String> {
    let store = SessionStore::open_read_only(session, cancel).map_err(|e| e.to_string())?;
    let start = user_entry(&store, item)?.id.clone();
    let header = store.header();
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
    // Rewind anchors on user messages and control checkpoints only, so content is not needed.
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
    let anchors = |entries: &[BranchEntry]| {
        prompts(entries)
            .into_iter()
            .map(|(entry, _)| entry.id)
            .collect::<Vec<_>>()
    };
    let commands = branch[index..end]
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
        .collect();
    let store = storage(
        &home.join("state/rewind"),
        &header.cwd,
        &header.id,
        header.parent_session.as_deref(),
        &entries,
        cancel,
    )
    .map_err(|e| e.0)?;
    Ok(Located {
        store,
        own: anchors(&entries[index..end]),
        later: anchors(&entries[end..]),
        commands,
    })
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
        check.ignored = at.commands.clone();
    }
    Ok(check)
}
