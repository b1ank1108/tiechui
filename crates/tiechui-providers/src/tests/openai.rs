use tiechui_config::{
    CompatOverride, CompletionsCompat, MaxTokensField, ThinkingFormat, ThinkingLevel as L,
    ThinkingLevelValue as V, UnsupportedLevel,
};
use tiechui_model::{OpaqueData, StopReason, ToolResult, Usage};

use super::streams::usage;
use super::*;
use crate::{ErrorKind, openai::Openai};

pub(super) const PLAIN: &str =
    include_str!("../../tests/fixtures/openai-completions/plain_text.sse");
const THINKING: &str = include_str!("../../tests/fixtures/openai-completions/thinking_tool.sse");
const PARALLEL: &str = include_str!("../../tests/fixtures/openai-completions/parallel_tools.sse");
const TRUNCATED: &str = include_str!("../../tests/fixtures/openai-completions/truncated.sse");
const ERROR: &str = include_str!("../../tests/fixtures/openai-completions/error_200.json");
const MISSING_INDEX: &str =
    include_str!("../../tests/fixtures/openai-completions/missing_index.sse");
const SHARED_INDEX: &str = include_str!("../../tests/fixtures/openai-completions/shared_index.sse");

pub(super) fn provider(base: &str) -> Openai {
    Openai::new(
        "fixture",
        &config(base),
        "fixture-key",
        crate::client_builder().no_proxy().build().unwrap(),
    )
    .unwrap()
}

async fn collect(fixture: &str) -> Vec<Result<StreamEvent, ProviderError>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/proxy/v1/chat/completions"))
        .and(header("authorization", "Bearer fixture-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(fixture, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    provider(&format!("{}/proxy/v1/", server.uri()))
        .stream(request(), CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await
}

fn missing_usage(reason: StopReason) -> StreamEvent {
    StreamEvent::Finish {
        reason,
        usage: Usage {
            partial: true,
            ..Usage::default()
        },
    }
}

fn finish(
    reason: StopReason,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
) -> StreamEvent {
    StreamEvent::Finish {
        reason,
        usage: Usage {
            input,
            output,
            cache_read,
            cache_write,
            ..Default::default()
        },
    }
}

fn complete(index: u32, content: Content) -> StreamEvent {
    StreamEvent::ContentComplete { index, content }
}
fn start(index: u32, id: &str) -> StreamEvent {
    StreamEvent::ToolCallStart {
        index,
        id: id.into(),
        name: "read".into(),
    }
}
fn args(index: u32, delta: &str) -> StreamEvent {
    StreamEvent::ToolCallArgumentsDelta {
        index,
        delta: delta.into(),
    }
}

#[tokio::test]
async fn text_usage_only_chunk_and_missing_done_have_exact_events() {
    let expected = vec![
        Ok(StreamEvent::TextDelta {
            index: 0,
            delta: "Hello".into(),
        }),
        Ok(StreamEvent::TextDelta {
            index: 0,
            delta: "!".into(),
        }),
        Ok(usage(7, 2, 11, 5)),
        Ok(complete(0, text("Hello!"))),
        Ok(finish(StopReason::EndTurn, 7, 2, 11, 5)),
    ];
    assert_eq!(collect(PLAIN).await, expected);
    assert_eq!(
        collect(&PLAIN.replace("data: [DONE]\n", "")).await,
        expected
    );
}

#[tokio::test]
async fn null_errors_and_empty_finish_reasons_are_absent() {
    let expected = collect(PLAIN).await;
    let fixture = PLAIN.replace("\"choices\"", "\"error\":null,\"choices\"");
    assert_eq!(collect(&fixture).await, expected);
    let fixture = fixture
        .replace("\"finish_reason\":null", "\"finish_reason\":\"\"")
        .replace(
            "data: [DONE]",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"\"}]}\n\ndata: [DONE]",
        );
    assert_eq!(collect(&fixture).await, expected);
    let unfinished = TRUNCATED.replace("\"delta\":", "\"finish_reason\":\"\",\"delta\":");
    assert_eq!(
        collect(&unfinished).await.last(),
        Some(&Err(ProviderError::stream(
            "stream ended without finish_reason"
        )))
    );
}

#[tokio::test]
async fn missing_indices_use_id_lookup_and_can_bind_an_index_later() {
    assert_eq!(
        collect(MISSING_INDEX).await,
        vec![
            Ok(start(0, "call_a")),
            Ok(args(0, "{\"path\":")),
            Ok(start(1, "call_b")),
            Ok(args(1, "{\"path\":")),
            Ok(args(1, "\"b\"")),
            Ok(args(0, "\"a\"}")),
            Ok(args(1, "}")),
            Ok(complete(0, call("call_a", "a"))),
            Ok(complete(1, call("call_b", "b"))),
            Ok(missing_usage(StopReason::ToolUse)),
        ]
    );
}

#[tokio::test]
async fn two_call_ids_on_index_zero_alias_the_first_block() {
    assert_eq!(
        collect(SHARED_INDEX).await,
        vec![
            Ok(start(0, "call_a")),
            Ok(args(0, "{\"path\":")),
            Ok(args(0, "\"a\"")),
            Ok(args(0, "}")),
            Ok(complete(0, call("call_a", "a"))),
            Ok(missing_usage(StopReason::ToolUse)),
        ]
    );
}

#[tokio::test]
async fn mixed_gateway_models_override_single_compat_fields_on_each_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(PLAIN, "text/event-stream"))
        .expect(3)
        .mount(&server)
        .await;
    let mut config = config(&server.uri());
    config.wire = Wire::OpenaiCompletions;
    config.compat = CompletionsCompat {
        thinking_format: ThinkingFormat::Deepseek,
        supports_developer_role: false,
        supports_store: false,
        supports_strict_mode: false,
        supports_usage_in_streaming: false,
        max_tokens_field: MaxTokensField::MaxTokens,
        ..Default::default()
    };
    config.models[0].id = "deepseek-v4.1-flash".into();
    let mut gemini = config.models[0].clone();
    gemini.id = "gemini-3.8-flash".into();
    gemini.compat = Some(CompatOverride {
        thinking_format: Some(ThinkingFormat::Openai),
        supports_developer_role: Some(true),
        supports_strict_mode: Some(true),
        ..Default::default()
    });
    config.models.push(gemini);
    let provider = Openai::new(
        "fixture",
        &config,
        "fixture-key",
        crate::client_builder().no_proxy().build().unwrap(),
    )
    .unwrap();
    for id in [
        "deepseek-v4.1-flash",
        "gemini-3.8-flash",
        "deepseek-v4.1-flash",
    ] {
        let mut request = request();
        request.model = ModelId(format!("fixture/{id}"));
        request.reasoning = Some(ReasoningLevel::High);
        request
            .messages
            .insert(0, message(Role::System, vec![text("Instructions")]));
        request.tools = vec![tool("read")];
        let events: Vec<_> = provider
            .stream(request, CancellationToken::new())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok));
    }
    let requests = server.received_requests().await.unwrap();
    let bodies: Vec<Value> = requests
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(bodies[0], bodies[2]);
    let deepseek = &bodies[0];
    assert_eq!(deepseek["model"], "deepseek-v4.1-flash");
    assert_eq!(deepseek["messages"][0]["role"], "system");
    assert_eq!(deepseek["thinking"], json!({"type":"enabled"}));
    assert_eq!(deepseek["reasoning_effort"], "high");
    assert_eq!(deepseek["max_tokens"], 4096);
    assert!(deepseek.get("store").is_none());
    assert!(deepseek.get("stream_options").is_none());
    assert!(deepseek["tools"][0]["function"].get("strict").is_none());
    let gemini = &bodies[1];
    assert_eq!(gemini["model"], "gemini-3.8-flash");
    assert_eq!(gemini["messages"][0]["role"], "developer");
    assert!(gemini.get("thinking").is_none());
    assert_eq!(gemini["reasoning_effort"], "high");
    assert_eq!(gemini["tools"][0]["function"]["strict"], false);
    // Fields the model omits keep the provider's values.
    assert_eq!(gemini["max_tokens"], 4096);
    assert!(gemini.get("store").is_none());
    assert!(gemini.get("stream_options").is_none());
}

