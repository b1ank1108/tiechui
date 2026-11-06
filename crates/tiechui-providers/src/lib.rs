//! Provider adapters own request encoding, streaming decoding, and error classification.

mod anthropic;
mod error;
pub mod estimate;
mod openai;
mod thinking;
mod transport;

#[cfg(feature = "testing")]
pub mod testing;

use async_trait::async_trait;
use futures::stream::BoxStream;
use tiechui_config::{ProviderConfig, Wire};
use tiechui_model::{ModelCapabilities, ModelId, ModelRequest, ReasoningLevel, StreamEvent, Usage};
use tokio_util::sync::CancellationToken;

pub use error::{ErrorKind, ProviderError};
use transport::client_builder;

#[async_trait]
pub trait ModelProvider: Send + Sync {
    /// Price a cumulative usage snapshot using the returned model's configured rates.
    fn cost_nano_usd(&self, _model: &ModelId, _usage: Usage) -> Option<u64> {
        None
    }
    /// Unknown model ids have zero limits and are rejected by `stream`.
    fn capabilities(&self, model: &ModelId) -> ModelCapabilities;

    /// Configured models, by qualified id, in configuration order.
    fn models(&self) -> Vec<ModelId> {
        Vec::new()
    }

    /// Levels the model accepts in ascending order, `None` being off.
    fn thinking_levels(&self, model: &ModelId) -> Vec<Option<ReasoningLevel>> {
        let mut levels = vec![None];
        if self.capabilities(model).reasoning {
            levels.extend(ReasoningLevel::ALL.map(Some));
        }
        levels
    }

    /// The form of another model's tool call id that this wire accepts.
    fn normalize_tool_call_id(&self, id: &str) -> String {
        id.to_owned()
    }

    /// Whether error tool results may contain images directly.
    fn error_tool_images(&self) -> bool {
        true
    }

    /// How long an unused prompt-cache entry for this model stays reusable, when the wire defines it.
    fn prompt_cache_ttl(&self, _model: &ModelId) -> Option<std::time::Duration> {
        None
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError>;
}

/// Model ids are qualified as `<name>/<model id>` using this provider entry's name.
pub fn from_config(
    name: &str,
    config: &ProviderConfig,
) -> Result<Box<dyn ModelProvider>, ProviderError> {
    let key = config
        .api_key()
        .map_err(|error| ProviderError::new(ErrorKind::Auth, error.to_string()))?
        .ok_or_else(|| ProviderError::new(ErrorKind::Auth, "api_key or api_key_env is required"))?;
    let client = client_builder().build().map_err(ProviderError::transport)?;
    match config.wire {
        Wire::AnthropicMessages => Ok(Box::new(anthropic::Anthropic::new(
            name, config, &key, client,
        )?)),
        Wire::OpenaiCompletions => Ok(Box::new(openai::Openai::new(name, config, &key, client)?)),
    }
}

#[cfg(test)]
mod tests;
