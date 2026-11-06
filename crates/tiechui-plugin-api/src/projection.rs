//! Request-only views of durable branch messages, before provider replay adaptation.

use crate::{model::Message, *};

#[derive(Clone)]
pub struct ProjectionContext {
    pub branch: BranchView,
    pub history: Vec<Message>,
    pub tool_details: std::collections::BTreeMap<String, serde_json::Value>,
    pub messages: Vec<Message>,
    pub tools: Vec<model::ToolSpec>,
    pub system: String,
    pub window: u64,
    /// Shared host estimate; contributors must not substitute their core tokenizer for request accounting.
    pub estimate_message: fn(&Message) -> u64,
    /// Shared host estimate of the tool definitions sent with the request.
    pub estimate_tools: fn(&[model::ToolSpec]) -> u64,
    /// The provider's prompt cache for this model has outlived its declared retention since its
    /// last use. False when the retention, the last use or the model continuity is unknown.
    pub cache_cold: bool,
}

#[derive(Debug)]
pub struct ProjectedMessage {
    /// The input message whose replay origin and stop reason must be retained.
    pub source: Option<usize>,
    pub message: Message,
}

#[derive(Debug)]
pub struct ProjectedRequest {
    pub messages: Vec<ProjectedMessage>,
    /// Custom entries persisted on the branch before the view is sent; if that fails the view is discarded.
    pub entries: Vec<(String, serde_json::Value)>,
}

#[async_trait]
pub trait RequestProjection: Send + Sync {
    fn fold(&self, _kind: &str, _data: &serde_json::Value) -> Option<ContextFold> {
        None
    }

    /// None leaves the request byte-identical. Errors fall back to the input view.
    async fn project(
        &self,
        context: ProjectionContext,
        cancel: CancellationToken,
    ) -> Result<Option<ProjectedRequest>, ToolError>;
}
