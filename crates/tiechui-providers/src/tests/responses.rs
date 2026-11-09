use tiechui_config::{
    CompletionsCompat, ThinkingLevel as L, ThinkingLevelValue as V, UnsupportedLevel,
};
use tiechui_model::{OpaqueData, StopReason, ToolResult, Usage};

use super::streams::usage;
use super::*;
use crate::{ErrorKind, responses::Responses};

pub(super) const PLAIN: &str = include_str!("../../tests/fixtures/openai-responses/plain_text.sse");
const THINKING: &str = include_str!("../../tests/fixtures/openai-responses/thinking_tool.sse");

pub(super) fn provider(base: &str) -> Responses {
    let mut config = config(base);
    config.wire = Wire::OpenaiResponses;
    Responses::new(
        "fixture",
        &config,
        "fixture-key",
        crate::client_builder().no_proxy().build().unwrap(),
    )
    .unwrap()
}

async fn collect(fixture: &str) -> Vec<Result<StreamEvent, ProviderError>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer fixture-key"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(fixture, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    provider(&server.uri())
        .stream(request(), CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await
}

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}

fn finish(reason: StopReason, input: u64, output: u64, cache_read: u64) -> StreamEvent {
    StreamEvent::Finish {
        reason,
        usage: Usage {
            input,
            output,
            cache_read,
            ..Default::default()
        },
    }
}

fn complete(index: u32, content: Content) -> StreamEvent {
    StreamEvent::ContentComplete { index, content }
}

fn args(index: u32, delta: &str) -> StreamEvent {
    StreamEvent::ToolCallArgumentsDelta {
        index,
        delta: delta.into(),
    }
}

fn thinking(delta: &str) -> StreamEvent {
    StreamEvent::ThinkingDelta {
        index: 0,
        delta: delta.into(),
    }
}

#[tokio::test]
async fn text_and_usage_arrive_with_the_completed_response() {
    assert_eq!(
        collect(PLAIN).await,
        vec![
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
            Ok(StreamEvent::Finish {
                reason: StopReason::EndTurn,
                usage: Usage {
                    input: 7,
                    output: 2,
                    cache_read: 11,
                    cache_write: 5,
                    ..Default::default()
                },
            }),
        ]
    );
}

#[tokio::test]
async fn reasoning_and_tool_call_are_reassembled_and_replay_without_item_ids() {
    let opaque = json!({"type":"reasoning","summary":[{"type":"summary_text","text":"Check."},{"type":"summary_text","text":"Read it."}],"encrypted_content":"sealed"});
    let reasoning = Content::Thinking {
        text: "Check.\n\nRead it.".into(),
        redacted: false,
        opaque: Some(OpaqueData(opaque.to_string())),
    };
    assert_eq!(
        collect(THINKING).await,
        vec![
            Ok(thinking("Check.")),
            Ok(thinking("\n\n")),
            Ok(thinking("Read it.")),
            Ok(StreamEvent::ToolCallStart {
                index: 1,
                id: "call_read".into(),
                name: "read".into(),
            }),
            Ok(args(1, "{\"path\":")),
            Ok(args(1, "\"sample.txt\"}")),
            Ok(usage(9, 8, 3, 0)),
            Ok(complete(0, reasoning.clone())),
            Ok(complete(1, call("call_read", "sample.txt"))),
            Ok(finish(StopReason::ToolUse, 9, 8, 3)),
        ]
    );
    let mut request = request();
    request.messages.extend([
        message(
            Role::Assistant,
            vec![reasoning, call("call_read", "sample.txt")],
        ),
        message(
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call_id: "call_read".into(),
                content: vec![
                    text("contents"),
                    Content::Image {
                        mime_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                ],
                is_error: false,
            })],
        ),
        message(
            Role::Assistant,
            vec![
                Content::Thinking {
                    text: "Unsealed.".into(),
                    redacted: false,
                    opaque: None,
                },
                text("Read it."),
            ],
        ),
        message(Role::User, vec![text("Continue")]),
    ]);
    request.tools = vec![tool("read")];
    let config = config("http://fixture.invalid");
    let body =
        crate::responses::request::encode(&request, &config.models[0], &config.compat).unwrap();
    assert_eq!(body["input"][1], opaque);
    insta::assert_json_snapshot!("responses_reasoning_tool_replay", body);
}

#[tokio::test]
async fn first_turn_request_is_captured_with_api_root_and_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/gateway/v1/responses"))
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
    insta::assert_json_snapshot!("responses_first_turn", body);
}

#[tokio::test]
async fn items_completed_without_deltas_are_started_from_the_done_event() {
    let fixture = [
        event(
            json!({"type":"response.output_item.done","output_index":3,"item":{"type":"function_call","call_id":"call_a","name":"read","arguments":"{\"path\":\"a\"}"}}),
        ),
        event(json!({"type":"response.output_item.done","output_index":4,"item":{"type":"web_search_call","id":"ws_1"}})),
        event(
            json!({"type":"response.completed","response":{"usage":{"input_tokens":5,"output_tokens":4}}}),
        ),
    ]
    .concat();
    assert_eq!(
        collect(&fixture).await,
        vec![
            Ok(StreamEvent::ToolCallStart {
                index: 0,
                id: "call_a".into(),
                name: "read".into(),
            }),
            Ok(args(0, "{\"path\":\"a\"}")),
            Ok(usage(5, 4, 0, 0)),
            Ok(complete(0, call("call_a", "a"))),
            Ok(finish(StopReason::ToolUse, 5, 4, 0)),
        ]
    );
}

