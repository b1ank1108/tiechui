use crate::{CancellationToken, Context, Plugin, Result, dispatch, error, protocol::*};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
    task::JoinSet,
};

const MAX_FRAME: usize = 1024 * 1024;
pub(crate) struct Peer {
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    pending: Mutex<BTreeMap<u64, oneshot::Sender<Result<Value>>>>,
    next: AtomicU64,
    stopped: CancellationToken,
}
impl Peer {
    pub(crate) fn send(&self, value: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&value).map_err(|e| error(-32000, e.to_string()))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_FRAME {
            return Err(error(-32600, "Frame exceeds 1 MiB"));
        }
        self.outgoing
            .send(bytes)
            .map_err(|_| error(-32000, "Transport closed"))
    }
    pub(crate) async fn request(
        self: &Arc<Self>,
        method: &str,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        if cancel.is_cancelled() {
            return Err(error(-32800, "Cancelled"));
        }
        if self.stopped.is_cancelled() {
            return Err(error(-32000, "Transport closed"));
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        let _guard = Pending {
            peer: self.clone(),
            id,
        };
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(error(-32800, "Cancelled")),
            _ = self.stopped.cancelled() => Err(error(-32000, "Transport closed")),
            response = rx => response.unwrap_or_else(|_| Err(error(-32000, "Transport closed"))),
        }
    }
    fn reply(&self, id: u64, result: Result<Value>) -> Result<()> {
        self.send(match result {
            Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
            Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
        })
    }
}
struct Pending {
    peer: Arc<Peer>,
    id: u64,
}
impl Drop for Pending {
    fn drop(&mut self) {
        if self
            .peer
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.id)
            .is_some()
        {
            let _ = self.peer.send(json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":self.id}}));
        }
    }
}

/// Run on supplied streams (useful for offline tests). Cancellation drops all handlers.
pub async fn serve<R, W>(
    read: R,
    mut write: W,
    plugin: impl Plugin,
    cancel: CancellationToken,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let stopped = cancel.child_token();
    let (outgoing, mut frames) = mpsc::unbounded_channel();
    let peer = Arc::new(Peer {
        outgoing,
        pending: Mutex::default(),
        next: AtomicU64::new(1),
        stopped: stopped.clone(),
    });
    let writer = async {
        while let Some(bytes) = frames.recv().await {
            write
                .write_all(&bytes)
                .await
                .map_err(|e| error(-32000, e.to_string()))?;
            write
                .flush()
                .await
                .map_err(|e| error(-32000, e.to_string()))?;
        }
        Ok(())
    };
    let reader = run(
        BufReader::new(read),
        Arc::new(plugin),
        peer.clone(),
        stopped.clone(),
    );
    tokio::pin!(writer);
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => Ok(()),
        result = &mut writer => result,
        result = reader => result,
    };
    stopped.cancel();
    peer.pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    drop(peer);
    // Drain the shutdown response, but never block exit on a host that stopped reading.
    if result.is_ok() && !cancel.is_cancelled() {
        let _ = tokio::time::timeout(std::time::Duration::from_millis(100), writer).await;
    }
    result
}

