//! The JSON-RPC connection to `codex app-server`: requests with awaited responses, and replies
//! to the server's own requests.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::oneshot;

use super::wire::RpcError;

/// How long a single write to Codex's stdin may block.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Why a call did not return a result.
#[derive(Debug)]
pub enum CallError {
    /// Codex answered with an error.
    Rpc(RpcError),
    Timeout,
    /// Codex went away or could not be written to.
    Closed(String),
}

impl CallError {
    pub fn describe(&self, what: &str) -> String {
        match self {
            Self::Rpc(error) => format!("codex {what}: {}", error.message),
            Self::Timeout => format!("codex {what} timed out"),
            Self::Closed(reason) => format!("codex {what}: {reason}"),
        }
    }
}

type Pending = oneshot::Sender<Result<Value, RpcError>>;

pub struct Rpc {
    stdin: tokio::sync::Mutex<ChildStdin>,
    next: AtomicI64,
    pending: Mutex<HashMap<i64, Pending>>,
}

impl Rpc {
    pub fn new(stdin: ChildStdin) -> Self {
        Self {
            stdin: tokio::sync::Mutex::new(stdin),
            next: AtomicI64::new(1),
            pending: Mutex::default(),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Pending>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    async fn write(&self, frame: &Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(frame).map_err(|error| error.to_string())?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        let write = async {
            stdin.write_all(&line).await?;
            stdin.flush().await
        };
        match tokio::time::timeout(WRITE_TIMEOUT, write).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(format!("cannot write to codex: {error}")),
            Err(_) => Err("codex stopped reading its input".into()),
        }
    }

    /// Sends a request and returns what resolves with its response.
    pub async fn send(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<(i64, oneshot::Receiver<Result<Value, RpcError>>), CallError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let params = serde_json::to_value(params).map_err(|e| CallError::Closed(e.to_string()))?;
        let (tx, rx) = oneshot::channel();
        self.pending().insert(id, tx);
        if let Err(error) = self
            .write(&json!({"id": id, "method": method, "params": params}))
            .await
        {
            self.pending().remove(&id);
            return Err(CallError::Closed(error));
        }
        Ok((id, rx))
    }

    pub async fn call(
        &self,
        method: &str,
        params: impl Serialize,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let (id, rx) = self.send(method, params).await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => Err(CallError::Rpc(error)),
            Ok(Err(_)) => Err(CallError::Closed("codex exited".into())),
            Err(_) => {
                self.pending().remove(&id);
                Err(CallError::Timeout)
            }
        }
    }

    pub async fn notify(&self, method: &str) -> Result<(), String> {
        self.write(&json!({"method": method})).await
    }

    /// Resolves the call a response belongs to; false when nobody waits for it any more.
    pub fn resolve(&self, id: &Value, outcome: Result<Value, RpcError>) -> bool {
        let Some(id) = id.as_i64() else { return false };
        match self.pending().remove(&id) {
            Some(waiting) => waiting.send(outcome).is_ok(),
            None => false,
        }
    }

    /// Fails every call still waiting, as Codex is gone.
    pub fn close(&self) {
        self.pending().clear();
    }

    pub async fn respond(&self, id: &Value, result: Value) -> Result<(), String> {
        self.write(&json!({"id": id, "result": result})).await
    }

    pub async fn reject(&self, id: &Value, code: i64, message: &str) -> Result<(), String> {
        self.write(&json!({"id": id, "error": {"code": code, "message": message}}))
            .await
    }
}
