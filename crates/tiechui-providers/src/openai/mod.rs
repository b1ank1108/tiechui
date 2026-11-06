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

pub(crate) struct Openai {
    transport: Transport,
    compat: CompletionsCompat,
}

impl Openai {
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
                "https://api.openai.com/v1",
                "/chat/completions",
                "Bearer ",
            )?,
            compat: config.compat.clone(),
        })
    }
}

#[async_trait]
impl ModelProvider for Openai {
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
        let model = self.transport.model(&request.model)?;
        let compat = self.compat.with(model.compat.as_ref());
        let body = request::encode(&request, model, &compat)?;
        let outgoing = self
            .transport
            .client
            .post(self.transport.endpoint.clone())
            .header("authorization", self.transport.key.clone())
            .header("accept", "text/event-stream")
            .json(&body);
        let (response, delay) = transport::send(outgoing, &cancel).await?;
        Ok(transport::stream(
            response,
            stream::State::default(),
            delay,
            cancel,
        ))
    }
}

/// Responses-style `call_id|item_id` ids become one sanitized id of at most 40 characters.
fn normalize_id(id: &str) -> String {
    let Some((call, item)) = id.split_once('|') else {
        return id.to_owned();
    };
    let clean = |part: &str| -> String {
        part.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let (call, item) = (clean(call), clean(item));
    let combined = if item.is_empty() {
        call.clone()
    } else {
        format!("{call}_{item}")
    };
    if combined.chars().count() <= 40 {
        return combined;
    }
    let hash: String = short_hash(id).chars().take(8).collect();
    let prefix: String = call.chars().take(40 - hash.len() - 1).collect();
    format!("{prefix}_{hash}")
}

/// Two 32-bit hash lanes over UTF-16 units, each printed in base 36.
fn short_hash(text: &str) -> String {
    let (mut h1, mut h2) = (0xdead_beef_u32, 0x41c6_ce57_u32);
    for unit in text.encode_utf16() {
        let unit = u32::from(unit);
        h1 = (h1 ^ unit).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ unit).wrapping_mul(1_597_334_677);
    }
    h1 = (h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909);
    h2 = (h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507)
        ^ (h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909);
    format!("{}{}", base36(h2), base36(h1))
}

fn base36(mut value: u32) -> String {
    let mut digits = Vec::new();
    loop {
        digits.push(char::from_digit(value % 36, 36).unwrap_or('0'));
        value /= 36;
        if value == 0 {
            break;
        }
    }
    digits.iter().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_ids_are_normalized() {
        let long = format!("call_abc|fc_{}+/=", "x".repeat(60));
        assert_eq!(short_hash(&long), "j9bhsd1kd73hm");
        assert_eq!(normalize_id(&long), "call_abc_j9bhsd1k");
        assert_eq!(normalize_id("call_1|item+2"), "call_1_item_2");
        assert_eq!(normalize_id("call_plain"), "call_plain");
    }
}