#[tokio::test]
async fn incomplete_failed_and_error_events_end_the_stream() {
    let prefix = PLAIN
        .split("event: response.output_item.done")
        .next()
        .unwrap();
    let incomplete = |reason: &str| {
        format!(
            "{prefix}{}",
            event(
                json!({"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":reason},"usage":{"input_tokens":3,"output_tokens":1}}})
            )
        )
    };
    let events = collect(&incomplete("max_output_tokens")).await;
    assert_eq!(
        events.last(),
        Some(&Ok(finish(StopReason::Length, 3, 1, 0)))
    );
    let events = collect(&incomplete("content_filter")).await;
    assert_eq!(events[events.len() - 2], Ok(usage(3, 1, 0, 0)));
    assert_eq!(
        events.last().unwrap().as_ref().unwrap_err().kind,
        ErrorKind::InvalidRequest
    );
    for (fixture, kind) in [
        (
            event(
                json!({"type":"response.failed","response":{"status":"failed","error":{"code":"context_length_exceeded","message":"too long"}}}),
            ),
            ErrorKind::ContextOverflow,
        ),
        (
            event(json!({"type":"error","code":"context_length_exceeded","message":"too long"})),
            ErrorKind::ContextOverflow,
        ),
    ] {
        let events = collect(&fixture).await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].as_ref().unwrap_err().kind, kind);
        assert_eq!(events[0].as_ref().unwrap_err().message, "too long");
    }
}

#[tokio::test]
async fn truncated_and_invalid_streams_never_finish() {
    for fixture in [
        PLAIN
            .split("event: response.completed")
            .next()
            .unwrap()
            .to_owned(),
        "data: {\n\n".into(),
        THINKING.replace("sample.txt\\\"}", "sample.txt\\\""),
        THINKING.replace("\"output_index\":1", "\"output_index\":0"),
        PLAIN.replace(
            "\"output_index\":0,\"content_index\"",
            "\"output_index\":7,\"content_index\"",
        ),
        THINKING.replace("\"call_id\":\"call_read\",", ""),
        format!(
            "{}{}",
            PLAIN.split("event: response.completed").next().unwrap(),
            event(
                json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"other"}}})
            )
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

#[test]
fn levels_compat_fields_and_roles_shape_the_request() {
    let mut config = config("http://fixture.invalid");
    let mut request = request();
    request
        .messages
        .insert(0, message(Role::System, vec![text("Be concise.")]));
    let encode = |request: &ModelRequest, model: &ModelConfig, compat: &CompletionsCompat| {
        crate::responses::request::encode(request, model, compat).unwrap()
    };
    let body = encode(&request, &config.models[0], &config.compat);
    assert_eq!(body["reasoning"], json!({"summary":"auto"}));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["store"], false);
    assert_eq!(body["input"][0]["role"], "developer");
    assert!(body.get("tools").is_none());
    config.models[0].thinking_levels = Some(
        [
            (L::Off, V::Wire("none".into())),
            (L::Low, V::Wire("minimal".into())),
            (L::Max, V::Unsupported(UnsupportedLevel)),
        ]
        .into(),
    );
    let body = encode(&request, &config.models[0], &config.compat);
    assert_eq!(body["reasoning"]["effort"], "none");
    request.reasoning = Some(ReasoningLevel::Low);
    let body = encode(&request, &config.models[0], &config.compat);
    assert_eq!(body["reasoning"]["effort"], "minimal");
    request.reasoning = Some(ReasoningLevel::Medium);
    let compat = CompletionsCompat {
        supports_store: false,
        supports_developer_role: false,
        supports_reasoning_effort: false,
        ..Default::default()
    };
    let body = encode(&request, &config.models[0], &compat);
    assert_eq!(body["reasoning"], json!({"summary":"auto"}));
    assert!(body.get("store").is_none());
    assert_eq!(body["input"][0]["role"], "system");
    request.reasoning = Some(ReasoningLevel::Max);
    assert_eq!(
        crate::responses::request::encode(&request, &config.models[0], &config.compat)
            .unwrap_err()
            .kind,
        ErrorKind::InvalidRequest
    );
    request.reasoning = None;
    config.models[0].reasoning = false;
    let body = encode(&request, &config.models[0], &config.compat);
    assert!(body.get("reasoning").is_none() && body.get("include").is_none());
    assert_eq!(body["input"][0]["role"], "system");
    let mut foreign = request.clone();
    foreign.messages.push(message(
        Role::Assistant,
        vec![Content::Thinking {
            text: String::new(),
            redacted: false,
            opaque: Some(OpaqueData("signature".into())),
        }],
    ));
    assert_eq!(
        crate::responses::request::encode(&foreign, &config.models[0], &config.compat)
            .unwrap_err()
            .kind,
        ErrorKind::InvalidRequest
    );
}
