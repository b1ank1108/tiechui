//! `btw/*` and `turn/approve`: side conversations and interactive approvals on open sessions.

use std::sync::Arc;

use serde_json::Value;
use tiechui_module_btw::{BtwRpcError, BtwRpcOp, prepare_rpc};
use tiechui_protocol::app::{
    ApprovalParams, BtwAskParams, BtwModelParams, BtwOpenParams, ThreadParams,
};
use tiechui_protocol::{BtwView, Op, ThreadId, rpc};

use super::projection::Projection;
use super::{Live, Reply, Server, internal, invalid, lock, parse, value};

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        "btw/open"
            | "btw/ask"
            | "btw/stop"
            | "btw/share"
            | "btw/model"
            | "btw/set_model"
            | "btw/close"
            | "turn/approve"
    )
}

pub(super) async fn handle(server: &Arc<Server>, method: &str, params: Value) -> Reply {
    match method {
        "turn/approve" => approve(server, params).await,
        "btw/open" => open(server, params).await,
        "btw/ask" => ask(server, params).await,
        "btw/stop" => stop(server, params).await,
        "btw/share" => share(server, params).await,
        "btw/model" | "btw/set_model" => set_model(server, params).await,
        "btw/close" => close(server, params).await,
        _ => Err(internal(format!("unhandled btw method {method}"))),
    }
}

async fn approve(server: &Server, params: Value) -> Reply {
    let p: ApprovalParams = parse(params)?;
    server
        .submit(
            &p.thread_id,
            Op::ApprovalResponse {
                approval_id: p.approval_id,
                decision: p.decision,
            },
        )
        .await
        .and_then(value)
}

async fn open(server: &Server, params: Value) -> Reply {
    let p: BtwOpenParams = parse(params)?;
    let out = call_btw(server, &p.thread_id, BtwRpcOp::Open { create: p.create }).await?;
    value(out.view)
}

async fn ask(server: &Server, params: Value) -> Reply {
    let p: BtwAskParams = parse(params)?;
    let out = call_btw(server, &p.thread_id, BtwRpcOp::Ask { content: p.content }).await?;
    require_view(out.view).and_then(value)
}

async fn stop(server: &Server, params: Value) -> Reply {
    let p: ThreadParams = parse(params)?;
    let out = call_btw(server, &p.thread_id, BtwRpcOp::Stop).await?;
    let view = if out.settled {
        lock(&out.live.projection).btw_view()
    } else {
        out.view
    };
    value(view)
}

async fn share(server: &Server, params: Value) -> Reply {
    let p: ThreadParams = parse(params)?;
    let out = call_btw(server, &p.thread_id, BtwRpcOp::Share).await?;
    require_view(out.view).and_then(value)
}

async fn set_model(server: &Server, params: Value) -> Reply {
    let p: BtwModelParams = parse(params)?;
    let out = call_btw(
        server,
        &p.thread_id,
        BtwRpcOp::Model {
            model: p.model,
            thinking: p.thinking,
        },
    )
    .await?;
    require_view(out.view).and_then(value)
}

async fn close(server: &Server, params: Value) -> Reply {
    let p: ThreadParams = parse(params)?;
    call_btw(server, &p.thread_id, BtwRpcOp::Close).await?;
    Ok(Value::Null)
}

fn require_view(view: Option<BtwView>) -> Result<BtwView, rpc::Error> {
    view.ok_or_else(|| internal("missing side conversation view"))
}

fn map_error(error: BtwRpcError) -> rpc::Error {
    if error.invalid {
        invalid(error.message)
    } else {
        internal(error.message)
    }
}

struct ResolvedBtw {
    live: Arc<Live>,
    view: Option<BtwView>,
    settled: bool,
}

async fn call_btw(
    server: &Server,
    thread_id: &ThreadId,
    op: BtwRpcOp,
) -> Result<ResolvedBtw, rpc::Error> {
    let live = server.live(thread_id)?;
    if !lock(&live.projection).has_command("btw") {
        return Err(invalid("btw module is disabled"));
    }
    let (data, ticket) = prepare_rpc(op);
    let sub_id = server
        .submit_tracked(
            thread_id,
            &live,
            Op::CommandInput {
                name: "btw".into(),
                data,
            },
        )
        .await?;
    let received = tokio::select! {
        biased;
        _ = live.shutdown.cancelled() => Err(BtwRpcError {
            invalid: false,
            message: "session ended".into(),
        }),
        res = ticket.recv() => res,
    };
    let outcome = match received {
        Ok(outcome) => outcome,
        Err(error) => {
            lock(&live.projection).untrack_submission(&sub_id);
            return Err(map_error(error));
        }
    };
    let sync = if outcome.emitted > 0 {
        wait_projection(&live, |p| p.submission_count(&sub_id) >= outcome.emitted).await
    } else {
        Ok(())
    };
    lock(&live.projection).untrack_submission(&sub_id);
    sync?;
    let settled = if let Some(mut settle) = outcome.settle {
        let _ = settle.wait_for(|done| *done).await;
        wait_projection(&live, |p| !p.side_busy()).await?;
        true
    } else {
        false
    };
    Ok(ResolvedBtw {
        live,
        view: outcome.view,
        settled,
    })
}

async fn wait_projection(
    live: &Live,
    mut ready: impl FnMut(&Projection) -> bool,
) -> Result<(), rpc::Error> {
    loop {
        let notified = live.applied.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if ready(&lock(&live.projection)) {
            return Ok(());
        }
        if live.shutdown.is_cancelled() {
            return Err(internal("session ended"));
        }
        tokio::select! {
            biased;
            _ = live.shutdown.cancelled() => return Err(internal("session ended")),
            () = &mut notified => {}
        }
    }
}
