#[cfg(feature = "workspace")]
use std::cmp::Reverse;
#[cfg(feature = "workspace")]
use std::collections::{HashMap, HashSet};
#[cfg(feature = "workspace")]
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tiechui_protocol::app::ScratchOpenParams;
#[cfg(feature = "workspace")]
use tiechui_protocol::app::{LiveState, ScratchListParams, SessionSummary};
use tiechui_protocol::rpc;
#[cfg(feature = "workspace")]
use tiechui_protocol::{SessionEpoch, ThreadId};
#[cfg(feature = "workspace")]
use tiechui_session::{SessionStore, Summary};
#[cfg(feature = "workspace")]
use tokio_util::sync::CancellationToken;

use super::{Live, Options, Reply, Server};
#[cfg(feature = "workspace")]
use super::{Open, blocking, lock, parse, value};

pub(super) fn handles(method: &str) -> bool {
    method == "scratch/list"
}

pub(super) async fn open_scratch(
    server: &Arc<Server>,
    options: Options,
    scratch: ScratchOpenParams,
) -> Result<Arc<Live>, rpc::Error> {
    #[cfg(not(feature = "workspace"))]
    {
        let _ = (server, options, scratch);
        Err(super::internal("workspace feature is disabled"))
    }
    #[cfg(feature = "workspace")]
    {
        let home = server.home.clone();
        let summary = scratch.summary;
        let scratch_cwd = blocking(move |_| {
            tiechui_workspace::allocate_scratch_dir(&home, summary.as_deref()).map_err(|e| e.0)
        })
        .await?;
        match server
            .start(
                options,
                Open::New(scratch_cwd.clone()),
                SessionEpoch(0),
                None,
            )
            .await
        {
            Ok((_, live)) => Ok(live),
            Err(error) => {
                let _ = std::fs::remove_dir(&scratch_cwd);
                Err(error)
            }
        }
    }
}

#[cfg(feature = "workspace")]
#[derive(Clone)]
struct OpenSessionSnapshot {
    thread_id: ThreadId,
    path: PathBuf,
    cwd: PathBuf,
    state: LiveState,
}

#[cfg(feature = "workspace")]
fn snapshot_open_sessions(server: &Server) -> Vec<OpenSessionSnapshot> {
    lock(&server.sessions)
        .iter()
        .map(|(thread_id, live)| OpenSessionSnapshot {
            thread_id: thread_id.clone(),
            path: live.path.clone(),
            cwd: live.cwd.clone(),
            state: lock(&live.projection).state(),
        })
        .collect()
}

pub(super) async fn list(server: &Server, params: Value) -> Reply {
    #[cfg(not(feature = "workspace"))]
    {
        let _ = (server, params);
        Err(super::internal("workspace feature is disabled"))
    }
    #[cfg(feature = "workspace")]
    {
        let params: ScratchListParams = parse(params)?;
        let home = server.home.clone();
        let open_sessions = snapshot_open_sessions(server);
        let listed = blocking(move |cancel| {
            collect_scratch_summaries(&home, &open_sessions, params.archived, cancel)
        })
        .await?;
        value(listed)
    }
}

#[cfg(feature = "workspace")]
fn collect_scratch_summaries(
    home: &Path,
    open_sessions: &[OpenSessionSnapshot],
    include_archived: bool,
    cancel: &CancellationToken,
) -> Result<Vec<SessionSummary>, String> {
    let open_scratch = filter_open_scratch(home, open_sessions);
    let cwds = gather_scratch_cwds(home, &open_scratch)?;
    let persisted = load_persisted_summaries(home, &cwds, cancel)?;
    let mut listed = merge_scratch_summaries(persisted, &open_scratch, cancel);
    listed.retain(|item| include_archived || item.archived_at.is_none());
    sort_summaries(&mut listed);
    Ok(listed)
}

