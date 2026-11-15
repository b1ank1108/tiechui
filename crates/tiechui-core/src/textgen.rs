//! Auxiliary single-shot text generation (session titles and commit messages) with thinking and
//! prompt caching disabled.

use futures::{StreamExt, stream::BoxStream};
use tiechui_model::{Content, Message, ModelId, ModelRequest, Role, StopReason, StreamEvent};
use tiechui_providers::{ErrorKind, ModelProvider, ProviderError};
use tokio_util::sync::CancellationToken;

use super::stream::Response;

pub const TITLE_USER_MAX_BYTES: usize = 2_000;
pub const TITLE_ASSISTANT_MAX_BYTES: usize = 1_500;
pub const TITLE_MAX_CHARS: usize = 80;
pub const COMMIT_DIFF_MAX_BYTES: usize = 32_000;

const TITLE_SYSTEM: &str = "Generate a concise title (at most 6 words, or 20 characters in CJK) \
summarizing the conversation topic in the same language as the user's message. Output only the \
plain title on a single line, with no quotes, markdown, or commentary.";

const COMMIT_SYSTEM: &str = "Write a concise, accurate Git commit message for the provided diff. \
Use an imperative subject line under 72 characters, and add a short blank-line-separated body \
only when context is needed. Output only the raw commit message text, with no markdown code \
fences or commentary.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextGenError {
    Cancelled,
    EmptyResponse,
    Provider(String),
}

impl std::fmt::Display for TextGenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cancelled => f.write_str("request cancelled"),
            Self::EmptyResponse => f.write_str("model returned an empty response"),
            Self::Provider(message) => f.write_str(message),
        }
    }
}

pub fn truncate_utf8(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

fn truncate_snippet(text: &str, max_bytes: usize) -> String {
    let (head, truncated) = truncate_utf8(text.trim(), max_bytes);
    if truncated {
        format!("{head}…")
    } else {
        head.to_owned()
    }
}

fn text_message(role: Role, text: String) -> Message {
    Message {
        role,
        content: vec![Content::Text { text, opaque: None }],
        opaque: None,
    }
}

fn build_request(
    provider: &dyn ModelProvider,
    model: ModelId,
    system: &str,
    user: String,
    desired_max: u64,
) -> ModelRequest {
    let limit = provider.capabilities(&model).max_output;
    let max_output = Some(if limit > 0 {
        desired_max.min(limit)
    } else {
        desired_max
    });
    ModelRequest {
        disable_cache: true,
        short_cache: false,
        temporary_tail: 0,
        cache_key: None,
        model,
        messages: vec![
            text_message(Role::System, system.to_owned()),
            text_message(Role::User, user),
        ],
        input_tokens: None,
        tools: Vec::new(),
        max_output,
        reasoning: None,
    }
}

fn map_provider_error(error: ProviderError) -> TextGenError {
    if error.kind == ErrorKind::Cancelled {
        TextGenError::Cancelled
    } else {
        TextGenError::Provider(error.to_string())
    }
}

async fn collect_stream_text(
    provider: &dyn ModelProvider,
    request: ModelRequest,
    cancel: CancellationToken,
) -> Result<String, TextGenError> {
    if cancel.is_cancelled() {
        return Err(TextGenError::Cancelled);
    }
    let stream = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(TextGenError::Cancelled),
        opened = provider.stream(request, cancel.clone()) => opened.map_err(map_provider_error)?,
    };
    read_stream_events(stream, cancel).await
}

async fn read_stream_events(
    mut stream: BoxStream<'_, Result<StreamEvent, ProviderError>>,
    cancel: CancellationToken,
) -> Result<String, TextGenError> {
    let mut response = Response::default();
    loop {
        let next = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(TextGenError::Cancelled),
            item = stream.next() => item,
        };
        let Some(item) = next else { break };
        let event = item.map_err(map_provider_error)?;
        if let StreamEvent::Finish { reason, .. } = event {
            return match reason {
                StopReason::Cancelled => Err(TextGenError::Cancelled),
                StopReason::Error => Err(TextGenError::Provider("stream ended with error".into())),
                _ => Ok(extract_text(&response)),
            };
        }
        response.apply(event);
    }
    Ok(extract_text(&response))
}

