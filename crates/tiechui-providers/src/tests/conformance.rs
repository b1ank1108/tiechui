use std::time::{Duration, Instant};

use tiechui_model::ToolResult;

use super::streams::StalledServer;
use super::*;
use crate::{ErrorKind, openai::Openai, responses::Responses};

fn adapter(wire: Wire, base: &str, client: reqwest::Client) -> Box<dyn ModelProvider> {
    let mut config = config(base);
    config.wire = wire;
    match wire {
        Wire::AnthropicMessages => {
            Box::new(Anthropic::new("fixture", &config, "fixture-key", client).unwrap())
        }
        Wire::OpenaiCompletions => {
            Box::new(Openai::new("fixture", &config, "fixture-key", client).unwrap())
        }
        Wire::OpenaiResponses => {
            Box::new(Responses::new("fixture", &config, "fixture-key", client).unwrap())
        }
    }
}

const WIRES: [Wire; 3] = [
    Wire::AnthropicMessages,
    Wire::OpenaiCompletions,
    Wire::OpenaiResponses,
];

const RESPONSES_PREFIX: &str = "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"Hello\"}\n\n";

fn fixture(wire: Wire) -> &'static str {
    match wire {
        Wire::AnthropicMessages => PLAIN,
        Wire::OpenaiCompletions => super::openai::PLAIN,
        Wire::OpenaiResponses => super::responses::PLAIN,
    }
}

#[tokio::test]
async fn same_neutral_conversations_produce_the_same_events_on_both_wires() {
    let mut later = request();
    later.messages.extend([
        message(Role::Assistant, vec![call("call_read", "a.txt")]),
        message(
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call_id: "call_read".into(),
                content: vec![text("contents")],
                is_error: false,
            })],
        ),
        message(Role::User, vec![text("Continue")]),
    ]);
    later.tools = vec![tool("read")];
    let mut baseline = None;
    for wire in WIRES {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(fixture(wire), "text/event-stream"),
            )
            .expect(2)
            .mount(&server)
            .await;
        let provider = adapter(
            wire,
            &server.uri(),
            crate::client_builder().no_proxy().build().unwrap(),
        );
        for request in [request(), later.clone()] {
            let events: Vec<_> = provider
                .stream(request, CancellationToken::new())
                .await
                .unwrap()
                .collect()
                .await;
            assert!(events.iter().all(Result::is_ok));
            assert!(matches!(
                events.last(),
                Some(Ok(StreamEvent::Finish { .. }))
            ));
            let events: Vec<_> = events
                .into_iter()
                .filter(|event| !matches!(event, Ok(StreamEvent::Usage { .. })))
                .collect();
            if let Some(expected) = &baseline {
                assert_eq!(&events, expected);
            } else {
                baseline = Some(events);
            }
        }
    }
}