#[tokio::test]
async fn reasoning_and_tool_call_are_reassembled_and_replay_later() {
    let events = collect(THINKING).await;
    let thinking = Content::Thinking {
        text: "Check.".into(),
        redacted: false,
        opaque: Some(OpaqueData("reasoning_content".into())),
    };
    assert_eq!(
        events,
        vec![
            Ok(StreamEvent::ThinkingDelta {
                index: 0,
                delta: "Check.".into()
            }),
            Ok(start(1, "call_read")),
            Ok(args(1, "{\"path\":")),
            Ok(usage(9, 8, 3, 0)),
            Ok(args(1, "\"sample.txt\"}")),
            Ok(complete(0, thinking.clone())),
            Ok(complete(1, call("call_read", "sample.txt"))),
            Ok(finish(StopReason::ToolUse, 9, 8, 3, 0)),
        ]
    );
    let mut request = request();
    request.messages.extend([
        message(
            Role::Assistant,
            vec![thinking, call("call_read", "sample.txt")],
        ),
        message(
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call_id: "call_read".into(),
                content: vec![text("contents")],
                is_error: false,
            })],
        ),
        message(Role::Assistant, vec![text("Read it.")]),
        message(Role::User, vec![text("Continue")]),
    ]);
    request.tools = vec![tool("read")];
    let mut config = config("http://fixture.invalid");
    config
        .compat
        .requires_reasoning_content_on_assistant_messages = true;
    let body = crate::openai::request::encode(&request, &config.models[0], &config.compat).unwrap();
    assert_eq!(body["messages"][1]["reasoning_content"], "Check.");
    assert_eq!(body["messages"][3]["reasoning_content"], "");
    insta::assert_json_snapshot!("openai_reasoning_tool_replay", body);
}

