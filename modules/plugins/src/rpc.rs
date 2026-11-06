//! T2 custom methods over the MCP client's shared JSON-RPC service and process owner.
use rmcp::{
    ClientHandler, RoleClient,
    model::{CustomRequest, CustomResult, ProgressNotificationParam},
    service::{
        NotificationContext, RequestContext, RxJsonRpcMessage, TxJsonRpcMessage,
        serve_directly_with_ct,
    },
    transport::Transport,
};
use std::{sync::Arc, time::Duration};
use tiechui_plugin_api::{
    CancellationToken, ToolError, async_trait,
    process::{self, ProcessGroup},
    serde_json::{self, Value, json},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, broadcast},
};

const MAX_FRAME: usize = 1024 * 1024;
pub struct Method {
    pub method: String,
    pub params: Value,
}
#[async_trait]
pub trait Handler: Send + Sync {
    async fn request(
        &self,
        method: Method,
        cancel: CancellationToken,
    ) -> Result<Value, (i32, String)>;
}
struct Adapter {
    handler: Arc<dyn Handler>,
    progress: broadcast::Sender<ProgressNotificationParam>,
}
impl ClientHandler for Adapter {
    async fn on_custom_request(
        &self,
        request: CustomRequest,
        context: RequestContext<RoleClient>,
    ) -> Result<CustomResult, rmcp::ErrorData> {
        self.handler
            .request(
                Method {
                    method: request.method,
                    params: request.params.unwrap_or(Value::Null),
                },
                context.ct,
            )
            .await
            .map(CustomResult::new)
            .map_err(|(code, message)| {
                rmcp::ErrorData::new(rmcp::model::ErrorCode(code), message, None)
            })
    }
    async fn on_progress(
        &self,
        progress: ProgressNotificationParam,
        _: NotificationContext<RoleClient>,
    ) {
        let _ = self.progress.send(progress);
    }
}

struct Stdio {
    read: BufReader<tokio::process::ChildStdout>,
    buffer: Vec<u8>,
    write: Arc<Mutex<tokio::process::ChildStdin>>,
}
impl Transport<RoleClient> for Stdio {
    type Error = ToolError;
    fn send(
        &mut self,
        message: TxJsonRpcMessage<RoleClient>,
    ) -> impl Future<Output = Result<(), ToolError>> + Send + 'static {
        let writer = self.write.clone();
        async move {
            write(
                &mut *writer.lock().await,
                &serde_json::to_value(message).map_err(error)?,
            )
            .await
        }
    }
    async fn receive(&mut self) -> Option<RxJsonRpcMessage<RoleClient>> {
        let value = frame(&mut self.read, &mut self.buffer).await.ok()?;
        decode(value).ok()
    }
    async fn close(&mut self) -> Result<(), ToolError> {
        self.write.lock().await.shutdown().await.map_err(Into::into)
    }
}

pub struct Peer {
    peer: rmcp::Peer<RoleClient>,
    progress: broadcast::Sender<ProgressNotificationParam>,
    session: CancellationToken,
    pub stopped: CancellationToken,
}
impl Peer {
    pub fn start(
        mut group: ProcessGroup,
        handler: Arc<dyn Handler>,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>, ToolError> {
        let stdout = group
            .child
            .stdout
            .take()
            .ok_or_else(|| ToolError("Plugin stdout unavailable".into()))?;
        let stdin = group
            .child
            .stdin
            .take()
            .ok_or_else(|| ToolError("Plugin stdin unavailable".into()))?;
        let transport = Stdio {
            read: BufReader::new(stdout),
            buffer: vec![],
            write: Arc::new(Mutex::new(stdin)),
        };
        let (progress, _) = broadcast::channel(128);
        let service_cancel = CancellationToken::new();
        let service = serve_directly_with_ct(
            Adapter {
                handler,
                progress: progress.clone(),
            },
            transport,
            None,
            service_cancel.clone(),
        );
        let peer = Arc::new(Self {
            peer: service.peer().clone(),
            progress,
            session: cancel.clone(),
            stopped: CancellationToken::new(),
        });
        let stopped = peer.stopped.clone();
        let shutdown = peer.peer.clone();
        tokio::spawn(async move {
            let waiting = service.waiting();
            tokio::pin!(waiting);
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = tokio::time::timeout(Duration::from_millis(200), async {
                        let _ = shutdown.send_request(rmcp::model::ClientRequest::CustomRequest(CustomRequest::new("shutdown", Some(json!({}))))).await.map_err(error)?;
                        group.child.wait().await.map_err(ToolError::from)
                    }).await;
                },
                _ = group.child.wait() => {},
                _ = &mut waiting => {},
            }
            service_cancel.cancel();
            drop(group);
            stopped.cancel();
        });
        Ok(peer)
    }
    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: CancellationToken,
        progress: Option<Arc<dyn Fn(String) + Send + Sync>>,
    ) -> Result<Value, ToolError> {
        let result = process::request(
            &self.peer,
            rmcp::model::ClientRequest::CustomRequest(CustomRequest::new(method, Some(params))),
            timeout,
            cancel,
            self.session.clone(),
            progress.map(|callback| (self.progress.subscribe(), callback)),
        )
        .await?;
        result
            .get("tiechuiResult")
            .cloned()
            .ok_or_else(|| ToolError("Invalid plugin response".into()))
    }
}

