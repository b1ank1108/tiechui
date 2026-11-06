use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

use eventsource_stream::Eventsource;
use futures::{StreamExt, stream::BoxStream};
use reqwest::{Client, RequestBuilder, Response, Url, header::HeaderValue};
use serde_json::Value;
use tiechui_config::{ModelConfig, ProviderConfig};
use tiechui_model::{ModelCapabilities, ModelId, ReasoningLevel, StreamEvent};
use tokio_util::sync::CancellationToken;

use crate::error::{classify, retry_after};
use crate::{ErrorKind, ProviderError};

pub(crate) fn client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(300))
}

pub(crate) struct Transport {
    pub client: Client,
    pub endpoint: Url,
    pub key: HeaderValue,
    models: BTreeMap<String, ModelConfig>,
    order: Vec<String>,
}

impl Transport {
    pub fn cost_nano_usd(&self, model: &ModelId, usage: tiechui_model::Usage) -> Option<u64> {
        let prices = self.model(model).ok()?.prices?;
        let long = usage.cache_write_1h.min(usage.cache_write);
        let amount = prices.input * usage.input as f64
            + prices.output * usage.output as f64
            + prices.cache_read * usage.cache_read as f64
            + prices.cache_write * (usage.cache_write - long) as f64
            + prices.input * 2.0 * long as f64;
        Some((amount * 1000.0).round() as u64)
    }
    pub fn new(
        name: &str,
        config: &ProviderConfig,
        key: &str,
        client: Client,
        default_root: &str,
        suffix: &str,
        key_prefix: &str,
    ) -> Result<Self, ProviderError> {
        let invalid = |message| ProviderError::new(ErrorKind::InvalidRequest, message);
        if name.is_empty() || name.contains('/') {
            return Err(invalid(
                "provider name must be non-empty and contain no slash",
            ));
        }
        let mut endpoint = Url::parse(config.base_url.as_deref().unwrap_or(default_root))
            .map_err(|_| invalid("invalid provider base URL"))?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(invalid(
                "base URL must be HTTP(S) without credentials, query, or fragment",
            ));
        }
        endpoint.set_path(&format!(
            "{}{suffix}",
            endpoint.path().trim_end_matches('/')
        ));
        if key.is_empty() {
            return Err(ProviderError::new(ErrorKind::Auth, "empty API key"));
        }
        let mut key = HeaderValue::from_str(&format!("{key_prefix}{key}"))
            .map_err(|_| ProviderError::new(ErrorKind::Auth, "invalid API key header"))?;
        key.set_sensitive(true);
        let mut models = BTreeMap::new();
        let mut order = Vec::new();
        for model in &config.models {
            if model.prices.is_some_and(|prices| !prices.valid()) {
                return Err(invalid("prices must be finite and non-negative"));
            }
            order.push(format!("{name}/{}", model.id));
            if model.id.is_empty() || model.context_window == 0 || model.max_output == 0 {
                return Err(invalid("model id and positive token limits are required"));
            }
            if models
                .insert(format!("{name}/{}", model.id), model.clone())
                .is_some()
            {
                return Err(invalid("duplicate model id"));
            }
        }
        Ok(Self {
            client,
            endpoint,
            key,
            models,
            order,
        })
    }

    pub fn model(&self, id: &ModelId) -> Result<&ModelConfig, ProviderError> {
        self.models
            .get(&id.0)
            .ok_or_else(|| ProviderError::new(ErrorKind::InvalidRequest, "unknown model id"))
    }

    pub fn models(&self) -> Vec<ModelId> {
        self.order.iter().cloned().map(ModelId).collect()
    }

    pub fn thinking_levels(&self, id: &ModelId) -> Vec<Option<ReasoningLevel>> {
        self.models
            .get(&id.0)
            .map_or_else(|| vec![None], crate::thinking::offered)
    }

    pub fn cache_retention(&self, id: &ModelId) -> Option<tiechui_config::CacheRetention> {
        self.models.get(&id.0).map(|model| model.cache_retention)
    }

    pub fn capabilities(&self, id: &ModelId) -> ModelCapabilities {
        match self.models.get(&id.0) {
            Some(model) => ModelCapabilities {
                context_window: model.context_window,
                max_output: model.max_output,
                reasoning: model.reasoning,
                images: model.images,
            },
            None => ModelCapabilities {
                context_window: 0,
                max_output: 0,
                reasoning: false,
                images: false,
            },
        }
    }
}

async fn error_body(
    response: &mut Response,
    cancel: &CancellationToken,
) -> Result<Value, ProviderError> {
    let mut body = Vec::new();
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::cancelled()),
            chunk = response.chunk() => chunk,
        };
        let Ok(Some(chunk)) = chunk else {
            break;
        };
        let remaining = 64 * 1024 - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if body.len() == 64 * 1024 {
            break;
        }
    }
    Ok(serde_json::from_slice(&body).unwrap_or(Value::Null))
}

pub(crate) async fn send(
    outgoing: RequestBuilder,
    cancel: &CancellationToken,
) -> Result<(Response, Option<Duration>), ProviderError> {
    let mut response = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ProviderError::cancelled()),
        response = outgoing.send() => response.map_err(ProviderError::transport)?,
    };
    let delay = retry_after(response.headers(), SystemTime::now());
    let json_body = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .is_some_and(|mime| mime.trim() == "application/json")
        });
    if !response.status().is_success() || json_body {
        let status = response.status().as_u16();
        let body = error_body(&mut response, cancel).await?;
        return Err(classify(Some(status), &body, delay));
    }
    Ok((response, delay))
}

pub(crate) trait Decoder: Send + 'static {
    fn take_usage(&mut self) -> Option<tiechui_model::Usage>;
    fn accept(
        &mut self,
        name: &str,
        data: &str,
        delay: Option<Duration>,
    ) -> Result<Vec<StreamEvent>, ProviderError>;
    fn eof(&mut self) -> Result<Vec<StreamEvent>, ProviderError>;
    fn finished(&self) -> bool;
}

pub(crate) fn stream(
    response: Response,
    mut state: impl Decoder,
    delay: Option<Duration>,
    cancel: CancellationToken,
) -> BoxStream<'static, Result<StreamEvent, ProviderError>> {
    async_stream::stream! {
        let mut input = Box::pin(response.bytes_stream().eventsource());
        loop {
            let next = tokio::select! {
                biased;
                _ = cancel.cancelled() => { yield Err(ProviderError::cancelled()); return; },
                next = input.next() => next,
            };
            let eof = next.is_none();
            let events = match next {
                Some(Ok(event)) => state.accept(&event.event, &event.data, delay),
                Some(Err(_)) => Err(ProviderError::stream("failed to read SSE stream")),
                None => state.eof(),
            };
            // Usage can accompany a refusal or malformed completed content. Preserve it
            // even when decoding the rest of that event fails.
            if let Some(usage) = state.take_usage() {
                yield Ok(StreamEvent::Usage { usage });
            }
            let events = match events {
                Ok(events) => events,
                Err(error) => { yield Err(error); return; },
            };
            for event in events {
                if cancel.is_cancelled() { yield Err(ProviderError::cancelled()); return; }
                yield Ok(event);
            }
            if eof || state.finished() { return; }
        }
    }
    .boxed()
}
