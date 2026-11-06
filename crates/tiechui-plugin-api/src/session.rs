//! Interactive command access to an actor-owned branch.

use crate::{CancellationToken, ToolError, async_trait, protocol::*};
use std::{path::PathBuf, sync::Arc};

#[derive(Clone)]
pub struct ModelContext {
    pub model: ModelInfo,
    pub thinking: ThinkingLevel,
    pub system: String,
    pub messages: Vec<crate::model::Message>,
    pub models: Vec<ModelInfo>,
}

pub type ModelStream =
    crate::futures::stream::BoxStream<'static, Result<crate::model::StreamEvent, ToolError>>;

#[derive(Debug, Clone)]
pub struct BranchView {
    pub session_id: String,
    pub leaf: String,
    pub cwd: PathBuf,
    pub parent_session: Option<PathBuf>,
    pub entries: Vec<BranchEntry>,
}

#[derive(Debug, Clone)]
pub enum SessionEffect {
    Validate {
        session_id: String,
        leaf: String,
    },
    /// Uses the same navigation path as Op::Branch.
    Navigate {
        entry_id: String,
    },
    Refill {
        content: Vec<InputContent>,
    },
    DetachImages,
    /// Stops session background work and waits for it to settle, e.g. before restoring files.
    Quiesce,
    Custom {
        kind: String,
        data: serde_json::Value,
    },
    /// Append reference material without starting or steering a turn.
    Message {
        kind: String,
        text: String,
        details: serde_json::Value,
    },
    Emit {
        kind: String,
        data: serde_json::Value,
    },
}

#[async_trait]
pub trait CommandHost: Send + Sync {
    fn model_context(&self) -> Result<ModelContext, ToolError> {
        Err(ToolError("Model context is unavailable".into()))
    }
    fn tool_specs(&self) -> Vec<crate::model::ToolSpec> {
        Vec::new()
    }
    /// Dispatches through the host's normal tool policies and scheduler.
    async fn execute_tool(
        &self,
        _scope: &str,
        _call: crate::model::ToolCall,
        _cancel: CancellationToken,
    ) -> Result<crate::tools::ToolOutput, ToolError> {
        Err(ToolError("Command tools are unavailable".into()))
    }
    /// Uses configured credentials; independent of the main turn.
    async fn model_stream(
        &self,
        _request: crate::model::ModelRequest,
        _cancel: CancellationToken,
    ) -> Result<ModelStream, ToolError> {
        Err(ToolError("Model calls are unavailable".into()))
    }
    async fn ask(
        &self,
        request: ApprovalRequest,
        cancel: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError>;
    /// Updates the current command prompt without changing its identity or selection.
    async fn update(&self, request: ApprovalRequest) -> Result<(), ToolError>;
    async fn apply(&self, effect: SessionEffect) -> Result<(), ToolError>;
}

pub struct CommandSession {
    pub branch: BranchView,
    pub host: Arc<dyn CommandHost>,
}