async fn write(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &Value,
) -> Result<(), ToolError> {
    let mut bytes = serde_json::to_vec(value).map_err(error)?;
    if bytes.len() + 1 > MAX_FRAME {
        return Err(ToolError("Plugin frame exceeds 1 MiB".into()));
    }
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn frame(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    bytes: &mut Vec<u8>,
) -> Result<Value, ToolError> {
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return Err(ToolError("Plugin stdout closed".into()));
        }
        let end = buffer.iter().position(|b| *b == b'\n');
        let count = end.map_or(buffer.len(), |n| n + 1);
        if bytes.len() + count > MAX_FRAME {
            return Err(ToolError("Plugin frame exceeds 1 MiB".into()));
        }
        bytes.extend_from_slice(&buffer[..count]);
        reader.consume(count);
        if end.is_some() {
            let result = serde_json::from_slice(bytes).map_err(error);
            bytes.clear();
            return result;
        }
    }
}

pub(super) fn error(error: impl std::fmt::Display) -> ToolError {
    ToolError(error.to_string())
}

fn decode(mut value: Value) -> Result<RxJsonRpcMessage<RoleClient>, ToolError> {
    // Validate the strict public envelope before rmcp's compatibility parser.
    serde_json::from_value::<tiechui_plugin_api::protocol::rpc::JsonRpcMessage>(value.clone())
        .map_err(error)?;
    if let Some(id) = value.get("id")
        && id.as_i64().is_none_or(|id| id < 0)
    {
        return Err(ToolError("Plugin IDs must be non-negative integers".into()));
    }
    // Keep custom result shapes out of rmcp's MCP-specific result normalization.
    if let Some(result) = value.get_mut("result") {
        *result = json!({"tiechuiResult":result.take()});
    }
    serde_json::from_value(value).map_err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn framing_survives_partial_reads_cancellation_and_multiple_frames() {
        let (mut writer, read) = tokio::io::duplex(4096);
        let mut read = BufReader::new(read);
        let mut buffer = vec![];
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), frame(&mut read, &mut buffer))
                .await
                .is_err()
        );
        writer
            .write_all(
                b"\"result\":\"line\\ntext\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":null}\n",
            )
            .await
            .unwrap();
        let first = frame(&mut read, &mut buffer).await.unwrap();
        assert_eq!(first["result"], "line\ntext");
        assert!(decode(first).is_ok());
        assert_eq!(frame(&mut read, &mut buffer).await.unwrap()["id"], 2);
    }

    #[tokio::test]
    async fn framing_rejects_oversize_truncated_and_invalid_json() {
        let mut reader = BufReader::new(std::io::Cursor::new(vec![b'x'; MAX_FRAME + 1]));
        assert!(
            frame(&mut reader, &mut vec![])
                .await
                .unwrap_err()
                .0
                .contains("1 MiB")
        );
        for bytes in [b"{}".as_slice(), b"invalid\n"] {
            assert!(
                frame(&mut BufReader::new(bytes), &mut vec![])
                    .await
                    .is_err()
            );
        }
        for value in [
            json!([]),
            json!({"jsonrpc":"1.0","id":1,"result":null}),
            json!({"jsonrpc":"2.0","id":1,"result":null,"error":{"code":1,"message":"bad"}}),
            json!({"jsonrpc":"2.0","id":"1","result":null}),
        ] {
            assert!(decode(value).is_err());
        }
    }
}
