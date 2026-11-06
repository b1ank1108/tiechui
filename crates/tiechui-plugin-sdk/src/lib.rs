//! Typed T2 handlers over newline-delimited JSON-RPC. Stdout belongs to the transport.

#[cfg(test)]
mod tests;
mod transport;

pub use async_trait::async_trait;
pub use tiechui_protocol as protocol;
pub use tokio_util::sync::CancellationToken;

use protocol::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
pub use transport::serve;

pub type Result<T> = std::result::Result<T, rpc::Error>;

pub fn error(code: i64, message: impl Into<String>) -> rpc::Error {
    rpc::Error {
        code,
        message: message.into(),
        data: None,
    }
}

/// Implement only declared capabilities. Unknown handlers fail closed.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    async fn initialize(
        &self,
        _declaration: &PluginInitialize,
        _cancel: CancellationToken,
    ) -> Result<()> {
        Ok(())
    }
    async fn tool(
        &self,
        _name: &str,
        _arguments: Value,
        _context: Context,
    ) -> Result<PluginOutput> {
        Err(error(-32601, "No tool handler"))
    }
    async fn command(&self, _name: &str, _arguments: &str, _context: Context) -> Result<String> {
        Err(error(-32601, "No command handler"))
    }
    async fn before(
        &self,
        _stage: PluginStage,
        _call: PluginCall,
        _context: Context,
    ) -> Result<PluginVerdict> {
        Err(error(-32601, "No before interceptor"))
    }
    async fn after(
        &self,
        _stage: PluginStage,
        _call: PluginCall,
        _output: Value,
        _context: Context,
    ) -> Result<Option<PluginOutput>> {
        Err(error(-32601, "No after interceptor"))
    }
}

/// A request-scoped peer. Its token also cancels nested UI requests.
#[derive(Clone)]
pub struct Context {
    pub cancel: CancellationToken,
    pub(crate) peer: Arc<transport::Peer>,
    pub(crate) progress_token: Option<u64>,
}

impl Context {
    pub fn progress(&self, progress: f64, message: impl Into<String>) -> Result<()> {
        if self.cancel.is_cancelled() {
            return Err(error(-32800, "Cancelled"));
        }
        if !progress.is_finite() {
            return Err(error(-32602, "Progress must be finite"));
        }
        if let Some(token) = self.progress_token {
            self.peer.send(json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":token,"progress":progress,"message":message.into()}}))?;
        }
        Ok(())
    }

    /// Low-level escape hatch; the host still checks methods and capabilities.
    pub async fn request<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let value = self.peer.request(method, params, &self.cancel).await?;
        serde_json::from_value(value).map_err(|e| error(-32602, e.to_string()))
    }
    pub async fn notify(&self, title: &str, text: &str) -> Result<()> {
        self.request("ui.notify", json!({"title":title,"text":text}))
            .await
    }
    pub async fn select(
        &self,
        title: &str,
        text: &str,
        options: Vec<String>,
    ) -> Result<ApprovalDecision> {
        self.request(
            "ui.select",
            json!({"title":title,"text":text,"options":options}),
        )
        .await
    }
    pub async fn confirm(&self, title: &str, text: &str) -> Result<ApprovalDecision> {
        self.request("ui.confirm", json!({"title":title,"text":text}))
            .await
    }
    pub async fn input(&self, title: &str, text: &str) -> Result<ApprovalDecision> {
        self.request("ui.input", json!({"title":title,"text":text}))
            .await
    }
    pub async fn panel(&self, panel: PluginPanel) -> Result<ApprovalDecision> {
        self.request("panel.show", json!(panel)).await
    }
    pub async fn status(&self, text: &str) -> Result<()> {
        self.request("status.set", json!({"text":text})).await
    }
}

pub async fn stdio(plugin: impl Plugin, cancel: CancellationToken) -> Result<()> {
    serve(tokio::io::stdin(), tokio::io::stdout(), plugin, cancel).await
}

pub(crate) async fn dispatch(
    plugin: &dyn Plugin,
    declaration: &PluginInitialize,
    method: &str,
    mut params: Value,
    context: Context,
) -> Result<Value> {
    if let Some(object) = params.as_object_mut() {
        object.remove("_meta");
    }
    if !matches!(
        method,
        "tools/list" | "tools/call" | "commands/list" | "commands/run" | "tool.intercept"
    ) {
        return Err(error(-32601, "Unknown host method"));
    }
    let request: PluginRequest = serde_json::from_value(json!({"method":method,"params":params}))
        .map_err(|e| error(-32602, e.to_string()))?;
    let capability = match &request {
        PluginRequest::ToolsList {} | PluginRequest::ToolsCall { .. } => PluginCapability::Tools,
        PluginRequest::CommandsList {} | PluginRequest::CommandsRun { .. } => {
            PluginCapability::Commands
        }
        _ => PluginCapability::Intercept,
    };
    if !declaration.capabilities.contains(&capability) {
        return Err(error(-32001, "Capability not declared"));
    }
    match request {
        PluginRequest::ToolsList {} => Ok(json!(declaration.contributions.tools)),
        PluginRequest::CommandsList {} => Ok(json!(declaration.contributions.commands)),
        PluginRequest::ToolsCall { name, arguments } => {
            if !declaration
                .contributions
                .tools
                .iter()
                .any(|tool| tool.name == name)
            {
                return Err(error(-32602, "Unknown tool"));
            }
            Ok(json!(plugin.tool(&name, arguments, context).await?))
        }
        PluginRequest::CommandsRun { name, arguments } => {
            if !declaration
                .contributions
                .commands
                .iter()
                .any(|command| command.name == name)
            {
                return Err(error(-32602, "Unknown command"));
            }
            Ok(json!(plugin.command(&name, &arguments, context).await?))
        }
        PluginRequest::Intercept(intercept) => {
            let stage = match &intercept {
                PluginIntercept::Before { stage, .. } | PluginIntercept::After { stage, .. } => {
                    stage
                }
            };
            if !declaration.contributions.interceptors.contains(stage) {
                return Err(error(-32602, "Stage not declared"));
            }
            match intercept {
                PluginIntercept::Before { stage, call } => {
                    Ok(json!(plugin.before(stage, call, context).await?))
                }
                PluginIntercept::After {
                    stage,
                    call,
                    output,
                } => Ok(json!(plugin.after(stage, call, output, context).await?)),
            }
        }
        _ => Err(error(-32601, "Unknown host method")),
    }
}
