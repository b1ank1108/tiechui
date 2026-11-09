pub(crate) mod request;
mod stream;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;
use tiechui_config::{ProviderConfig, Thinking};
use tiechui_model::{ModelCapabilities, ModelId, ModelRequest, ReasoningLevel, StreamEvent};
use tokio_util::sync::CancellationToken;

use crate::transport::{self, Transport};
use crate::{ModelProvider, ProviderError};

pub(crate) const ROOT: &str = "https://api.anthropic.com";
pub(crate) const VERSION: &str = "2023-06-01";

pub(crate) struct Anthropic(Transport);

impl Anthropic {
    pub(crate) fn new(
        name: &str,
        config: &ProviderConfig,
        key: &str,
        client: Client,
    ) -> Result<Self, ProviderError> {
        Transport::new(name, config, key, client, ROOT, "/messages", "").map(Self)
    }
}

#[async_trait]
impl ModelProvider for Anthropic {
    fn cost_nano_usd(&self, model: &ModelId, usage: tiechui_model::Usage) -> Option<u64> {
        self.0.cost_nano_usd(model, usage)
    }
    fn capabilities(&self, model: &ModelId) -> ModelCapabilities {
        self.0.capabilities(model)
    }

    fn models(&self) -> Vec<ModelId> {
        self.0.models()
    }

    fn thinking_levels(&self, model: &ModelId) -> Vec<Option<ReasoningLevel>> {
        self.0.thinking_levels(model)
    }

    /// Anthropic accepts `^[a-zA-Z0-9_-]+$`, at most 64 characters.
    fn error_tool_images(&self) -> bool {
        false
    }

    /// Ephemeral cache entries last five minutes, or one hour with `ttl: 1h`, refreshed on each hit.
    fn prompt_cache_ttl(&self, model: &ModelId) -> Option<std::time::Duration> {
        match self.0.cache_retention(model)? {
            tiechui_config::CacheRetention::None => None,
            tiechui_config::CacheRetention::Short => Some(std::time::Duration::from_secs(300)),
            tiechui_config::CacheRetention::Long => Some(std::time::Duration::from_secs(3600)),
        }
    }

    fn normalize_tool_call_id(&self, id: &str) -> String {
        normalize_id(id)
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        let model = self.0.model(&request.model)?;
        let body = request::encode(&request, model)?;
        let mut outgoing = self
            .0
            .client
            .post(self.0.endpoint.clone())
            .header("x-api-key", self.0.key.clone())
            .header("anthropic-version", VERSION)
            .header("accept", "text/event-stream")
            .json(&body);
        let mut betas = Vec::new();
        if request.reasoning.is_some() && model.thinking == Thinking::Budget {
            betas.push("interleaved-thinking-2025-05-14");
        }
        if !model.fallback_models.is_empty() {
            betas.push("server-side-fallback-2026-07-01");
        }
        if !betas.is_empty() {
            outgoing = outgoing.header("anthropic-beta", betas.join(","));
        }
        let (response, delay) = transport::send(outgoing, &cancel).await?;
        Ok(transport::stream(
            response,
            stream::State::new(request.model),
            delay,
            cancel,
        ))
    }
}

/// At most 64 ASCII letters, digits, `_` and `-`, other characters becoming `_`.
pub(crate) fn normalize_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}
