use std::sync::Arc;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tiechui_protocol::workspace::*;
use tiechui_workspace::{self as ws, Sink, Workspace};

use super::{Reply, internal, parse, value};

pub(super) fn new(sink: Sink) -> Arc<Workspace> {
    Arc::new(Workspace::new(sink))
}

pub(super) fn handles(method: &str) -> bool {
    ["fs/", "git/", "terminal/", "ports/"]
        .iter()
        .any(|group| method.starts_with(group))
}

pub(super) async fn dispatch(
    workspace: &Arc<Workspace>,
    client_id: u64,
    method: &str,
    params: Value,
) -> Reply {
    let workspace = workspace.clone();
    let method = method.to_owned();
    tokio::task::spawn_blocking(move || handle(&workspace, client_id, &method, params))
        .await
        .map_err(internal)?
}

fn call<P: DeserializeOwned, R: Serialize>(
    params: Value,
    work: impl FnOnce(P) -> ws::Result<R>,
) -> Reply {
    work(parse(params)?)
        .map_err(|error| internal(error.0))
        .and_then(value)
}

fn handle(workspace: &Workspace, client_id: u64, method: &str, params: Value) -> Reply {
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
        "git/status" => call(params, |p: RootParams| ws::status(&ws::root(&p.root)?)),
        "git/compare" => call(params, |p: CompareParams| {
            ws::compare(&ws::root(&p.root)?, &p.base)
        }),
        "git/log" => call(params, |p: LogParams| ws::log(&ws::root(&p.root)?, p.limit)),
        "git/show" => call(params, |p: ShowParams| {
            ws::show(&ws::root(&p.root)?, &p.rev, &p.path)
        }),
        "git/branches" => call(params, |p: RootParams| ws::branches(&ws::root(&p.root)?)),
        "git/pr" => call(params, |p: RootParams| {
            ws::pull_request(&ws::root(&p.root)?)
        }),
        "git/run" => call(params, |p: GitRunParams| {
            ws::run(&ws::root(&p.root)?, &p.op).map(|output| GitRunResult { output })
        }),
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
