#![allow(clippy::unwrap_used)]

mod conformance;
mod errors;
mod openai;
mod recovery;
mod requests;
mod responses;
mod streams;
mod usage_caching;

use futures::StreamExt;
use serde_json::{Value, json};
use tiechui_config::{ModelConfig, ProviderConfig, Thinking, Wire};
use tiechui_model::{
    Content, Message, ModelId, ModelRequest, ReasoningLevel, Role, StreamEvent, ToolCall, ToolSpec,
};
use tokio_util::sync::CancellationToken;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

use crate::{ModelProvider, ProviderError, anthropic::Anthropic};

const PLAIN: &str = include_str!("../../tests/fixtures/anthropic/plain_text.sse");
const THINKING: &str = include_str!("../../tests/fixtures/anthropic/thinking_tool.sse");
const PARALLEL: &str = include_str!("../../tests/fixtures/anthropic/parallel_tools.sse");
const ERROR: &str = include_str!("../../tests/fixtures/anthropic/error_midstream.sse");
const TRUNCATED: &str = include_str!("../../tests/fixtures/anthropic/truncated.sse");
const REDACTED: &str = include_str!("../../tests/fixtures/anthropic/redacted_thinking.sse");
const UNKNOWN: &str = include_str!("../../tests/fixtures/anthropic/unknown_blocks.sse");
const FALLBACK: &str = include_str!("../../tests/fixtures/anthropic/fallback_after_content.sse");

fn config(base: &str) -> ProviderConfig {
    ProviderConfig {
        wire: Wire::AnthropicMessages,
        base_url: Some(base.into()),
        api_key: None,
        api_key_env: None,
        compat: Default::default(),
        models: vec![ModelConfig {
            prices: None,
            cache_retention: Default::default(),
            fallback_models: vec![],
            id: "fixture-model".into(),
            context_window: 200_000,
            max_output: 4096,
            reasoning: true,
            thinking: Thinking::Budget,
            thinking_levels: None,
            compat: None,
            images: true,
        }],
    }
}

fn provider(base: &str) -> Anthropic {
    let client = crate::client_builder().no_proxy().build().unwrap();
    Anthropic::new("fixture", &config(base), "fixture-key", client).unwrap()
}

fn text(value: &str) -> Content {
    Content::Text {
        text: value.into(),
        opaque: None,
    }
}

fn message(role: Role, content: Vec<Content>) -> Message {
    Message {
        role,
        content,
        opaque: None,
    }
}

fn call(id: &str, file: &str) -> Content {
    Content::ToolCall(ToolCall {
        id: id.into(),
        name: "read".into(),
        arguments: json!({"path":file}),
        opaque: None,
    })
}

fn request() -> ModelRequest {
    ModelRequest {
        disable_cache: false,
        short_cache: false,
        cache_key: None,
        input_tokens: None,
        model: ModelId("fixture/fixture-model".into()),
        messages: vec![message(Role::User, vec![text("Hello")])],
        tools: vec![],
        max_output: None,
        reasoning: None,
    }
}

fn tool(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("{name} a file"),
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
    }
}

async fn serve(fixture: &str, expected: u64) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "fixture-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(fixture, "text/event-stream"))
        .expect(expected)
        .mount(&server)
        .await;
    server
}

async fn collect(base: &str, request: ModelRequest) -> Vec<Result<StreamEvent, ProviderError>> {
    provider(base)
        .stream(request, CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await
}

async fn captured(request: ModelRequest) -> Value {
    let server = serve(PLAIN, 1).await;
    assert!(
        collect(&server.uri(), request)
            .await
            .iter()
            .all(Result::is_ok)
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    serde_json::from_slice(&requests[0].body).unwrap()
}
