use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tiechui_plugin_api::{
    CancellationToken, CommandSession, ToolError,
    protocol::{BtwEvent, BtwView, InputContent, ModelInfo, ThinkingLevel},
    serde_json::{Value, json},
};
use tokio::sync::{oneshot, watch};

use crate::{Btw, WAIT, clamp, emit, find_model, last_completed_answer};

const RPC_KEY: &str = "__btw_rpc";

pub enum BtwRpcOp {
    Open {
        create: bool,
    },
    Ask {
        content: Vec<InputContent>,
    },
    Stop,
    Share,
    Model {
        model: String,
        thinking: Option<ThinkingLevel>,
    },
    Close,
}

pub struct BtwRpcOutcome {
    pub view: Option<BtwView>,
    pub emitted: u32,
    pub settle: Option<watch::Receiver<bool>>,
}

#[derive(Debug, Clone)]
pub struct BtwRpcError {
    pub invalid: bool,
    pub message: String,
}

impl BtwRpcError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            invalid: true,
            message: message.into(),
        }
    }

    fn internal(error: ToolError) -> Self {
        Self {
            invalid: false,
            message: error.0,
        }
    }
}

type RpcReply = oneshot::Sender<Result<BtwRpcOutcome, BtwRpcError>>;

static NEXT_RPC: AtomicU64 = AtomicU64::new(1);
static PENDING: Mutex<Option<HashMap<u64, (BtwRpcOp, RpcReply)>>> = Mutex::new(None);

fn with_pending<R>(f: impl FnOnce(&mut HashMap<u64, (BtwRpcOp, RpcReply)>) -> R) -> R {
    let mut guard = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(HashMap::new))
}

pub struct BtwRpcTicket {
    id: u64,
    rx: Option<oneshot::Receiver<Result<BtwRpcOutcome, BtwRpcError>>>,
}

impl BtwRpcTicket {
    pub async fn recv(mut self) -> Result<BtwRpcOutcome, BtwRpcError> {
        let Some(rx) = self.rx.take() else {
            return Err(BtwRpcError {
                invalid: false,
                message: "side request ticket already consumed".into(),
            });
        };
        rx.await.unwrap_or_else(|_| {
            Err(BtwRpcError {
                invalid: false,
                message: "session ended".into(),
            })
        })
    }
}

impl Drop for BtwRpcTicket {
    fn drop(&mut self) {
        if self.rx.is_some() {
            with_pending(|map| {
                map.remove(&self.id);
            });
        }
    }
}

pub fn prepare_rpc(op: BtwRpcOp) -> (Value, BtwRpcTicket) {
    let id = NEXT_RPC.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    with_pending(|map| {
        map.insert(id, (op, tx));
    });
    (json!({ RPC_KEY: id }), BtwRpcTicket { id, rx: Some(rx) })
}

pub(crate) fn rpc_id(data: &Value) -> Option<u64> {
    data.get(RPC_KEY).and_then(Value::as_u64)
}

fn has_input(content: &[InputContent]) -> bool {
    content.iter().any(|part| match part {
        InputContent::Text { text } => !text.trim().is_empty(),
        InputContent::Image { data, .. } => !data.trim().is_empty(),
    })
}

impl Btw {
    pub(crate) async fn run_rpc(
        &self,
        id: u64,
        session: CommandSession,
        cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        let entry = with_pending(|map| map.remove(&id));
        if let Some((op, reply)) = entry {
            let outcome = self.execute_rpc(op, session, cancel).await;
            let _ = reply.send(outcome);
        }
        Ok(String::new())
    }

    async fn execute_rpc(
        &self,
        op: BtwRpcOp,
        session: CommandSession,
        cancel: CancellationToken,
    ) -> Result<BtwRpcOutcome, BtwRpcError> {
        match op {
            BtwRpcOp::Open { create } => self.rpc_open(create, &session).await,
            BtwRpcOp::Ask { content } => self.rpc_ask(content, session, cancel).await,
            BtwRpcOp::Stop => self.rpc_stop(),
            BtwRpcOp::Share => self.rpc_share(&session).await,
            BtwRpcOp::Model { model, thinking } => self.rpc_model(&model, thinking, &session).await,
            BtwRpcOp::Close => self.rpc_close(&session).await,
        }
    }

    async fn rpc_open(
        &self,
        create: bool,
        session: &CommandSession,
    ) -> Result<BtwRpcOutcome, BtwRpcError> {
        if !create {
            let view = self
                .state()
                .map_err(BtwRpcError::internal)?
                .as_ref()
                .map(|t| t.view.clone());
            return Ok(BtwRpcOutcome {
                view,
                emitted: 0,
                settle: None,
            });
        }
        self.create(&*session.host).map_err(BtwRpcError::internal)?;
        let view = self
            .publish(&*session.host, true)
            .await
            .map_err(BtwRpcError::internal)?;
        Ok(BtwRpcOutcome {
            emitted: u32::from(view.is_some()),
            view,
            settle: None,
        })
    }