#[cfg(feature = "workspace")]
fn filter_open_scratch<'a>(
    home: &Path,
    open_sessions: &'a [OpenSessionSnapshot],
) -> Vec<&'a OpenSessionSnapshot> {
    open_sessions
        .iter()
        .filter(|item| tiechui_workspace::is_scratch_dir(home, &item.cwd))
        .collect()
}

#[cfg(feature = "workspace")]
fn gather_scratch_cwds(
    home: &Path,
    open_scratch: &[&OpenSessionSnapshot],
) -> Result<Vec<PathBuf>, String> {
    let mut cwds = tiechui_workspace::list_scratch_dirs(home).map_err(|e| e.0)?;
    for item in open_scratch {
        if item.cwd.is_dir() && !cwds.contains(&item.cwd) {
            cwds.push(item.cwd.clone());
        }
    }
    Ok(cwds)
}

#[cfg(feature = "workspace")]
fn merge_scratch_summaries(
    persisted: Vec<Summary>,
    open_scratch: &[&OpenSessionSnapshot],
    cancel: &CancellationToken,
) -> Vec<SessionSummary> {
    let live_map: HashMap<ThreadId, LiveState> = open_scratch
        .iter()
        .map(|item| (item.thread_id.clone(), item.state))
        .collect();
    let mut seen = HashSet::new();
    let mut listed = Vec::new();
    for item in persisted {
        let summary = from_persisted_summary(item, &live_map);
        seen.insert(summary.thread_id.clone());
        listed.push(summary);
    }
    for open in open_scratch {
        if !seen.contains(&open.thread_id)
            && let Some(summary) = from_live_only_session(open, cancel)
        {
            listed.push(summary);
        }
    }
    listed
}

#[cfg(feature = "workspace")]
fn sort_summaries(listed: &mut [SessionSummary]) {
    listed.sort_by(|a, b| {
        Reverse(a.modified)
            .cmp(&Reverse(b.modified))
            .then_with(|| Reverse(a.created_at).cmp(&Reverse(b.created_at)))
            .then_with(|| a.thread_id.0.cmp(&b.thread_id.0))
    });
}

#[cfg(feature = "workspace")]
fn load_persisted_summaries(
    home: &Path,
    cwds: &[PathBuf],
    cancel: &CancellationToken,
) -> Result<Vec<Summary>, String> {
    if cwds.is_empty() {
        return Ok(Vec::new());
    }
    let (summaries, _) =
        tiechui_session::summaries_in(home, cwds, cancel).map_err(|e| e.to_string())?;
    Ok(summaries)
}

#[cfg(feature = "workspace")]
fn from_persisted_summary(
    summary: Summary,
    live_map: &HashMap<ThreadId, LiveState>,
) -> SessionSummary {
    let thread_id = ThreadId(summary.header.id);
    SessionSummary {
        live: live_map.get(&thread_id).copied(),
        thread_id,
        path: summary.path.display().to_string(),
        cwd: summary.header.cwd.display().to_string(),
        worktree: None,
        created_at: summary.header.created_at,
        modified: summary.modified,
        first_message: summary.first_message,
        title: summary.meta.title,
        archived_at: summary.meta.archived_at,
        parent: summary
            .header
            .parent_session
            .map(|p| p.display().to_string()),
    }
}

#[cfg(feature = "workspace")]
fn from_live_only_session(
    open: &OpenSessionSnapshot,
    cancel: &CancellationToken,
) -> Option<SessionSummary> {
    let store = SessionStore::open_read_only(&open.path, cancel).ok()?;
    let meta = tiechui_session::meta(&open.path).ok().unwrap_or_default();
    let header = store.header();
    Some(SessionSummary {
        thread_id: open.thread_id.clone(),
        path: open.path.display().to_string(),
        cwd: open.cwd.display().to_string(),
        worktree: None,
        created_at: header.created_at,
        modified: header.created_at,
        first_message: String::new(),
        title: meta.title,
        archived_at: meta.archived_at,
        parent: header
            .parent_session
            .as_ref()
            .map(|p| p.display().to_string()),
        live: Some(open.state),
    })
}