#[tokio::test]
async fn both_wires_cancel_stalled_bodies_within_100_ms() {
    for wire in WIRES {
        let prefix = match wire {
            Wire::AnthropicMessages => PLAIN.split("event: ping").next().unwrap(),
            Wire::OpenaiCompletions => {
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n"
            }
            Wire::OpenaiResponses => RESPONSES_PREFIX,
        };
        let server = StalledServer::start(Some(prefix)).await;
        let provider = adapter(
            wire,
            &server.base,
            crate::client_builder().no_proxy().build().unwrap(),
        );
        let cancel = CancellationToken::new();
        let mut stream = provider.stream(request(), cancel.clone()).await.unwrap();
        if matches!(wire, Wire::AnthropicMessages) {
            assert_eq!(
                stream.next().await,
                Some(Ok(super::streams::usage(7, 0, 11, 5)))
            );
        }
        assert_eq!(
            stream.next().await,
            Some(Ok(StreamEvent::TextDelta {
                index: 0,
                delta: "Hello".into()
            }))
        );
        let mut next = Box::pin(stream.next());
        assert!(futures::poll!(&mut next).is_pending());
        let started = Instant::now();
        cancel.cancel();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
        assert_eq!(
            tokio::time::timeout_at(deadline, next).await.unwrap(),
            Some(Err(ProviderError::cancelled()))
        );
        assert!(
            tokio::time::timeout_at(deadline, stream.next())
                .await
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}

#[tokio::test]
async fn both_wires_cancel_before_sending_and_while_waiting_for_headers() {
    for wire in WIRES {
        let mut server = StalledServer::start(None).await;
        let provider = adapter(
            wire,
            &server.base,
            crate::client_builder().no_proxy().build().unwrap(),
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            provider.stream(request(), cancel).await.err().unwrap().kind,
            ErrorKind::Cancelled
        );
        let cancel = CancellationToken::new();
        let mut sending = Box::pin(provider.stream(request(), cancel.clone()));
        tokio::select! {
            result = &mut sending => panic!("completed before cancellation: {}", result.is_ok()),
            connected = &mut server.connected => connected.unwrap(),
        }
        cancel.cancel();
        assert_eq!(
            tokio::time::timeout(Duration::from_millis(100), sending)
                .await
                .unwrap()
                .err()
                .unwrap(),
            ProviderError::cancelled()
        );
    }
}

#[tokio::test]
async fn both_wires_bound_silent_headers_and_body_reads() {
    for wire in WIRES {
        let prefix = match wire {
            Wire::AnthropicMessages => PLAIN.split("event: ping").next().unwrap(),
            Wire::OpenaiCompletions => {
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n"
            }
            Wire::OpenaiResponses => RESPONSES_PREFIX,
        };
        for body in [None, Some(""), Some(prefix)] {
            let server = StalledServer::start(body).await;
            let provider = adapter(
                wire,
                &server.base,
                crate::client_builder()
                    .no_proxy()
                    .read_timeout(Duration::from_millis(30))
                    .build()
                    .unwrap(),
            );
            let error = tokio::time::timeout(Duration::from_secs(1), async {
                match provider.stream(request(), CancellationToken::new()).await {
                    Err(error) => error,
                    Ok(mut stream) => {
                        if body.is_some_and(|body| !body.is_empty()) {
                            if matches!(wire, Wire::AnthropicMessages) {
                                assert_eq!(
                                    stream.next().await,
                                    Some(Ok(super::streams::usage(7, 0, 11, 5)))
                                );
                            }
                            assert_eq!(
                                stream.next().await,
                                Some(Ok(StreamEvent::TextDelta {
                                    index: 0,
                                    delta: "Hello".into()
                                }))
                            );
                        }
                        let error = stream.next().await.unwrap().unwrap_err();
                        assert!(stream.next().await.is_none());
                        error
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(error.kind, ErrorKind::TransientNetwork);
        }
    }
}

#[tokio::test]
async fn both_wires_finish_at_the_terminal_marker_without_socket_eof() {
    for wire in WIRES {
        let server = StalledServer::start(Some(fixture(wire))).await;
        let provider = adapter(
            wire,
            &server.base,
            crate::client_builder().no_proxy().build().unwrap(),
        );
        let events: Vec<_> = tokio::time::timeout(Duration::from_secs(1), async {
            provider
                .stream(request(), CancellationToken::new())
                .await
                .unwrap()
                .collect()
                .await
        })
        .await
        .unwrap();
        assert_eq!(
            events.len(),
            if matches!(wire, Wire::AnthropicMessages) {
                6
            } else {
                5
            }
        );
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Finish { .. }))
        ));
    }
}

#[tokio::test]
async fn both_wires_share_http_error_body_and_retry_hint_behavior() {
    for wire in WIRES {
        for (status, body, expected, message) in [
            (401, "{}", ErrorKind::Auth, "HTTP 401"),
            (
                429,
                r#"{"error":"slow down"}"#,
                ErrorKind::RateLimit,
                "slow down",
            ),
            (
                503,
                r#"{"error":{"message":"busy"}}"#,
                ErrorKind::Overloaded,
                "busy",
            ),
            (
                502,
                "<html>gateway</html>",
                ErrorKind::TransientNetwork,
                "HTTP 502",
            ),
            (
                400,
                r#"{"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"limit"}}"#,
                ErrorKind::ContextOverflow,
                "limit",
            ),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("retry-after-ms", "1250")
                        .set_body_raw(body, "application/json"),
                )
                .expect(1)
                .mount(&server)
                .await;
            let provider = adapter(
                wire,
                &server.uri(),
                crate::client_builder().no_proxy().build().unwrap(),
            );
            let error = provider
                .stream(request(), CancellationToken::new())
                .await
                .err()
                .unwrap();
            assert_eq!(error.kind, expected);
            assert_eq!(error.message, message);
            assert_eq!(error.retry_after, Some(Duration::from_millis(1250)));
        }
    }
}

#[tokio::test]
async fn usage_in_a_refusal_event_is_delivered_before_the_error_on_each_wire() {
    for wire in WIRES {
        let fixture = match wire {
            Wire::AnthropicMessages => {
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\"},\"usage\":{\"input_tokens\":9,\"output_tokens\":2}}\n\n"
            }
            Wire::OpenaiCompletions => {
                "data: {\"choices\":[{\"finish_reason\":\"content_filter\"}],\"usage\":{\"prompt_tokens\":9,\"completion_tokens\":2}}\n\n"
            }
            Wire::OpenaiResponses => {
                "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"content_filter\"},\"usage\":{\"input_tokens\":9,\"output_tokens\":2}}}\n\n"
            }
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(fixture, "text/event-stream"))
            .expect(1)
            .mount(&server)
            .await;
        let events: Vec<_> = adapter(
            wire,
            &server.uri(),
            crate::client_builder().no_proxy().build().unwrap(),
        )
        .stream(request(), CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], Ok(super::streams::usage(9, 2, 0, 0)));
        assert_eq!(
            events[1].as_ref().unwrap_err().kind,
            ErrorKind::InvalidRequest
        );
    }
}