async fn run<R: AsyncBufRead + Unpin>(
    mut read: R,
    plugin: Arc<dyn Plugin>,
    peer: Arc<Peer>,
    stopped: CancellationToken,
) -> Result<()> {
    let mut bytes = Vec::new();
    let mut declaration: Option<Arc<PluginInitialize>> = None;
    let mut initializing = false;
    let mut active = BTreeMap::<u64, CancellationToken>::new();
    let mut tasks = JoinSet::<(u64, Result<Value>, Option<PluginInitialize>, bool)>::new();
    loop {
        let value = tokio::select! {
            biased;
            _ = stopped.cancelled() => return Ok(()),
            done = tasks.join_next(), if !tasks.is_empty() => {
                let (id, result, init, was_initializing) = done.ok_or_else(|| error(-32000, "Missing handler"))?.map_err(|e| error(-32000, e.to_string()))?;
                if let Some(token) = active.remove(&id) { token.cancel(); }
                if was_initializing { initializing = false; }
                if let Some(init) = init { declaration = Some(Arc::new(init)); }
                peer.reply(id, result)?;
                continue;
            }
            value = frame(&mut read, &mut bytes) => value?,
        };
        let Some(value) = value else {
            return Ok(());
        };
        let message: rpc::JsonRpcMessage =
            serde_json::from_value(value.clone()).map_err(|e| error(-32600, e.to_string()))?;
        if value
            .get("id")
            .is_some_and(|id| id.as_i64().is_none_or(|id| id < 0))
        {
            return Err(error(-32600, "IDs must be non-negative integers"));
        }
        match message {
            rpc::JsonRpcMessage::Success(_) | rpc::JsonRpcMessage::Failure(_) => {
                let id = value["id"]
                    .as_u64()
                    .ok_or_else(|| error(-32600, "Invalid ID"))?;
                if let Some(tx) = peer
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id)
                {
                    let result = match message {
                        rpc::JsonRpcMessage::Success(v) => Ok(v.result),
                        rpc::JsonRpcMessage::Failure(v) => Err(v.error),
                        _ => unreachable!(),
                    };
                    let _ = tx.send(result);
                }
            }
            rpc::JsonRpcMessage::Notification(notification) => {
                if notification.method == "notifications/cancelled" {
                    let id = value["params"]["requestId"]
                        .as_u64()
                        .ok_or_else(|| error(-32602, "Invalid cancellation ID"))?;
                    if let Some(token) = active.get(&id) {
                        token.cancel();
                    }
                    // Each direction owns its IDs; this notification cancels host requests only.
                }
            }
            rpc::JsonRpcMessage::Request(request) => {
                let id = value["id"]
                    .as_u64()
                    .ok_or_else(|| error(-32600, "Invalid ID"))?;
                if active.contains_key(&id) {
                    return Err(error(-32600, "Duplicate active request ID"));
                }
                let mut params = value.get("params").cloned().unwrap_or(json!({}));
                if request.method == "initialize" {
                    if declaration.is_some() || initializing {
                        peer.reply(id, Err(error(-32600, "Already initialized")))?;
                        continue;
                    }
                    if let Some(object) = params.as_object_mut() {
                        object.remove("_meta");
                    }
                    let parsed = serde_json::from_value::<PluginInitialize>(params)
                        .map_err(|e| error(-32602, e.to_string()));
                    let cancel = stopped.child_token();
                    active.insert(id, cancel.clone());
                    initializing = true;
                    let plugin = plugin.clone();
                    tasks.spawn(async move {
                        let initialize = async {
                            let init = parsed?;
                            if init.api_version != 1 {
                                return Err(error(-32002, "Incompatible API version"));
                            }
                            plugin.initialize(&init, cancel.clone()).await?;
                            Ok(init)
                        };
                        let result = tokio::select! {
                            biased;
                            _ = cancel.cancelled() => Err(error(-32800, "Cancelled")),
                            result = initialize => result,
                        };
                        match result {
                            Ok(init) => (id, Ok(json!(init)), Some(init), true),
                            Err(e) => (id, Err(e), None, true),
                        }
                    });
                    continue;
                }
                let Some(declaration) = declaration.clone() else {
                    peer.reply(id, Err(error(-32001, "Not initialized")))?;
                    continue;
                };
                if request.method == "shutdown" {
                    peer.reply(id, Ok(Value::Null))?;
                    return Ok(());
                }
                let cancel = stopped.child_token();
                active.insert(id, cancel.clone());
                let context = Context {
                    cancel: cancel.clone(),
                    peer: peer.clone(),
                    progress_token: params["_meta"]["progressToken"].as_u64(),
                };
                let plugin = plugin.clone();
                tasks.spawn(async move {
                    let result = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => Err(error(-32800, "Cancelled")),
                        result = dispatch(plugin.as_ref(), &declaration, &request.method, params, context) => result,
                    };
                    (id, result, None, false)
                });
            }
        }
    }
}

async fn frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    bytes: &mut Vec<u8>,
) -> Result<Option<Value>> {
    loop {
        let buffer = reader
            .fill_buf()
            .await
            .map_err(|e| error(-32000, e.to_string()))?;
        if buffer.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(error(-32600, "Truncated frame"))
            };
        }
        let end = buffer.iter().position(|b| *b == b'\n');
        let count = end.map_or(buffer.len(), |n| n + 1);
        if bytes.len() + count > MAX_FRAME {
            return Err(error(-32600, "Frame exceeds 1 MiB"));
        }
        bytes.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if end.is_some() {
            let value = serde_json::from_slice(bytes).map_err(|e| error(-32600, e.to_string()));
            bytes.clear();
            return value.map(Some);
        }
    }
}