fn extract_text(response: &Response) -> String {
    response
        .message()
        .content
        .into_iter()
        .filter_map(|block| match block {
            Content::Text { text, .. } => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

pub async fn generate_session_title(
    provider: &dyn ModelProvider,
    model: ModelId,
    user_text: &str,
    assistant_text: &str,
    cancel: CancellationToken,
) -> Option<String> {
    if user_text.trim().is_empty() {
        return None;
    }
    let user_part = truncate_snippet(user_text, TITLE_USER_MAX_BYTES);
    let assistant_part = truncate_snippet(assistant_text, TITLE_ASSISTANT_MAX_BYTES);
    let prompt = format!(
        "<user_message>\n{user_part}\n</user_message>\n\n<assistant_reply>\n{assistant_part}\n</assistant_reply>"
    );
    let request = build_request(provider, model, TITLE_SYSTEM, prompt, 128);
    let raw = collect_stream_text(provider, request, cancel).await.ok()?;
    sanitize_title(&raw)
}

fn strip_wrapping_quotes(text: &str) -> &str {
    const PAIRS: [(char, char); 6] = [
        ('"', '"'),
        ('\'', '\''),
        ('`', '`'),
        ('“', '”'),
        ('「', '」'),
        ('《', '》'),
    ];
    for (open, close) in PAIRS {
        if let Some(inner) = text.strip_prefix(open).and_then(|s| s.strip_suffix(close)) {
            return inner.trim();
        }
    }
    text
}

pub fn sanitize_title(raw: &str) -> Option<String> {
    let first = raw.lines().map(str::trim).find(|line| !line.is_empty())?;
    let unheaded = first.trim_start_matches('#').trim();
    let unquoted = strip_wrapping_quotes(unheaded);
    let collapsed = unquoted.split_whitespace().collect::<Vec<_>>().join(" ");
    let capped = match collapsed.char_indices().nth(TITLE_MAX_CHARS) {
        Some((idx, _)) => collapsed[..idx].trim_end(),
        None => collapsed.as_str(),
    };
    (!capped.is_empty()).then(|| capped.to_owned())
}

pub async fn generate_commit_message(
    provider: &dyn ModelProvider,
    model: ModelId,
    diff: &str,
    cancel: CancellationToken,
) -> Result<String, TextGenError> {
    if diff.trim().is_empty() {
        return Err(TextGenError::Provider("No uncommitted changes".into()));
    }
    let (head, truncated) = truncate_utf8(diff, COMMIT_DIFF_MAX_BYTES);
    let prompt = if truncated {
        format!("{head}\n...[diff truncated]")
    } else {
        head.to_owned()
    };
    let request = build_request(provider, model, COMMIT_SYSTEM, prompt, 512);
    let raw = collect_stream_text(provider, request, cancel).await?;
    sanitize_commit_message(&raw).ok_or(TextGenError::EmptyResponse)
}

pub fn sanitize_commit_message(raw: &str) -> Option<String> {
    let normalized = raw.replace("\r\n", "\n");
    let trimmed = normalized.trim();
    let unfenced = if trimmed.starts_with("```") && trimmed.ends_with("```") {
        trimmed
            .split_once('\n')
            .and_then(|(_, rest)| rest.strip_suffix("```"))
            .map_or(trimmed, str::trim)
    } else {
        trimmed
    };
    (!unfenced.is_empty()).then(|| unfenced.to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};
    use tiechui_config::{
        ApiKey, CacheRetention, ModelConfig, NetworkConfig, ProviderConfig, Thinking,
        ThinkingLevel, ThinkingLevelValue, Wire,
    };
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn anthropic_provider(uri: &str) -> Box<dyn ModelProvider> {
        let mut levels = BTreeMap::new();
        levels.insert(ThinkingLevel::Off, ThinkingLevelValue::Wire("off".into()));
        levels.insert(ThinkingLevel::Low, ThinkingLevelValue::Wire("low".into()));
        levels.insert(ThinkingLevel::High, ThinkingLevelValue::Wire("high".into()));
        let config = ProviderConfig {
            wire: Wire::AnthropicMessages,
            base_url: Some(uri.into()),
            api_key: Some(ApiKey("sk-ant".into())),
            api_key_env: None,
            compat: Default::default(),
            models: vec![ModelConfig {
                id: "claude-haiku-4-5".into(),
                cache_retention: CacheRetention::Long,
                prices: None,
                fallback_models: vec![],
                context_window: 200_000,
                max_output: 8_192,
                reasoning: true,
                thinking: Thinking::Budget,
                thinking_levels: Some(levels),
                compat: None,
                images: true,
            }],
        };
        tiechui_providers::from_config("ant", &config, &NetworkConfig::default()).unwrap()
    }

    fn openai_provider(uri: &str) -> Box<dyn ModelProvider> {
        let mut levels = BTreeMap::new();
        levels.insert(ThinkingLevel::Off, ThinkingLevelValue::Wire("none".into()));
        levels.insert(ThinkingLevel::High, ThinkingLevelValue::Wire("high".into()));
        let config = ProviderConfig {
            wire: Wire::OpenaiCompletions,
            base_url: Some(uri.into()),
            api_key: Some(ApiKey("sk-oai".into())),
            api_key_env: None,
            compat: Default::default(),
            models: vec![ModelConfig {
                id: "gpt-5.4-mini".into(),
                cache_retention: CacheRetention::Short,
                prices: None,
                fallback_models: vec![],
                context_window: 200_000,
                max_output: 4_096,
                reasoning: true,
                thinking: Thinking::Adaptive,
                thinking_levels: Some(levels),
                compat: None,
                images: true,
            }],
        };
        tiechui_providers::from_config("oai", &config, &NetworkConfig::default()).unwrap()
    }

    #[tokio::test]
    async fn title_generation_disables_thinking_and_cache_and_truncates_utf8() {
        let mock = MockServer::start().await;
        let sse = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"model\":\"claude-haiku-4-5\",\"usage\":{\"input_tokens\":20,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"  # \\\"修复登录超时🚀\\\" \\n忽略第二行\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":6}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .expect(1)
            .mount(&mock)
            .await;

        let provider = anthropic_provider(&mock.uri());
        let long_user = "修复登录超时🚀".repeat(300);
        let long_assistant = "已经定位到连接池配置问题🔧".repeat(200);

        let title = generate_session_title(
            provider.as_ref(),
            ModelId("ant/claude-haiku-4-5".into()),
            &long_user,
            &long_assistant,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(title.as_deref(), Some("修复登录超时🚀"));

        let requests = mock.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let raw_body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(!raw_body.contains("cache_control"), "{raw_body}");
        if let Some(beta) = requests[0].headers.get("anthropic-beta") {
            assert!(
                !beta.to_str().unwrap().contains("interleaved-thinking"),
                "{beta:?}"
            );
        }
        let body: Value = serde_json::from_str(&raw_body).unwrap();
        assert_eq!(body["model"], "claude-haiku-4-5");
        assert_eq!(body["thinking"], json!({ "type": "disabled" }));
        let prompt_text = body["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert!(
            prompt_text.len() < TITLE_USER_MAX_BYTES + TITLE_ASSISTANT_MAX_BYTES + 256,
            "prompt was not truncated: {} bytes",
            prompt_text.len()
        );
        assert!(prompt_text.contains("修复登录超时🚀"));
        assert!(prompt_text.contains("已经定位到连接池配置问题🔧"));
    }

    #[tokio::test]
    async fn commit_message_truncates_diff_strips_fences_and_cancels_promptly() {
        let mock = MockServer::start().await;
        let sse = format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{
                    "index": 0,
                    "delta": { "content": "```text\nfeat(session): generate titles with lightweight model\n\nTruncate first-turn inputs on UTF-8 boundaries.\n```" },
                    "finish_reason": "stop"
                }]
            })
        );
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
            .expect(1)
            .mount(&mock)
            .await;

        let provider = openai_provider(&mock.uri());
        let huge_diff = "+新增多字节中文与表情行🌿\n".repeat(1_500);
        assert!(huge_diff.len() > COMMIT_DIFF_MAX_BYTES);

        let msg = generate_commit_message(
            provider.as_ref(),
            ModelId("oai/gpt-5.4-mini".into()),
            &huge_diff,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            msg,
            "feat(session): generate titles with lightweight model\n\nTruncate first-turn inputs on UTF-8 boundaries."
        );

        let requests = mock.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let raw_body = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(!raw_body.contains("prompt_cache_key"), "{raw_body}");
        let body: Value = serde_json::from_str(&raw_body).unwrap();
        assert_eq!(body["model"], "gpt-5.4-mini");
        assert_eq!(body["reasoning_effort"], "none");
        let user_content = body["messages"][1]["content"][0]["text"].as_str().unwrap();
        assert!(user_content.len() <= COMMIT_DIFF_MAX_BYTES + 128);
        assert!(user_content.contains("[diff truncated]"));

        struct NotifyOnPost {
            arrived: std::sync::Arc<tokio::sync::Notify>,
        }
        impl wiremock::Respond for NotifyOnPost {
            fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
                self.arrived.notify_one();
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(30))
                    .set_body_raw("data: [DONE]\n\n", "text/event-stream")
            }
        }

        let arrived = std::sync::Arc::new(tokio::sync::Notify::new());
        let slow_mock = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(NotifyOnPost {
                arrived: arrived.clone(),
            })
            .mount(&slow_mock)
            .await;

        let slow_provider = openai_provider(&slow_mock.uri());
        let cancel = CancellationToken::new();
        let cancel_trigger = cancel.clone();
        tokio::spawn(async move {
            arrived.notified().await;
            cancel_trigger.cancel();
        });
        let started = Instant::now();
        let res = generate_commit_message(
            slow_provider.as_ref(),
            ModelId("oai/gpt-5.4-mini".into()),
            "+line\n",
            cancel,
        )
        .await;
        assert_eq!(res, Err(TextGenError::Cancelled));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
