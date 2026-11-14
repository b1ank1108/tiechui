//! Tool contracts, stable registration, and concurrency scheduling.

pub mod bash;
pub mod output;

pub use bash::BashTool;
pub use tiechui_exec as exec;

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tiechui_model::{Content, ModelId, ModelRequest, StreamEvent, ToolCall, ToolSpec};
use tiechui_protocol::{SessionEpoch, ThreadId, TurnId};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

/// Caller-supplied identity retained by tool wrappers, including future policy checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionScope {
    pub thread_id: ThreadId,
    pub turn_id: TurnId,
}

#[derive(Clone)]
pub struct ToolContext {
    pub invoker: Option<Arc<dyn ToolInvoker>>,
    pub branch: Arc<Vec<tiechui_protocol::BranchEntry>>,
    pub parent_session: Option<PathBuf>,
    pub cwd: PathBuf,
    pub cancel: CancellationToken,
    pub updates: Arc<dyn Fn(ToolUpdate) + Send + Sync>,
    /// Queues module-owned audit data for persistence before tool results.
    pub append_entry: Arc<dyn Fn(CustomSessionEntry) -> Result<(), ToolError> + Send + Sync>,
    pub epoch: SessionEpoch,
    pub scope: ExecutionScope,
    pub origin: ToolOrigin,
}

/// Who started a call; set by the host, never derived from call data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolOrigin {
    #[default]
    Model,
    /// A command the user ran directly, such as `!command`.
    User,
}

#[derive(Debug, Clone)]
pub struct CustomSessionEntry {
    pub kind: String,
    pub data: Value,
}

#[derive(Debug, Clone)]
pub struct ToolUpdate {
    pub call_id: String,
    pub epoch: SessionEpoch,
    pub output: ToolOutput,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: Vec<Content>,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_content: Option<Value>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![Content::Text {
                text: text.into(),
                opaque: None,
            }],
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(pub String);

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for ToolError {}

impl From<std::io::Error> for ToolError {
    fn from(error: std::io::Error) -> Self {
        Self(error.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Concurrency {
    Shared,
    Exclusive,
    Delegated,
}

pub type ModelStream = futures::stream::BoxStream<'static, Result<StreamEvent, ToolError>>;

#[async_trait]
pub trait ToolInvoker: Send + Sync {
    /// The callable tool with this name after the allowed-tools filter.
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>>;
    /// Runs through the same validation, interceptors, scheduling and settlement as a model call.
    async fn invoke(&self, call: ToolCall, ctx: ToolContext) -> ToolOutput;
    /// The turn's model, for one-off requests such as permission reviews.
    fn model(&self) -> Option<ModelId> {
        None
    }
    /// Streams a one-off request on the turn's model provider.
    async fn model_stream(
        &self,
        _request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ToolError> {
        Err(ToolError("Model access is unavailable".into()))
    }
    /// Streams a one-off request on `<provider>/<id>`, any provider the session has loaded.
    async fn model_stream_on(
        &self,
        _model: &str,
        _request: ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ToolError> {
        Err(ToolError("Model access is unavailable".into()))
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    fn programmatic(&self) -> bool {
        false
    }
    fn deferrable(&self) -> bool {
        false
    }
    /// Known declaration provenance for read-only inspectors; never inferred from names.
    fn source(&self) -> Option<String> {
        None
    }
    /// Declared by the tool's provider as having no side effects, such as MCP `readOnlyHint`.
    fn read_only(&self) -> bool {
        false
    }
    fn concurrency(&self, input: &Value) -> Concurrency;
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError>;
}

#[derive(Default)]
pub struct ToolRegistry {
    allowed: Option<Vec<String>>,
    tools: BTreeMap<String, Arc<dyn Tool>>,
    sources: Vec<Arc<dyn ToolSource>>,
}

/// A live catalog contributed by an extension; snapshots must return promptly.
pub trait ToolSource: Send + Sync {
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

impl ToolRegistry {
    pub fn restrict(&mut self, names: Vec<String>) {
        self.allowed = Some(names);
    }
    fn allows(&self, name: &str) -> bool {
        self.allowed
            .as_ref()
            .is_none_or(|names| names.iter().any(|n| n == name))
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        let name = tool.spec().name.clone();
        if self.tools.contains_key(&name) {
            return Err(ToolError(format!("Tool already registered: {name}")));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    pub fn register_source(&mut self, source: Arc<dyn ToolSource>) {
        self.sources.push(source);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if !self.allows(name) {
            return None;
        }
        self.tools.get(name).cloned().or_else(|| {
            self.sources
                .iter()
                .flat_map(|source| source.tools())
                .find(|tool| tool.spec().name == name)
        })
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.callable()
            .iter()
            .map(|tool| tool.spec().clone())
            .collect()
    }

    pub fn callable(&self) -> Vec<Arc<dyn Tool>> {
        let mut tools = self.tools.clone();
        for tool in self.sources.iter().flat_map(|source| source.tools()) {
            tools.entry(tool.spec().name.clone()).or_insert(tool);
        }
        tools
            .into_values()
            .filter(|tool| self.allows(&tool.spec().name))
            .collect()
    }

    pub fn spec_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self.specs())
    }
}

/// Retain one scheduler across batches to enforce exclusivity between batches too.
#[derive(Default)]
pub struct ToolScheduler {
    gate: RwLock<()>,
}

impl ToolScheduler {
    pub async fn run_batch(
        &self,
        registry: &ToolRegistry,
        calls: Vec<ToolCall>,
        ctx: ToolContext,
    ) -> Vec<Result<ToolOutput, ToolError>> {
        join_all(calls.into_iter().map(|call| {
            let ctx = ctx.clone();
            async move {
                let tool = registry.get(&call.name)
                    .ok_or_else(|| ToolError(format!("Unknown tool: {}", call.name)))?;
                match tool.concurrency(&call.arguments) {
                    Concurrency::Delegated => tool.run(call, ctx).await,
                    Concurrency::Shared => {
                        let _guard = tokio::select! {
                            biased;
                            _ = ctx.cancel.cancelled() => return Err(ToolError("Operation aborted".into())),
                            guard = self.gate.read() => guard,
                        };
                        tool.run(call, ctx).await
                    }
                    Concurrency::Exclusive => {
                        let _guard = tokio::select! {
                            biased;
                            _ = ctx.cancel.cancelled() => return Err(ToolError("Operation aborted".into())),
                            guard = self.gate.write() => guard,
                        };
                        tool.run(call, ctx).await
                    }
                }
            }
        })).await
    }
}