#[tokio::test]
async fn parallel_tools_keep_arrival_indices_and_interleaved_arguments() {
    assert_eq!(
        collect(PARALLEL).await,
        vec![
            Ok(start(0, "call_a")),
            Ok(args(0, "{\"path\":")),
            Ok(start(1, "call_b")),
            Ok(args(1, "{\"path\":")),
            Ok(args(1, "\"b\"}")),
            Ok(args(0, "\"a\"}")),
            Ok(usage(5, 4, 0, 0)),
            Ok(complete(0, call("call_a", "a"))),
            Ok(complete(1, call("call_b", "b"))),
            Ok(finish(StopReason::ToolUse, 5, 4, 0, 0)),
        ]
    );
}

#[tokio::test]
async fn http_200_error_bodies_and_stream_envelopes_are_classified() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("retry-after-ms", "123")
                .set_body_raw(ERROR, "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let error = provider(&server.uri())
        .stream(request(), CancellationToken::new())
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind, ErrorKind::ContextOverflow);
    assert_eq!(error.message, "sanitized context overflow");
    assert_eq!(
        error.retry_after,
        Some(std::time::Duration::from_millis(123))
    );
    let events = collect(&format!("data: {}\n\n", ERROR.trim())).await;
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].as_ref().unwrap_err().kind,
        ErrorKind::ContextOverflow
    );
}

#[tokio::test]
async fn truncated_and_invalid_streams_never_finish() {
    for fixture in [
        TRUNCATED.to_owned(),
        format!("{TRUNCATED}data: [DONE]\n\n"),
        "data: {\n\n".into(),
        THINKING.replace("sample.txt", "sample.txt\\\""),
        PARALLEL.replace("call_b", "call_a"),
        PARALLEL.replace(
            "\"finish_reason\":\"tool_calls\"",
            "\"finish_reason\":\"unknown\"",
        ),
    ] {
        let events = collect(&fixture).await;
        assert!(events.last().unwrap().is_err(), "{fixture}");
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::Finish { .. })))
        );
    }
}

#[tokio::test]
async fn first_turn_request_is_captured_with_api_root_and_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gateway/v1/chat/completions"))
        .and(header("authorization", "Bearer fixture-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(PLAIN, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let mut request = request();
    request
        .messages
        .insert(0, message(Role::System, vec![text("Be concise.")]));
    request.tools = vec![tool("read")];
    request.reasoning = Some(ReasoningLevel::High);
    let events: Vec<_> = provider(&format!("{}/gateway", server.uri()))
        .stream(request, CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    insta::assert_json_snapshot!("openai_first_turn", body);
}

#[tokio::test]
async fn base_urls_gain_the_api_version_only_without_one() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(PLAIN, "text/event-stream"))
        .mount(&server)
        .await;
    for (root, expected) in [
        ("", "/v1/chat/completions"),
        ("/v1/", "/v1/chat/completions"),
        ("/api/paas/v4", "/api/paas/v4/chat/completions"),
        ("/v1beta/openai", "/v1beta/openai/chat/completions"),
        ("/proxy", "/proxy/v1/chat/completions"),
    ] {
        let events: Vec<_> = provider(&format!("{}{root}", server.uri()))
            .stream(request(), CancellationToken::new())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok), "{root}");
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.last().unwrap().url.path(), expected, "{root}");
    }
    // Anthropic roots follow the same rule.
    let anthropic = serve(super::PLAIN, 1).await;
    assert!(
        super::collect(&format!("{}/v1", anthropic.uri()), request())
            .await
            .iter()
            .all(Result::is_ok)
    );
}

#[tokio::test]
async fn models_are_listed_from_the_versioned_root() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer fixture-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": [{"id": "b"}, {"id": "a"}, {"object": "model"}]})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("x-api-key", "fixture-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "c"}]})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/denied/v1/models"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"error": {"message": "bad key"}})),
        )
        .mount(&server)
        .await;
    let list = |wire, root: &str| {
        let config = ProviderConfig {
            wire,
            api_key: Some(tiechui_config::ApiKey("fixture-key".into())),
            models: vec![],
            ..super::config(&format!("{}{root}", server.uri()))
        };
        async move { crate::list_models(&config, &Default::default(), &CancellationToken::new()).await }
    };
    assert_eq!(list(Wire::OpenaiCompletions, "").await.unwrap(), ["a", "b"]);
    assert_eq!(list(Wire::OpenaiResponses, "").await.unwrap(), ["a", "b"]);
    assert_eq!(list(Wire::AnthropicMessages, "/v1").await.unwrap(), ["c"]);
    let error = list(Wire::OpenaiCompletions, "/denied").await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::Auth);
}

