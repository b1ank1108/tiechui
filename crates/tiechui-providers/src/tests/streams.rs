use tiechui_model::{OpaqueData, StopReason, Usage};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

use super::*;
use crate::ErrorKind;

fn delta(index: u32, value: &str) -> StreamEvent {
    StreamEvent::TextDelta {
        index,
        delta: value.into(),
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

fn arguments(index: u32, value: &str) -> StreamEvent {
    StreamEvent::ToolCallArgumentsDelta {
        index,
        delta: value.into(),
    }
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

pub(super) fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> StreamEvent {
    StreamEvent::Usage {
        usage: Usage {
            input,
            output,
            cache_read,
            cache_write,
            ..Default::default()
        },
    }
}

#[tokio::test]
async fn plain_text_and_cache_usage() {
    let server = serve(PLAIN, 1).await;
    assert_eq!(
        collect(&server.uri(), request()).await,
        vec![
            Ok(usage(7, 0, 11, 5)),
            Ok(delta(0, "Hello")),
            Ok(delta(0, "!")),
            Ok(complete(0, text("Hello!"))),
            Ok(usage(7, 2, 11, 5)),
            Ok(finish(StopReason::EndTurn, 7, 2, 11, 5)),
        ]
    );
}

#[tokio::test]
async fn redacted_thinking_completes_without_exposing_text() {
    let server = serve(REDACTED, 1).await;
    assert_eq!(
        collect(&server.uri(), request()).await,
        vec![
            Ok(StreamEvent::Usage {
                usage: Usage {
                    partial: true,
                    input: 3,
                    ..Usage::default()
                }
            }),
            Ok(complete(
                0,
                Content::Thinking {
                    text: String::new(),
                    redacted: true,
                    opaque: Some(OpaqueData(" opaque-fixture\n\t ".into())),
                }
            )),
            Ok(delta(1, "Answer")),
            Ok(complete(1, text("Answer"))),
            Ok(usage(3, 2, 0, 0)),
            Ok(finish(StopReason::EndTurn, 3, 2, 0, 0)),
        ]
    );
}

#[tokio::test]
async fn unknown_blocks_deltas_initial_fallback_and_missing_usage_are_tolerated() {
    let server = serve(UNKNOWN, 1).await;
    assert_eq!(
        collect(&server.uri(), request()).await,
        vec![
            Ok(delta(1, "Visible")),
            Ok(complete(1, text("Visible"))),
            Ok(missing_usage(StopReason::EndTurn)),
        ]
    );
}

#[tokio::test]
async fn fallback_after_even_an_empty_content_block_is_a_stream_error() {
    let server = serve(FALLBACK, 1).await;
    assert_eq!(
        collect(&server.uri(), request()).await,
        vec![Err(ProviderError::stream(
            "Anthropic performed an unsupported mid-output model fallback"
        )),]
    );
}

#[tokio::test]
async fn unknown_delta_on_an_index_never_started_is_still_an_error() {
    let fixture = UNKNOWN.replace(
        "\"index\":0,\"delta\":{\"type\":\"future_delta\"}",
        "\"index\":99,\"delta\":{\"type\":\"future_delta\"}",
    );
    let server = serve(&fixture, 1).await;
    assert_eq!(
        collect(&server.uri(), request()).await,
        vec![Err(ProviderError::stream(
            "delta for inactive content block"
        )),]
    );
}

#[tokio::test]
async fn thinking_signature_tool_fragments_and_usage_overrides() {
    let server = serve(THINKING, 1).await;
    assert_eq!(
        collect(&server.uri(), request()).await,
        vec![
            Ok(usage(12, 0, 18, 7)),
            Ok(StreamEvent::ThinkingDelta {
                index: 0,
                delta: "Check.".into()
            }),
            Ok(complete(
                0,
                Content::Thinking {
                    text: "Check.".into(),
                    redacted: false,
                    opaque: Some(OpaqueData("sig-fixture \t".into()))
                }
            )),
            Ok(start(1, "call_read")),
            Ok(arguments(1, "{\"path\":")),
            Ok(arguments(1, "\"sample.txt\"}")),
            Ok(complete(1, call("call_read", "sample.txt"))),
            Ok(usage(12, 9, 19, 0)),
            Ok(finish(StopReason::ToolUse, 12, 9, 19, 0)),
        ]
    );
}

#[tokio::test]
async fn parallel_tool_calls_keep_indices_and_arrival_order() {
    let server = serve(PARALLEL, 1).await;
    assert_eq!(
        collect(&server.uri(), request()).await,
        vec![
            Ok(usage(4, 0, 0, 0)),
            Ok(start(0, "call_a")),
            Ok(start(1, "call_b")),
            Ok(arguments(0, "{\"path\":\"a")),
            Ok(arguments(1, "{\"path\":\"b")),
            Ok(arguments(1, ".txt\"}")),
            Ok(arguments(0, ".txt\"}")),
            Ok(complete(1, call("call_b", "b.txt"))),
            Ok(complete(0, call("call_a", "a.txt"))),
            Ok(usage(4, 6, 0, 0)),
            Ok(finish(StopReason::ToolUse, 4, 6, 0, 0)),
        ]
    );
}

#[tokio::test]
async fn error_midstream_terminates_without_finish() {
    let server = serve(ERROR, 1).await;
    assert_eq!(
        collect(&server.uri(), request()).await,
        vec![
            Ok(StreamEvent::Usage {
                usage: Usage {
                    partial: true,
                    input: 1,
                    ..Usage::default()
                }
            }),
            Ok(delta(0, "Partial")),
            Err(ProviderError::new(
                ErrorKind::Overloaded,
                "fixture overload"
            )),
        ]
    );
}

#[tokio::test]
async fn stop_reason_without_terminal_event_is_truncated() {
    let server = serve(TRUNCATED, 1).await;
    assert_eq!(
        collect(&server.uri(), request()).await,
        vec![
            Ok(StreamEvent::Usage {
                usage: Usage {
                    partial: true,
                    input: 1,
                    ..Usage::default()
                }
            }),
            Ok(delta(0, "Cut")),
            Ok(complete(0, text("Cut"))),
            Ok(usage(1, 1, 0, 0)),
            Err(ProviderError::stream("stream ended before message_stop")),
        ]
    );
}

#[tokio::test]
async fn malformed_or_incomplete_streams_never_finish() {
    let cases = [
        "event: message_start\ndata: broken\n\n".to_owned(),
        PLAIN.replace("\"index\":0,\"delta\"", "\"index\":9,\"delta\""),
        PLAIN.replace(
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "",
        ),
        PLAIN.replace("\"stop_reason\":\"end_turn\"", "\"stop_reason\":null"),
        THINKING.replace("sample.txt", "sample\\\".txt"),
    ];
    for fixture in cases {
        let server = serve(&fixture, 1).await;
        let events = collect(&server.uri(), request()).await;
        assert_eq!(
            events.last().unwrap().as_ref().unwrap_err().kind,
            ErrorKind::TransientNetwork
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Ok(StreamEvent::Finish { .. })))
        );
    }
}

#[tokio::test]
async fn null_usage_and_supported_stop_reasons() {
    for (wire, reason) in [
        ("max_tokens", StopReason::Length),
        ("stop_sequence", StopReason::EndTurn),
        ("pause_turn", StopReason::EndTurn),
    ] {
        let fixture = PLAIN.replace("\"stop_reason\":\"end_turn\"", &format!("\"stop_reason\":\"{wire}\""))
            .replace("\"usage\":{\"input_tokens\":null,\"output_tokens\":2,\"cache_creation_input_tokens\":null}", "\"usage\":null");
        let server = serve(&fixture, 1).await;
        assert_eq!(
            collect(&server.uri(), request()).await.last(),
            Some(&Ok(finish(reason, 7, 0, 11, 5)))
        );
    }
}

// This server sends headers/body independently, then keeps the socket open indefinitely.
pub(super) struct StalledServer {
    pub(super) base: String,
    task: tokio::task::JoinHandle<()>,
    pub(super) connected: tokio::sync::oneshot::Receiver<()>,
}

impl StalledServer {
    pub(super) async fn start(body: Option<&str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let body = body.map(str::to_owned);
        let (connected, receiver) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut buffer = [0; 1024];
                let size = socket.read(&mut buffer).await.unwrap();
                assert!(size > 0);
                request.extend_from_slice(&buffer[..size]);
            }
            if let Some(body) = body {
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.unwrap();
                socket.write_all(body.as_bytes()).await.unwrap();
                socket.flush().await.unwrap();
            }
            let _ = connected.send(());
            std::future::pending::<()>().await;
            drop(socket);
        });
        Self {
            base,
            task,
            connected: receiver,
        }
    }
}

impl Drop for StalledServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
