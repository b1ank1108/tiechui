//! Provider adapters own request encoding, streaming decoding, and error classification.

mod anthropic;
mod error;
pub mod estimate;
mod openai;
mod responses;
mod thinking;
mod transport;

#[cfg(feature = "testing")]
pub mod testing;

use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use tiechui_config::{NetworkConfig, ProviderConfig, Wire};
use tiechui_model::{ModelCapabilities, ModelId, ModelRequest, ReasoningLevel, StreamEvent, Usage};
use tokio_util::sync::CancellationToken;

pub use error::{ErrorKind, ProviderError};
pub use thinking::offered;
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
    network: &NetworkConfig,
) -> Result<Box<dyn ModelProvider>, ProviderError> {
    let key = api_key(config)?;
    let client = client(network, client_builder()).map_err(|e| redact_error(network, e))?;
    let provider: Box<dyn ModelProvider> = match config.wire {
        Wire::AnthropicMessages => Box::new(
            anthropic::Anthropic::new(name, config, &key, client)
                .map_err(|e| redact_error(network, e))?,
        ),
        Wire::OpenaiCompletions => Box::new(
            openai::Openai::new(name, config, &key, client)
                .map_err(|e| redact_error(network, e))?,
        ),
        Wire::OpenaiResponses => Box::new(
            responses::Responses::new(name, config, &key, client)
                .map_err(|e| redact_error(network, e))?,
        ),
    };
    if network
        .proxy
        .as_deref()
        .is_some_and(tiechui_config::proxy_has_credentials)
    {
        Ok(Box::new(RedactedProvider {
            inner: provider,
            network: network.clone(),
        }))
    } else {
        Ok(provider)
    }
}

struct RedactedProvider {
    inner: Box<dyn ModelProvider>,
    network: NetworkConfig,
}

#[async_trait]
impl ModelProvider for RedactedProvider {
    fn cost_nano_usd(&self, model: &ModelId, usage: Usage) -> Option<u64> {
        self.inner.cost_nano_usd(model, usage)
    }
    fn capabilities(&self, model: &ModelId) -> ModelCapabilities {
        self.inner.capabilities(model)
    }
    fn models(&self) -> Vec<ModelId> {
        self.inner.models()
    }
    fn thinking_levels(&self, model: &ModelId) -> Vec<Option<ReasoningLevel>> {
        self.inner.thinking_levels(model)
    }
    fn normalize_tool_call_id(&self, id: &str) -> String {
        self.inner.normalize_tool_call_id(id)
    }
    fn error_tool_images(&self) -> bool {
        self.inner.error_tool_images()
    }
    fn prompt_cache_ttl(&self, model: &ModelId) -> Option<std::time::Duration> {
        self.inner.prompt_cache_ttl(model)
    }
    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let network = self.network.clone();
        let stream = self
            .inner
            .stream(request, cancel)
            .await
            .map_err(|e| redact_error(&network, e))?;
        Ok(stream
            .map(move |item| item.map_err(|e| redact_error(&network, e)))
            .boxed())
    }
}

/// The model ids the provider's `/models` endpoint lists, sorted.
pub async fn list_models(
    config: &ProviderConfig,
    network: &NetworkConfig,
    cancel: &CancellationToken,
) -> Result<Vec<String>, ProviderError> {
    let key = api_key(config)?;
    let builder = client_builder().timeout(std::time::Duration::from_secs(30));
    let client = client(network, builder).map_err(|e| redact_error(network, e))?;
    let request = match config.wire {
        Wire::AnthropicMessages => {
            let transport = transport::Transport::new(
                "models",
                config,
                &key,
                client,
                anthropic::ROOT,
                "/models",
                "",
            )
            .map_err(|e| redact_error(network, e))?;
            transport
                .client
                .get(transport.endpoint)
                .header("x-api-key", transport.key)
                .header("anthropic-version", anthropic::VERSION)
                .query(&[("limit", "1000")])
        }
        Wire::OpenaiCompletions | Wire::OpenaiResponses => {
            let transport = transport::Transport::new(
                "models",
                config,
                &key,
                client,
                openai::ROOT,
                "/models",
                "Bearer ",
            )
            .map_err(|e| redact_error(network, e))?;
            transport
                .client
                .get(transport.endpoint)
                .header("authorization", transport.key)
        }
    };
    transport::list(request, cancel)
        .await
        .map_err(|e| redact_error(network, e))
}

fn redact_error(network: &NetworkConfig, mut err: ProviderError) -> ProviderError {
    err.message = network.redact(err.message);
    err
}

fn client(
    network: &NetworkConfig,
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::Client, ProviderError> {
    network
        .apply(builder)
        .map_err(|e| ProviderError::new(ErrorKind::InvalidRequest, e))?
        .build()
        .map_err(ProviderError::transport)
}

fn api_key(config: &ProviderConfig) -> Result<String, ProviderError> {
    config
        .api_key()
        .map_err(|error| ProviderError::new(ErrorKind::Auth, error.to_string()))?
        .ok_or_else(|| ProviderError::new(ErrorKind::Auth, "api_key or api_key_env is required"))
}

#[cfg(test)]
mod tests;