#[tokio::test]
async fn requests_go_through_the_configured_proxy() {
    let proxy = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "a"}]})))
        .mount(&proxy)
        .await;
    let config = ProviderConfig {
        wire: Wire::OpenaiCompletions,
        api_key: Some(tiechui_config::ApiKey("fixture-key".into())),
        models: vec![],
        ..super::config("http://127.0.0.1:1")
    };
    let mut network = tiechui_config::NetworkConfig {
        proxy: Some(proxy.uri()),
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    assert_eq!(
        crate::list_models(&config, &network, &cancel)
            .await
            .unwrap(),
        ["a"]
    );
    // Hosts in no_proxy are reached directly, and this one refuses connections.
    network.no_proxy = Some("127.0.0.1".into());
    let error = crate::list_models(&config, &network, &cancel)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ErrorKind::TransientNetwork);
    assert_eq!(proxy.received_requests().await.unwrap().len(), 1);
}

async fn assert_proxy_forwards_models(config: &ProviderConfig, proxy_url: String) {
    let network = tiechui_config::NetworkConfig {
        proxy: Some(proxy_url),
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    assert_eq!(
        crate::list_models(config, &network, &cancel).await.unwrap(),
        ["m1"]
    );
}

async fn assert_proxy_fails_closed(
    config: &ProviderConfig,
    upstream: &MockServer,
    proxy_url: String,
    secret: &str,
) {
    let before = upstream.received_requests().await.unwrap().len();
    let network = tiechui_config::NetworkConfig {
        proxy: Some(proxy_url),
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    let err = crate::list_models(config, &network, &cancel)
        .await
        .unwrap_err();
    assert!(!err.to_string().contains(secret), "{err}");
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        before,
        "failed proxy request must fail closed and never fall back to direct"
    );
}

async fn start_models_upstream() -> (MockServer, ProviderConfig) {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "m1"}]})))
        .mount(&upstream)
        .await;
    let config = ProviderConfig {
        wire: Wire::OpenaiCompletions,
        api_key: Some(tiechui_config::ApiKey("fixture-key".into())),
        models: vec![],
        ..super::config(&upstream.uri())
    };
    (upstream, config)
}

#[tokio::test]
async fn socks5_and_http_proxies_forward_fail_closed_and_redact_credentials() {
    use tiechui_config::test_support::Socks5TestProxy;

    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let (upstream, config) = start_models_upstream().await;
        let mut socks_noauth = Socks5TestProxy::start(None);
        assert_proxy_forwards_models(&config, format!("socks5://{}", socks_noauth.addr())).await;
        assert_eq!(socks_noauth.hits(), 1);

        let mut socks_auth = Socks5TestProxy::start(Some(("alice", "s3cr3t-pass")));
        let auth_addr = socks_auth.addr();
        assert_proxy_forwards_models(&config, format!("socks5://alice:s3cr3t-pass@{auth_addr}"))
            .await;
        assert_eq!(socks_auth.hits(), 1);

        let wrong_url = format!("socks5://alice:wrong-pass@{auth_addr}");
        assert_proxy_fails_closed(&config, &upstream, wrong_url, "wrong-pass").await;

        socks_noauth.shutdown();
        socks_auth.shutdown();
        let closed_url = format!("socks5://alice:s3cr3t-pass@{auth_addr}");
        assert_proxy_fails_closed(&config, &upstream, closed_url, "s3cr3t-pass").await;
        assert_http_407_and_bypass(&config, &upstream, auth_addr).await;
    })
    .await
    .expect("socks5/http proxy test timed out");
}

async fn assert_http_407_and_bypass(
    config: &ProviderConfig,
    upstream: &MockServer,
    closed_addr: std::net::SocketAddr,
) {
    let proxy_407 = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(407))
        .mount(&proxy_407)
        .await;
    assert_proxy_fails_closed(
        config,
        upstream,
        format!("http://alice:s3cr3t-pass@{}", proxy_407.address()),
        "s3cr3t-pass",
    )
    .await;

    let before = upstream.received_requests().await.unwrap().len();
    let bypassed = tiechui_config::NetworkConfig {
        proxy: Some(format!("socks5://alice:s3cr3t-pass@{closed_addr}")),
        no_proxy: Some("127.0.0.1".into()),
        ca_cert: None,
    };
    let cancel = CancellationToken::new();
    assert_eq!(
        crate::list_models(config, &bypassed, &cancel)
            .await
            .unwrap(),
        ["m1"]
    );
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        before + 1
    );
}

#[test]
fn tool_results_and_images_request() {
    let mut request = request();
    let image = Content::Image {
        mime_type: "image/png".into(),
        data: "Zml4dHVyZQ==".into(),
    };
    request.messages[0].content.push(image.clone());
    request.messages.extend([
        message(
            Role::Assistant,
            vec![text("Reading"), call("a", "a.txt"), call("b", "b.txt")],
        ),
        message(
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call_id: "a".into(),
                content: vec![image],
                is_error: true,
            })],
        ),
        message(
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call_id: "b".into(),
                content: vec![],
                is_error: false,
            })],
        ),
        message(Role::User, vec![text("Continue")]),
    ]);
    let config = config("http://fixture.invalid");
    let body = crate::openai::request::encode(&request, &config.models[0], &config.compat).unwrap();
    assert_eq!(body["tools"], json!([]));
    assert_eq!(body["messages"][2]["content"], "(see attached image)");
    assert_eq!(body["messages"][3]["content"], "(no tool output)");
    assert_eq!(body["messages"][4]["role"], "user");
    insta::assert_json_snapshot!("openai_tool_results_images", body);
}

