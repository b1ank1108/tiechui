use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tiechui_protocol::app::TrashWorktreePolicy;
use tiechui_protocol::rpc::Id;
use tiechui_protocol::workspace::*;
use tiechui_workspace::{self as ws, Sink, Workspace};
use tokio_util::sync::CancellationToken;

use super::{Reply, Server, internal, parse, value};

#[derive(Default)]
pub(super) struct TrashPlan {
    pub(super) confirm_dirty: Option<WorktreeEntry>,
    pub(super) worktree: Option<WorktreeEntry>,
    pub(super) remove_force: Option<bool>,
}

impl TrashPlan {
    fn keep(entry: WorktreeEntry) -> Self {
        Self {
            confirm_dirty: None,
            worktree: Some(entry),
            remove_force: None,
        }
    }

    fn remove(entry: WorktreeEntry, force: bool) -> Self {
        Self {
            confirm_dirty: None,
            worktree: Some(entry),
            remove_force: Some(force),
        }
    }

    fn confirm(entry: WorktreeEntry) -> Self {
        Self {
            confirm_dirty: Some(entry),
            worktree: None,
            remove_force: None,
        }
    }
}

pub(super) fn new(home: PathBuf, sink: Sink) -> Arc<Workspace> {
    Arc::new(Workspace::with_home(home, sink))
}

pub(super) fn handles(method: &str) -> bool {
    ["fs/", "git/", "worktree/", "terminal/", "ports/"]
        .iter()
        .any(|group| method.starts_with(group))
}

pub(super) async fn dispatch(
    server: &Server,
    client_id: u64,
    rpc_id: &Id,
    method: &str,
    params: Value,
) -> Reply {
    if super::textgen::handles_git(method) {
        return super::textgen::dispatch_git(server, client_id, rpc_id, method, params).await;
    }
    if method == "worktree/cleanup" {
        return server.run_worktree_cleanup(true).await.and_then(value);
    }
    if method == "worktree/cleanup_status" {
        return server.worktree_cleanup_status().await.and_then(value);
    }
    let home = server.home.clone();
    let workspace = server.workspace.clone();
    let method_owned = method.to_owned();
    let reply = tokio::task::spawn_blocking(move || {
        handle(&home, &workspace, client_id, &method_owned, params)
    })
    .await
    .map_err(internal)??;
    if method == "worktree/remove" {
        server.prune_worktree_cleanup_cache();
    }
    Ok(reply)
}

pub(super) fn sweep_worktrees(
    home: &Path,
    live: &[ws::LiveWorktreeSession],
) -> Result<WorktreeCleanupStatus, String> {
    let config = crate::setup::user_config(&home.join("config.toml"))?;
    let policy = ws::WorktreeCleanupPolicy {
        auto_clean_idle_days: config.worktrees.auto_clean_idle_days,
        auto_clean_on_session_delete: config.worktrees.auto_clean_on_session_delete,
    };
    let env = config.network.subprocess_env();
    ws::cleanup_worktrees(home, policy, live, &env).map_err(|e| e.0)
}

pub(super) fn plan_session_trash(
    home: &Path,
    session_path: &Path,
    live_info: Option<(String, PathBuf)>,
    policy: TrashWorktreePolicy,
    live_sessions: &[ws::LiveSessionCwd],
    cancel: &CancellationToken,
) -> Result<TrashPlan, String> {
    let Some((session_id, cwd)) = resolve_session_target(session_path, live_info, cancel) else {
        return Ok(TrashPlan::default());
    };
    let env = git_env(home);
    let Some(entry) = ws::managed_worktree_with_env(home, &cwd, &env).map_err(|e| e.0)? else {
        return Ok(TrashPlan::default());
    };
    if ws::check_worktree_isolation(home, &cwd, &session_id, live_sessions)
        .map_err(|e| e.0)?
        .is_some()
    {
        return Ok(TrashPlan::keep(entry));
    }
    Ok(decide_sole_worktree_plan(entry, policy))
}

fn resolve_session_target(
    session_path: &Path,
    live_info: Option<(String, PathBuf)>,
    cancel: &CancellationToken,
) -> Option<(String, PathBuf)> {
    if live_info.is_some() {
        return live_info;
    }
    let store = tiechui_session::SessionStore::open_read_only(session_path, cancel).ok()?;
    Some((store.header().id.clone(), store.header().cwd.clone()))
}

fn decide_sole_worktree_plan(entry: WorktreeEntry, policy: TrashWorktreePolicy) -> TrashPlan {
    match (entry.dirty, policy) {
        (false, TrashWorktreePolicy::Keep) => TrashPlan::keep(entry),
        (false, _) => TrashPlan::remove(entry, false),
        (true, TrashWorktreePolicy::AskIfDirty) => TrashPlan::confirm(entry),
        (true, TrashWorktreePolicy::ForceRemove) => TrashPlan::remove(entry, true),
        (true, TrashWorktreePolicy::Auto | TrashWorktreePolicy::Keep) => TrashPlan::keep(entry),
    }
}

pub(super) fn remove_trashed_worktree(
    home: &Path,
    wt_path: &Path,
    force: bool,
) -> Result<bool, String> {
    match ws::remove_worktree_with_env(home, wt_path, force, &git_env(home)) {
        Ok(_) => Ok(true),
        Err(err) if !force && err.0.contains("uncommitted changes") => Ok(false),
        Err(err) => Err(err.0),
    }
}

