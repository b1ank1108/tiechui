pub(crate) mod request;
mod stream;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;
use tiechui_config::{CompletionsCompat, ProviderConfig};
use tiechui_model::{ModelCapabilities, ModelId, ModelRequest, ReasoningLevel, StreamEvent};
use tokio_util::sync::CancellationToken;

use crate::transport::{self, Transport};
use crate::{ModelProvider, ProviderError};

pub(crate) struct Responses {
    transport: Transport,
    compat: CompletionsCompat,
}

impl Responses {
    pub(crate) fn new(
        name: &str,
        config: &ProviderConfig,
        key: &str,
        client: Client,
    ) -> Result<Self, ProviderError> {
        Ok(Self {
            transport: Transport::new(
                name,
                config,
                key,
                client,
                crate::openai::ROOT,
                "/responses",
                "Bearer ",
            )?,
            compat: config.compat.clone(),
        })
    }
}

#[async_trait]
impl ModelProvider for Responses {
    fn cost_nano_usd(&self, model: &ModelId, usage: tiechui_model::Usage) -> Option<u64> {
        self.transport.cost_nano_usd(model, usage)
    }
    fn capabilities(&self, model: &ModelId) -> ModelCapabilities {
        self.transport.capabilities(model)
    }

    fn models(&self) -> Vec<ModelId> {
        self.transport.models()
    }

    fn thinking_levels(&self, model: &ModelId) -> Vec<Option<ReasoningLevel>> {
        self.transport.thinking_levels(model)
    }

    fn normalize_tool_call_id(&self, id: &str) -> String {
        crate::anthropic::normalize_id(id)
    }

    async fn stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        if cancel.is_cancelled() {
            return Err(ProviderError::cancelled());
        }
        let model = self.transport.model(&request.model)?;
        let compat = self.compat.with(model.compat.as_ref());
        let body = request::encode(&request, model, &compat)?;
        let mut outgoing = self
            .transport
            .client
            .post(self.transport.endpoint.clone())
            .header("authorization", self.transport.key.clone())
            .header("accept", "text/event-stream");
        // Sticky routing for the session's cache, as Codex sends it.
        if let Some(key) = &request.cache_key {
            outgoing = outgoing.header("session-id", key);
        }
        let outgoing = outgoing.json(&body);
        let (response, delay) = transport::send(outgoing, &cancel).await?;
        Ok(transport::stream(
            response,
            stream::State::default(),
            delay,
            cancel,
        ))
    }
}