#[tokio::test]
async fn reasoning_aliases_aggregate_before_text_and_replay_with_the_original_field() {
    for field in ["reasoning_content", "reasoning", "reasoning_text"] {
        let fixture = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"{field}\":\"Think\",\"content\":\"Answer\",\"tool_calls\":null}},\"finish_reason\":\"end\"}}]}}\n\ndata: [DONE]\n\n"
        );
        let thinking = Content::Thinking {
            text: "Think".into(),
            redacted: false,
            opaque: Some(OpaqueData(field.into())),
        };
        assert_eq!(
            collect(&fixture).await,
            vec![
                Ok(StreamEvent::ThinkingDelta {
                    index: 0,
                    delta: "Think".into()
                }),
                Ok(StreamEvent::TextDelta {
                    index: 1,
                    delta: "Answer".into()
                }),
                Ok(complete(0, thinking.clone())),
                Ok(complete(1, text("Answer"))),
                Ok(missing_usage(StopReason::EndTurn)),
            ]
        );
        let mut request = request();
        request
            .messages
            .push(message(Role::Assistant, vec![thinking, text("Answer")]));
        let config = config("http://fixture.invalid");
        assert_eq!(
            crate::openai::request::encode(&request, &config.models[0], &config.compat).unwrap()["messages"]
                [1][field],
            "Think"
        );
    }
}

