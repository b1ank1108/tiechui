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
async fn decodes_gzip_and_zstd_compressed_sse_streams() {
    const PLAIN_GZIP: &[u8] = &[
        31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 189, 83, 65, 110, 195, 32, 16, 188, 251, 21, 116, 207,
        86, 213, 84, 170, 42, 113, 175, 212, 95, 32, 106, 54, 41, 10, 93, 144, 189, 174, 92, 69,
        252, 189, 16, 219, 41, 78, 156, 228, 214, 19, 176, 51, 236, 12, 218, 1, 191, 145, 88, 138,
        47, 236, 58, 189, 67, 213, 177, 110, 185, 50, 154, 181, 20, 7, 224, 159, 128, 32, 97, 1,
        66, 61, 159, 65, 30, 192, 154, 132, 111, 237, 192, 125, 139, 42, 56, 109, 41, 225, 253, 9,
        165, 208, 179, 98, 191, 71, 234, 64, 190, 214, 224, 123, 46, 43, 79, 53, 52, 186, 249, 68,
        213, 162, 54, 106, 201, 222, 108, 102, 176, 73, 40, 91, 79, 103, 132, 151, 24, 99, 85, 225,
        232, 191, 241, 196, 105, 163, 62, 156, 111, 246, 235, 175, 88, 161, 36, 175, 150, 12, 14,
        163, 147, 18, 207, 238, 167, 123, 140, 67, 38, 30, 23, 9, 112, 85, 212, 160, 99, 125, 91,
        244, 72, 89, 136, 142, 149, 165, 216, 137, 54, 73, 190, 163, 115, 190, 212, 13, 150, 118,
        231, 66, 185, 6, 255, 110, 237, 1, 110, 204, 192, 135, 123, 35, 240, 161, 80, 252, 107, 52,
        231, 109, 213, 246, 2, 132, 194, 102, 110, 151, 131, 212, 121, 74, 52, 36, 163, 82, 40, 9,
        226, 213, 60, 82, 239, 220, 69, 36, 159, 239, 164, 46, 95, 138, 151, 86, 215, 94, 91, 98,
        121, 52, 82, 188, 145, 17, 126, 43, 166, 255, 242, 88, 253, 2, 111, 105, 108, 254, 125, 3,
        0, 0,
    ];
    const PLAIN_ZSTD: &[u8] = &[
        40, 181, 47, 253, 4, 88, 213, 8, 0, 194, 77, 40, 30, 80, 75, 226, 6, 163, 71, 229, 166,
        180, 36, 34, 86, 63, 214, 84, 38, 102, 232, 8, 244, 17, 139, 148, 226, 9, 197, 44, 86, 15,
        1, 36, 160, 32, 184, 164, 53, 40, 72, 200, 241, 181, 222, 14, 172, 245, 246, 248, 182, 215,
        39, 173, 30, 191, 236, 82, 219, 97, 220, 238, 140, 219, 1, 188, 88, 197, 138, 85, 140, 215,
        210, 131, 99, 246, 210, 244, 163, 121, 27, 218, 120, 67, 211, 170, 155, 39, 16, 47, 132,
        211, 171, 189, 140, 8, 57, 142, 131, 96, 89, 117, 106, 67, 135, 195, 73, 169, 237, 104, 2,
        74, 116, 35, 208, 143, 111, 10, 240, 110, 53, 193, 125, 250, 177, 125, 55, 95, 149, 139,
        29, 109, 175, 13, 133, 56, 62, 169, 55, 224, 205, 143, 87, 198, 208, 111, 40, 152, 157,
        137, 116, 203, 118, 71, 139, 116, 78, 21, 10, 246, 234, 130, 6, 46, 32, 48, 66, 130, 164,
        74, 30, 51, 147, 253, 195, 69, 124, 10, 145, 31, 184, 127, 76, 121, 178, 2, 222, 107, 73,
        102, 194, 164, 172, 81, 164, 33, 128, 3, 116, 250, 97, 78, 8, 48, 223, 0, 145, 1, 101, 15,
        160, 136, 208, 189, 12, 26, 184, 51, 15, 83, 192, 147, 192, 28, 214, 167, 70, 200, 14, 182,
        158, 6, 234, 26, 192, 151, 80, 131, 193, 7, 134, 46, 112, 81, 227, 112, 13, 121, 148, 0,
        11, 28, 240, 208, 112, 96, 67, 18, 36, 85, 127, 120, 86, 72, 157, 131, 8, 44, 182, 172, 86,
        60, 24, 92, 240, 76, 52, 130, 227, 145, 1, 249, 155, 142, 228,
    ];
    for (encoding, payload) in [("gzip", PLAIN_GZIP), ("zstd", PLAIN_ZSTD)] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-encoding", encoding)
                    .set_body_raw(payload.to_vec(), "text/event-stream"),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            collect(&server.uri(), request()).await,
            vec![
                Ok(usage(7, 0, 11, 5)),
                Ok(delta(0, "Hello")),
                Ok(delta(0, "!")),
                Ok(complete(0, text("Hello!"))),
                Ok(usage(7, 2, 11, 5)),
                Ok(finish(StopReason::EndTurn, 7, 2, 11, 5)),
            ],
            "encoding={encoding}"
        );
    }
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
