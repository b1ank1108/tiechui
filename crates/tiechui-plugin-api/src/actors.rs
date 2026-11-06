//! Host-owned child execution; modules supply an isolated registry and consume real events.

use crate::*;

pub const ACTOR_USAGE_EVENT: &str = "actor.usage";

pub struct ChildRequest {
    pub cwd: PathBuf,
    pub task: String,
    pub system: String,
    pub model: Option<String>,
    pub tools: Option<Vec<String>>,
}

#[async_trait]
pub trait ChildRunner: Send + Sync {
    /// Returns only after the actor and its active tools have settled, including cancellation.
    async fn run(
        &self,
        request: ChildRequest,
        registry: Registry,
        cancel: CancellationToken,
        events: Arc<dyn Fn(protocol::Event) + Send + Sync>,
    ) -> Result<(), ToolError>;
}

pub trait ActorClient: Send + Sync {
    /// Bound before a tool batch, with the current model, thinking and provider instances.
    fn bind(&self, runner: Arc<dyn ChildRunner>);
}

pub trait ChildRegistry: Send + Sync {
    fn build(&self, parent: &ToolContext, identity: &str) -> Result<Registry, ToolError>;
}