#[tokio::test]
async fn choice_usage_fallback_cache_aliases_and_length_stop() {
    for usage in [
        json!({"prompt_tokens":2,"completion_tokens":5,"cached_tokens":3}),
        json!({"prompt_tokens":2,"completion_tokens":5,"prompt_cache_hit_tokens":3,"cached_tokens":9}),
        json!({"prompt_tokens":2,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":3},"prompt_cache_hit_tokens":9}),
    ] {
        let fixture = format!(
            "data: {{\"choices\":[{{\"delta\":null,\"finish_reason\":\"length\",\"usage\":{usage}}}]}}\n\n"
        );
        assert_eq!(
            collect(&fixture).await,
            vec![
                Ok(super::streams::usage(0, 5, 3, 0)),
                Ok(finish(StopReason::Length, 0, 5, 3, 0))
            ]
        );
        let fixture = format!(
            "data: {{\"choices\":[{{\"finish_reason\":\"stop\",\"usage\":{usage}}}],\"usage\":{{\"prompt_tokens\":7,\"completion_tokens\":1}}}}\n\n"
        );
        assert_eq!(
            collect(&fixture).await,
            vec![
                Ok(super::streams::usage(7, 1, 0, 0)),
                Ok(finish(StopReason::EndTurn, 7, 1, 0, 0))
            ]
        );
    }
}

#[tokio::test]
async fn indexed_arguments_wait_for_late_identity_without_losing_fragments() {
    let fixture = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":9,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\ndata: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":9,\"id\":\"late\",\"function\":{\"name\":\"read\",\"arguments\":\"\\\"a\\\"}\"}}]},\"finish_reason\":\"function_call\"}]}\n\n";
    assert_eq!(
        collect(fixture).await,
        vec![
            Ok(start(0, "late")),
            Ok(args(0, "{\"path\":")),
            Ok(args(0, "\"a\"}")),
            Ok(complete(0, call("late", "a"))),
            Ok(missing_usage(StopReason::ToolUse))
        ]
    );
}

#[tokio::test]
async fn invalid_requests_fail_before_http_and_non_reasoning_models_reject_levels() {
    let server = MockServer::start().await;
    let provider = provider(&server.uri());
    let mut cases = Vec::new();
    let mut value = request();
    value.model = ModelId("missing/model".into());
    cases.push(value);
    let mut value = request();
    value.max_output = Some(0);
    cases.push(value);
    let mut value = request();
    value.max_output = Some(4097);
    cases.push(value);
    let mut value = request();
    value.messages.clear();
    cases.push(value);
    let mut value = request();
    value
        .messages
        .push(message(Role::Assistant, vec![call("pending", "a")]));
    cases.push(value);
    let mut value = request();
    value.messages.push(message(
        Role::Tool,
        vec![Content::ToolResult(ToolResult {
            call_id: "orphan".into(),
            content: vec![],
            is_error: false,
        })],
    ));
    cases.push(value);
    let mut value = request();
    value.tools = vec![tool("read"), tool("read")];
    cases.push(value);
    for request in cases {
        assert_eq!(
            provider
                .stream(request, CancellationToken::new())
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::InvalidRequest
        );
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    let mut config = config(&server.uri());
    config.models[0].reasoning = false;
    let mut request = request();
    request.reasoning = Some(ReasoningLevel::Low);
    assert_eq!(
        crate::openai::request::encode(&request, &config.models[0], &config.compat)
            .unwrap_err()
            .kind,
        ErrorKind::InvalidRequest
    );
}

#[test]
fn compat_fields_control_only_their_wire_fields() {
    let mut config = config("http://fixture.invalid");
    let mut request = request();
    request.tools = vec![tool("read")];
    request.reasoning = Some(ReasoningLevel::High);
    request
        .messages
        .insert(0, message(Role::System, vec![text("Instructions")]));
    config.compat = CompletionsCompat {
        supports_store: false,
        supports_developer_role: false,
        supports_strict_mode: true,
        supports_reasoning_effort: false,
        supports_usage_in_streaming: false,
        max_tokens_field: MaxTokensField::MaxTokens,
        thinking_format: ThinkingFormat::Deepseek,
        requires_reasoning_content_on_assistant_messages: true,
    };
    let body = crate::openai::request::encode(&request, &config.models[0], &config.compat).unwrap();
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["tools"][0]["function"]["strict"], false);
    assert_eq!(body["max_tokens"], 4096);
    assert_eq!(body["thinking"], json!({"type":"enabled"}));
    for field in [
        "store",
        "reasoning_effort",
        "stream_options",
        "max_completion_tokens",
    ] {
        assert!(body.get(field).is_none());
    }
    request.reasoning = None;
    assert_eq!(
        crate::openai::request::encode(&request, &config.models[0], &config.compat).unwrap()["thinking"],
        json!({"type":"disabled"})
    );
}

#[test]
fn both_formats_use_per_model_level_maps_and_off_semantics() {
    for format in [ThinkingFormat::Openai, ThinkingFormat::Deepseek] {
        for (id, unsupported) in [
            ("claude-opus-5-5", vec![L::Minimal]),
            ("deepseek-v4.1-flash", vec![L::Minimal, L::Medium, L::Xhigh]),
        ] {
            let mut config = config("http://fixture.invalid");
            config.compat.thinking_format = format;
            let model = &mut config.models[0];
            model.id = id.into();
            let mut map =
                std::collections::BTreeMap::from([(L::Off, V::Unsupported(UnsupportedLevel))]);
            for (key, value) in [
                (L::Low, "low"),
                (L::Medium, "medium"),
                (L::High, "high"),
                (L::Xhigh, "xhigh"),
                (L::Max, "max"),
            ] {
                map.insert(key, V::Wire(value.into()));
            }
            for key in &unsupported {
                map.insert(*key, V::Unsupported(UnsupportedLevel));
            }
            model.thinking_levels = Some(map);
            let off = crate::openai::request::encode(&request(), model, &config.compat).unwrap();
            assert!(off.get("thinking").is_none());
            assert!(off.get("reasoning_effort").is_none());
            for (key, level, wire) in [
                (L::Minimal, ReasoningLevel::Minimal, "minimal"),
                (L::Low, ReasoningLevel::Low, "low"),
                (L::Medium, ReasoningLevel::Medium, "medium"),
                (L::High, ReasoningLevel::High, "high"),
                (L::Xhigh, ReasoningLevel::Xhigh, "xhigh"),
                (L::Max, ReasoningLevel::Max, "max"),
            ] {
                let mut request = request();
                request.reasoning = Some(level);
                let result = crate::openai::request::encode(&request, model, &config.compat);
                if unsupported.contains(&key) {
                    assert_eq!(result.unwrap_err().kind, ErrorKind::InvalidRequest);
                } else {
                    let body = result.unwrap();
                    assert_eq!(body["reasoning_effort"], wire);
                    assert_eq!(
                        body.get("thinking").is_some(),
                        format == ThinkingFormat::Deepseek
                    );
                }
            }
            model
                .thinking_levels
                .as_mut()
                .unwrap()
                .insert(L::High, V::Wire("custom-effort".into()));
            let mut high = request();
            high.reasoning = Some(ReasoningLevel::High);
            assert_eq!(
                crate::openai::request::encode(&high, model, &config.compat).unwrap()["reasoning_effort"],
                "custom-effort"
            );
            model.thinking_levels.as_mut().unwrap().remove(&L::Minimal);
            high.reasoning = Some(ReasoningLevel::Minimal);
            assert_eq!(
                crate::openai::request::encode(&high, model, &config.compat).unwrap()["reasoning_effort"],
                "minimal"
            );
            model
                .thinking_levels
                .as_mut()
                .unwrap()
                .insert(L::Off, V::Wire("none".into()));
            let body = crate::openai::request::encode(&request(), model, &config.compat).unwrap();
            if format == ThinkingFormat::Openai {
                assert_eq!(body["reasoning_effort"], "none");
            } else {
                assert_eq!(body["thinking"], json!({"type":"disabled"}));
                assert!(body.get("reasoning_effort").is_none());
            }
        }
    }
}

#[tokio::test]
async fn thought_signature_on_parallel_tool_calls_round_trips() {
    let fixture = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_0\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"},\"extra_content\":{\"google\":{\"thought_signature\":\"sig_initial\"}}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"b.txt\\\"}\"},\"extra_content\":\"non_google_payload\"}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.txt\\\"\"},\"extra_content\":{\"google\":{\"thought_signature\":\"AY89a19HjvBjUXH7\"}}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"}\"},\"extra_content\":{\"google\":{\"thought_signature\":\"\"}}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":6}}\n\n",
        "data: [DONE]\n\n",
    );
    let sig_opaque = OpaqueData("{\"google\":{\"thought_signature\":\"AY89a19HjvBjUXH7\"}}".into());
    let call_0 = Content::ToolCall(ToolCall {
        id: "call_0".into(),
        name: "read".into(),
        arguments: json!({"path": "a.txt"}),
        opaque: Some(sig_opaque),
    });
    let call_1 = call("call_1", "b.txt");
    assert_eq!(
        collect(fixture).await,
        vec![
            Ok(start(0, "call_0")),
            Ok(args(0, "{\"path\":")),
            Ok(start(1, "call_1")),
            Ok(args(1, "{\"path\":\"b.txt\"}")),
            Ok(args(0, "\"a.txt\"")),
            Ok(args(0, "}")),
            Ok(usage(10, 6, 0, 0)),
            Ok(complete(0, call_0.clone())),
            Ok(complete(1, call_1.clone())),
            Ok(finish(StopReason::ToolUse, 10, 6, 0, 0)),
        ]
    );
    let mut request = request();
    request.tools = vec![tool("read")];
    request.messages.extend([
        message(Role::Assistant, vec![call_0, call_1]),
        message(
            Role::Tool,
            vec![
                Content::ToolResult(ToolResult {
                    call_id: "call_0".into(),
                    content: vec![text("alpha")],
                    is_error: false,
                }),
                Content::ToolResult(ToolResult {
                    call_id: "call_1".into(),
                    content: vec![text("beta")],
                    is_error: false,
                }),
            ],
        ),
        message(Role::User, vec![text("Continue")]),
    ]);
    let config = config("http://fixture.invalid");
    let body = crate::openai::request::encode(&request, &config.models[0], &config.compat).unwrap();
    let assistant = &body["messages"][1];
    assert!(assistant.get("extra_content").is_none());
    assert_eq!(
        assistant["tool_calls"][0]["extra_content"],
        json!({"google": {"thought_signature": "AY89a19HjvBjUXH7"}})
    );
    assert!(assistant["tool_calls"][1].get("extra_content").is_none());
}