fn call<P: DeserializeOwned, R: Serialize>(
    params: Value,
    work: impl FnOnce(P) -> ws::Result<R>,
) -> Reply {
    work(parse(params)?)
        .map_err(|error| internal(error.0))
        .and_then(value)
}

fn git_env(home: &Path) -> BTreeMap<String, String> {
    crate::setup::user_config(&home.join("config.toml"))
        .map(|config| config.network.subprocess_env())
        .unwrap_or_default()
}

fn handle(
    home: &Path,
    workspace: &Workspace,
    client_id: u64,
    method: &str,
    params: Value,
) -> Reply {
    match method {
        "fs/tree" => call(params, |p: RootParams| ws::tree(&ws::root(&p.root)?)),
        "fs/list" => call(params, |p: FileParams| {
            ws::list(&ws::root(&p.root)?, &p.path)
        }),
        "fs/read" => call(params, |p: FileParams| {
            ws::read(&ws::root(&p.root)?, &p.path)
        }),
        "fs/write" => call(params, |p: WriteParams| {
            ws::write(&ws::root(&p.root)?, &p.path, &p.text, p.expected.as_deref())
        }),
        "fs/create" => call(params, |p: FileParams| {
            ws::create(&ws::root(&p.root)?, &p.path)
        }),
        "fs/mkdir" => call(params, |p: FileParams| {
            ws::mkdir(&ws::root(&p.root)?, &p.path)
        }),
        "fs/delete" => call(params, |p: FileParams| {
            ws::delete(&ws::root(&p.root)?, &p.path)
        }),
        "fs/rename" => call(params, |p: MoveParams| {
            ws::rename(&ws::root(&p.root)?, &p.from, &p.to)
        }),
        "fs/search" => call(params, |p: SearchParams| {
            ws::search(&ws::root(&p.root)?, &p)
        }),
        "fs/watch" => call(params, |p: RootParams| workspace.watch(&ws::root(&p.root)?)),
        "fs/unwatch" => call(params, |p: RootParams| {
            workspace.unwatch(&ws::root(&p.root)?);
            Ok(())
        }),
        "git/status" => call(params, |p: RootParams| {
            ws::status_with_env(&ws::root(&p.root)?, &git_env(home))
        }),
        "git/compare" => call(params, |p: CompareParams| {
            ws::compare_with_env(&ws::root(&p.root)?, &p.base, &git_env(home))
        }),
        "git/log" => call(params, |p: LogParams| {
            ws::log_with_env(&ws::root(&p.root)?, p.limit, &git_env(home))
        }),
        "git/show" => call(params, |p: ShowParams| {
            ws::show_with_env(&ws::root(&p.root)?, &p.rev, &p.path, &git_env(home))
        }),
        "git/branches" => call(params, |p: RootParams| {
            ws::branches_with_env(&ws::root(&p.root)?, &git_env(home))
        }),
        "git/pr" => call(params, |p: RootParams| {
            ws::pull_request_with_env(&ws::root(&p.root)?, &git_env(home))
        }),
        "git/run" => call(params, |p: GitRunParams| {
            ws::run_with_env(&ws::root(&p.root)?, &p.op, &git_env(home))
                .map(|output| GitRunResult { output })
        }),
        _ if method.starts_with("worktree/") => handle_worktree(home, method, params),
        "terminal/shells" => value(workspace.terminal_shells()),
        "terminal/create" => call(params, |p: TerminalCreateParams| {
            workspace.create_terminal(p)
        }),
        "terminal/attach" => call(params, |p: TerminalParams| workspace.attach_terminal(&p.id)),
        "terminal/write" => call(params, |p: TerminalWriteParams| {
            workspace.write_terminal(&p.id, &p.data)
        }),
        "terminal/resize" => call(params, |p: TerminalResizeParams| {
            workspace.resize_terminal(&p)
        }),
        "terminal/kill" => call(params, |p: TerminalParams| {
            workspace.kill_terminal(&p.id);
            Ok(())
        }),
        "ports/list" => call(params, |p: PortsListParams| {
            workspace.list_ports_for_client(client_id, &p.roots)
        }),
        "ports/stop" => call(params, |p: PortStopParams| workspace.stop_port(&p)),
        _ => Err(super::failure(-32601, format!("unknown method {method}"))),
    }
}

fn handle_worktree(home: &Path, method: &str, params: Value) -> Reply {
    match method {
        "worktree/create" => call(params, |p: WorktreeCreateParams| {
            ws::create_worktree_with_options(
                home,
                &ws::root(&p.root)?,
                ws::CreateWorktreeOptions {
                    base_branch: p.base_branch,
                    carry_changes: p.carry_changes,
                    post_create_timeout: None,
                },
                &git_env(home),
            )
        }),
        "worktree/list" => call(params, |p: WorktreeListParams| {
            let root = p.root.as_deref().map(ws::root).transpose()?;
            ws::list_worktrees_with_env(home, root.as_deref(), &git_env(home))
        }),
        "worktree/remove" => call(params, |p: WorktreeRemoveParams| {
            ws::remove_worktree_with_env(home, &ws::root(&p.path)?, p.force, &git_env(home))
        }),
        _ => Err(super::failure(-32601, format!("unknown method {method}"))),
    }
}