    fn ensure_not_busy(&self, message: &str) -> Result<(), BtwRpcError> {
        let busy = self
            .state()
            .map_err(BtwRpcError::internal)?
            .as_ref()
            .is_some_and(|t| t.view.busy);
        if busy {
            return Err(BtwRpcError::invalid(message));
        }
        Ok(())
    }

    async fn rpc_ask(
        &self,
        content: Vec<InputContent>,
        session: CommandSession,
        cancel: CancellationToken,
    ) -> Result<BtwRpcOutcome, BtwRpcError> {
        if !has_input(&content) {
            return Err(BtwRpcError::invalid("Side question cannot be empty."));
        }
        self.ensure_not_busy(WAIT)?;
        self.create(&*session.host).map_err(BtwRpcError::internal)?;
        let work = self
            .start_question(content, &*session.host, &cancel)
            .map_err(BtwRpcError::internal)?;
        let view = self
            .publish(&*session.host, false)
            .await
            .map_err(BtwRpcError::internal)?;
        if let Some(work) = work {
            self.spawn_answer(session.host, work);
        }
        Ok(BtwRpcOutcome {
            emitted: u32::from(view.is_some()),
            view,
            settle: None,
        })
    }

    fn rpc_stop(&self) -> Result<BtwRpcOutcome, BtwRpcError> {
        let state = self.state().map_err(BtwRpcError::internal)?;
        let Some(thread) = state.as_ref() else {
            return Ok(BtwRpcOutcome {
                view: None,
                emitted: 0,
                settle: None,
            });
        };
        let settle = if thread.view.busy {
            thread.cancel.cancel();
            Some(thread.settled.clone())
        } else {
            None
        };
        Ok(BtwRpcOutcome {
            view: Some(thread.view.clone()),
            emitted: 0,
            settle,
        })
    }

    async fn rpc_share(&self, session: &CommandSession) -> Result<BtwRpcOutcome, BtwRpcError> {
        let answer = self
            .state()
            .map_err(BtwRpcError::internal)?
            .as_ref()
            .and_then(last_completed_answer)
            .ok_or_else(|| BtwRpcError::invalid("No completed side answer to share."))?;
        self.apply_share(&*session.host, answer.0, answer.1)
            .await
            .map_err(BtwRpcError::internal)?;
        let view = self
            .publish(&*session.host, false)
            .await
            .map_err(BtwRpcError::internal)?;
        Ok(BtwRpcOutcome {
            emitted: 1 + u32::from(view.is_some()),
            view,
            settle: None,
        })
    }

    fn apply_model_selection(
        &self,
        model: ModelInfo,
        thinking: Option<ThinkingLevel>,
    ) -> Result<(), BtwRpcError> {
        if let Some(thread) = self.state().map_err(BtwRpcError::internal)?.as_mut() {
            if let Some(level) = thinking {
                thread.thinking = level;
            }
            thread.view.thinking = clamp(thread.thinking, &model);
            thread.view.model = model;
            thread.view.context = None;
        }
        Ok(())
    }

    async fn rpc_model(
        &self,
        spec: &str,
        thinking: Option<ThinkingLevel>,
        session: &CommandSession,
    ) -> Result<BtwRpcOutcome, BtwRpcError> {
        self.ensure_not_busy("Stop the side reply before changing its model.")?;
        let context = session
            .host
            .model_context()
            .map_err(BtwRpcError::internal)?;
        let model: ModelInfo = find_model(context.models, spec)
            .ok_or_else(|| BtwRpcError::invalid(format!("Unknown side model: {spec}")))?;
        self.create(&*session.host).map_err(BtwRpcError::internal)?;
        self.apply_model_selection(model, thinking)?;
        let view = self
            .publish(&*session.host, false)
            .await
            .map_err(BtwRpcError::internal)?;
        Ok(BtwRpcOutcome {
            emitted: u32::from(view.is_some()),
            view,
            settle: None,
        })
    }

    async fn rpc_close(&self, session: &CommandSession) -> Result<BtwRpcOutcome, BtwRpcError> {
        let settle = self.discard_with_settle();
        emit(&*session.host, BtwEvent::Closed)
            .await
            .map_err(BtwRpcError::internal)?;
        Ok(BtwRpcOutcome {
            view: None,
            emitted: 1,
            settle,
        })
    }
}