#[tokio::test]
async fn thought_signature_on_leading_and_trailing_empty_text_deltas_round_trips() {
    let trailing = concat!(
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"!\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"\",\"role\":\"assistant\",\"extra_content\":{\"google\":{\"thought_signature\":\"sig_trailing\"}}},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let trailing_text = Content::Text {
        text: "Hello!".into(),
        opaque: Some(OpaqueData(
            "{\"google\":{\"thought_signature\":\"sig_trailing\"}}".into(),
        )),
    };
    assert_eq!(
        collect(trailing).await,
        vec![
            Ok(StreamEvent::TextDelta {
                index: 0,
                delta: "Hello".into(),
            }),
            Ok(StreamEvent::TextDelta {
                index: 0,
                delta: "!".into(),
            }),
            Ok(complete(0, trailing_text.clone())),
            Ok(missing_usage(StopReason::EndTurn)),
        ]
    );

    let leading_and_post_finish = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"\",\"extra_content\":{\"google\":{\"thought_signature\":\"sig_leading\"}}}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Answer\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"\",\"extra_content\":{\"google\":{\"thought_signature\":\"\"}}}}]}\n\n",
        "data: [DONE]\n\n",
    );
    assert_eq!(
        collect(leading_and_post_finish).await,
        vec![
            Ok(StreamEvent::TextDelta {
                index: 0,
                delta: "Answer".into(),
            }),
            Ok(complete(
                0,
                Content::Text {
                    text: "Answer".into(),
                    opaque: Some(OpaqueData(
                        "{\"google\":{\"thought_signature\":\"sig_leading\"}}".into(),
                    )),
                }
            )),
            Ok(missing_usage(StopReason::EndTurn)),
        ]
    );

    let both = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"\",\"extra_content\":{\"google\":{\"thought_signature\":\"sig_first\"}}}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"Updated\",\"extra_content\":{\"google\":{\"thought_signature\":\"sig_inline\"}}}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"\",\"extra_content\":{\"google\":123}}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"\",\"extra_content\":{\"google\":{\"thought_signature\":42}}},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    assert_eq!(
        collect(both).await,
        vec![
            Ok(StreamEvent::TextDelta {
                index: 0,
                delta: "Updated".into(),
            }),
            Ok(complete(
                0,
                Content::Text {
                    text: "Updated".into(),
                    opaque: Some(OpaqueData(
                        "{\"google\":{\"thought_signature\":\"sig_inline\"}}".into(),
                    )),
                }
            )),
            Ok(missing_usage(StopReason::EndTurn)),
        ]
    );

    let mut request = request();
    request.messages.extend([
        message(Role::Assistant, vec![trailing_text]),
        message(Role::User, vec![text("Follow up")]),
    ]);
    let config = config("http://fixture.invalid");
    let body = crate::openai::request::encode(&request, &config.models[0], &config.compat).unwrap();
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][1]["content"], "Hello!");
    assert_eq!(
        body["messages"][1]["extra_content"],
        json!({"google": {"thought_signature": "sig_trailing"}})
    );
}

