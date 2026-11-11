//! Host-owned child execution; modules supply an isolated registry and consume real events.

use crate::*;

pub const ACTOR_USAGE_EVENT: &str = "actor.usage";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub model: String,
    pub provider: String,
    pub id: String,
    pub reasoning: bool,
    pub thinking_levels: Vec<protocol::ThinkingLevel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalog {
    pub is_child: bool,
    pub active: String,
    pub models: Vec<CatalogModel>,
}

impl ModelCatalog {
    pub fn candidates(&self) -> impl Iterator<Item = &CatalogModel> {
        self.models
            .iter()
            .filter(|entry| entry.model != self.active)
    }

    pub fn available_label(&self) -> String {
        if self.models.is_empty() {
            "none".into()
        } else {
            self.models
                .iter()
                .map(|entry| entry.model.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    }
}

pub fn split_child_model<'a>(
    model: &'a str,
    catalog: &ModelCatalog,
) -> Result<(&'a str, &'a str), ToolError> {
    model
        .split_once('/')
        .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())
        .ok_or_else(|| {
            ToolError(format!(
                "Invalid model '{model}': expected <provider>/<id>. Available models: {}",
                catalog.available_label()
            ))
        })
}

pub fn unknown_child_model_error(model: &str, catalog: &ModelCatalog) -> ToolError {
    ToolError(format!(
        "Unknown or unavailable model '{model}'. Available models: {}",
        catalog.available_label()
    ))
}

pub struct ChildRequest {
    pub cwd: PathBuf,
    pub task: String,
    pub system: String,
    pub model: Option<String>,
    pub thinking: Option<ChildThinking>,
    pub tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildThinking {
    Inherit,
    Level(protocol::ThinkingLevel),
}

impl ChildThinking {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::Level(protocol::ThinkingLevel::Off) => "off",
            Self::Level(protocol::ThinkingLevel::Minimal) => "minimal",
            Self::Level(protocol::ThinkingLevel::Low) => "low",
            Self::Level(protocol::ThinkingLevel::Medium) => "medium",
            Self::Level(protocol::ThinkingLevel::High) => "high",
            Self::Level(protocol::ThinkingLevel::Xhigh) => "xhigh",
            Self::Level(protocol::ThinkingLevel::Max) => "max",
        }
    }
}

pub fn parse_child_thinking(value: &str) -> Result<ChildThinking, ToolError> {
    if value == "inherit" {
        return Ok(ChildThinking::Inherit);
    }
    serde_json::from_value(serde_json::Value::String(value.into()))
        .map(ChildThinking::Level)
        .map_err(|_| {
            ToolError(format!(
                "Invalid thinking level '{value}': expected off, minimal, low, medium, high, xhigh, max, or inherit"
            ))
        })
}

#[async_trait]
pub trait ChildRunner: Send + Sync {
    fn model_catalog(&self) -> ModelCatalog;

    fn validate_model(&self, model: &str) -> Result<(), ToolError>;

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
    /// Bound with the session's active model, requested thinking, and ready provider catalog.
    fn bind(&self, runner: Arc<dyn ChildRunner>);
}

pub trait ChildRegistry: Send + Sync {
    fn build(&self, parent: &ToolContext, identity: &str) -> Result<Registry, ToolError>;

    /// Builds an unnamed default child registry without loading disk agent definitions.
    fn build_default(&self, parent: &ToolContext) -> Result<Registry, ToolError>;
}
