use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::oneshot;

const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    #[serde(default)]
    pub code: i64,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub data: Option<Value>,
}

#[derive(Debug)]
pub enum CallError {
    Rpc(RpcError),
    Timeout,
    Closed(String),
}

impl CallError {
    pub fn describe(&self, peer: &str, what: &str) -> String {
        match self {
            Self::Rpc(error) => format!("{peer} {what}: {}", error.message),
            Self::Timeout => format!("{peer} {what} timed out"),
            Self::Closed(reason) => format!("{peer} {what}: {reason}"),
        }
    }
}

type Pending = oneshot::Sender<Result<Value, RpcError>>;

pub struct Rpc {
    stdin: tokio::sync::Mutex<ChildStdin>,
    peer: &'static str,
    v2: bool,
    next: AtomicI64,
    pending: Mutex<HashMap<i64, Pending>>,
}

impl Rpc {
    pub fn new(stdin: ChildStdin, peer: &'static str, v2: bool) -> Self {
        Self {
            stdin: tokio::sync::Mutex::new(stdin),
            peer,
            v2,
            next: AtomicI64::new(1),
            pending: Mutex::default(),
        }
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Pending>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn envelope(&self, mut body: Map<String, Value>) -> Value {
        if self.v2 {
            body.insert("jsonrpc".into(), Value::String("2.0".into()));
        }
        Value::Object(body)
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
            Ok(Err(error)) => Err(format!("cannot write to {}: {error}", self.peer)),
            Err(_) => Err(format!("{} stopped reading its input", self.peer)),
        }
    }

    pub fn next_id(&self) -> i64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    pub async fn write_request(
        &self,
        id: i64,
        method: &str,
        params: impl Serialize,
    ) -> Result<(), String> {
        let params = serde_json::to_value(params).map_err(|e| e.to_string())?;
        let mut body = Map::new();
        body.insert("id".into(), json!(id));
        body.insert("method".into(), json!(method));
        body.insert("params".into(), params);
        self.write(&self.envelope(body)).await
    }

    pub async fn send_with_id(
        &self,
        id: i64,
        method: &str,
        params: impl Serialize,
    ) -> Result<oneshot::Receiver<Result<Value, RpcError>>, CallError> {
        let (tx, rx) = oneshot::channel();
        self.pending().insert(id, tx);
        if let Err(error) = self.write_request(id, method, params).await {
            self.pending().remove(&id);
            return Err(CallError::Closed(error));
        }
        Ok(rx)
    }

    pub async fn send(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<(i64, oneshot::Receiver<Result<Value, RpcError>>), CallError> {
        let id = self.next_id();
        let rx = self.send_with_id(id, method, params).await?;
        Ok((id, rx))
    }

    pub async fn call_with_id(
        &self,
        id: i64,
        method: &str,
        params: impl Serialize,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let rx = self.send_with_id(id, method, params).await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => Err(CallError::Rpc(error)),
            Ok(Err(_)) => Err(CallError::Closed(format!("{} exited", self.peer))),
            Err(_) => {
                self.pending().remove(&id);
                Err(CallError::Timeout)
            }
        }
    }

    pub async fn call(
        &self,
        method: &str,
        params: impl Serialize,
        timeout: Duration,
    ) -> Result<Value, CallError> {
        let id = self.next_id();
        self.call_with_id(id, method, params, timeout).await
    }

    pub async fn notify(&self, method: &str) -> Result<(), String> {
        let mut body = Map::new();
        body.insert("method".into(), json!(method));
        self.write(&self.envelope(body)).await
    }

    pub async fn notify_params(&self, method: &str, params: impl Serialize) -> Result<(), String> {
        let params = serde_json::to_value(params).map_err(|e| e.to_string())?;
        let mut body = Map::new();
        body.insert("method".into(), json!(method));
        body.insert("params".into(), params);
        self.write(&self.envelope(body)).await
    }

    pub fn resolve(&self, id: &Value, outcome: Result<Value, RpcError>) -> bool {
        let Some(id) = id.as_i64().or_else(|| id.as_str()?.parse().ok()) else {
            return false;
        };
        match self.pending().remove(&id) {
            Some(waiting) => waiting.send(outcome).is_ok(),
            None => false,
        }
    }

    pub fn close(&self) {
        self.pending().clear();
    }

    pub async fn respond(&self, id: &Value, result: Value) -> Result<(), String> {
        let mut body = Map::new();
        body.insert("id".into(), id.clone());
        body.insert("result".into(), result);
        self.write(&self.envelope(body)).await
    }

    pub async fn reject(&self, id: &Value, code: i64, message: &str) -> Result<(), String> {
        let mut body = Map::new();
        body.insert("id".into(), id.clone());
        body.insert("error".into(), json!({"code": code, "message": message}));
        self.write(&self.envelope(body)).await
    }
}