#[tokio::test]
async fn thinking_only_with_trailing_thought_signature_skips_empty_text() {
    let fixture = concat!(
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"Plan first.\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"\",\"extra_content\":{\"google\":{\"thought_signature\":\"sig_unused\"}}},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let thinking = Content::Thinking {
        text: "Plan first.".into(),
        redacted: false,
        opaque: Some(OpaqueData("reasoning_content".into())),
    };
    let events = collect(fixture).await;
    assert_eq!(
        events,
        vec![
            Ok(StreamEvent::ThinkingDelta {
                index: 0,
                delta: "Plan first.".into(),
            }),
            Ok(complete(0, thinking.clone())),
            Ok(missing_usage(StopReason::EndTurn)),
        ]
    );

    let config = config("http://fixture.invalid");
    for assistant_blocks in [
        vec![thinking.clone()],
        vec![
            thinking,
            Content::Text {
                text: "   \n\t ".into(),
                opaque: Some(OpaqueData(
                    "{\"google\":{\"thought_signature\":\"sig_ws\"}}".into(),
                )),
            },
        ],
    ] {
        let mut request = request();
        request.messages.extend([
            message(Role::Assistant, assistant_blocks),
            message(Role::User, vec![text("Continue")]),
        ]);
        let body =
            crate::openai::request::encode(&request, &config.models[0], &config.compat).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["role"], "user");
    }
}

#[test]
fn invalid_thought_signature_opaque_is_rejected() {
    let config = config("http://fixture.invalid");
    let valid = OpaqueData("{\"google\":{\"thought_signature\":\"sig_ok\"}}".into());

    let mut ok_req = request();
    ok_req.tools = vec![tool("read")];
    ok_req.messages.extend([
        message(
            Role::Assistant,
            vec![
                Content::Text {
                    text: "Checking".into(),
                    opaque: Some(valid.clone()),
                },
                Content::ToolCall(ToolCall {
                    id: "call_ok".into(),
                    name: "read".into(),
                    arguments: json!({"path": "a.txt"}),
                    opaque: Some(valid),
                }),
            ],
        ),
        message(
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call_id: "call_ok".into(),
                content: vec![text("ok")],
                is_error: false,
            })],
        ),
    ]);
    assert!(crate::openai::request::encode(&ok_req, &config.models[0], &config.compat).is_ok());

    for bad in [
        "not-json",
        "[]",
        "{}",
        "{\"google\":{\"thought_signature\":\"sig\"},\"extra\":1}",
        "{\"other\":{\"thought_signature\":\"sig\"}}",
        "{\"google\":\"sig\"}",
        "{\"google\":{}}",
        "{\"google\":{\"thought_signature\":\"sig\",\"extra\":1}}",
        "{\"google\":{\"other\":\"sig\"}}",
        "{\"google\":{\"thought_signature\":123}}",
        "{\"google\":{\"thought_signature\":\"\"}}",
    ] {
        let bad_opaque = OpaqueData(bad.into());
        for text_value in ["Answer", ""] {
            let mut req = request();
            req.messages.push(message(
                Role::Assistant,
                vec![Content::Text {
                    text: text_value.into(),
                    opaque: Some(bad_opaque.clone()),
                }],
            ));
            let err = crate::openai::request::encode(&req, &config.models[0], &config.compat)
                .unwrap_err();
            assert_eq!(err.kind, ErrorKind::InvalidRequest, "{bad}");
            assert_eq!(
                err.message, "unsupported assistant content or replay metadata",
                "{bad}"
            );
        }

        let mut tool_req = request();
        tool_req.tools = vec![tool("read")];
        tool_req.messages.extend([
            message(
                Role::Assistant,
                vec![Content::ToolCall(ToolCall {
                    id: "call_bad".into(),
                    name: "read".into(),
                    arguments: json!({"path": "a.txt"}),
                    opaque: Some(bad_opaque),
                })],
            ),
            message(
                Role::Tool,
                vec![Content::ToolResult(ToolResult {
                    call_id: "call_bad".into(),
                    content: vec![text("out")],
                    is_error: false,
                })],
            ),
        ]);
        let err = crate::openai::request::encode(&tool_req, &config.models[0], &config.compat)
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidRequest, "{bad}");
        assert_eq!(
            err.message, "unsupported assistant content or replay metadata",
            "{bad}"
        );
    }
}
