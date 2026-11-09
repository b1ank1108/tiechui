use std::fmt::Debug;

use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tiechui_model::*;

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) -> Value {
    let json = serde_json::to_value(value).expect("fixture serializes");
    let decoded: T = serde_json::from_value(json.clone()).expect("fixture deserializes");
    assert_eq!(&decoded, value);
    json
}

fn call() -> ToolCall {
    ToolCall {
        id: "call-1".into(),
        name: "read".into(),
        arguments: json!({"path": "fixture.txt", "limit": 20}),
        opaque: Some(OpaqueData("tool-replay-fixture".into())),
    }
}

fn text() -> Content {
    Content::Text {
        text: "hello 世界".into(),
        opaque: None,
    }
}

#[test]
fn role_variants() {
    let values = [Role::System, Role::User, Role::Assistant, Role::Tool];
    insta::assert_json_snapshot!(values.iter().map(round_trip).collect::<Vec<_>>());
}

#[test]
fn content_variants() {
    let values = [
        text(),
        Content::Image {
            mime_type: "image/png".into(),
            data: "AA==".into(),
        },
        Content::Thinking {
            text: "check the input".into(),
            redacted: false,
            opaque: Some(OpaqueData("opaque-signature-fixture".into())),
        },
        Content::ToolCall(call()),
        Content::ToolResult(ToolResult {
            call_id: "call-1".into(),
            content: vec![text()],
            is_error: false,
        }),
    ];
    insta::assert_json_snapshot!(values.iter().map(round_trip).collect::<Vec<_>>());
}

#[test]
fn thinking_redaction_defaults_false_and_round_trips_when_true() {
    let ordinary: Content = serde_json::from_value(json!({"kind":"thinking", "text":""})).unwrap();
    assert_eq!(
        ordinary,
        Content::Thinking {
            text: String::new(),
            redacted: false,
            opaque: None
        }
    );
    assert_eq!(round_trip(&ordinary), json!({"kind":"thinking", "text":""}));
    let redacted = Content::Thinking {
        text: String::new(),
        redacted: true,
        opaque: Some(OpaqueData(" opaque\n\t ".into())),
    };
    assert_eq!(
        round_trip(&redacted),
        json!({"kind":"thinking", "text":"", "redacted":true, "opaque":" opaque\n\t "})
    );
}

#[test]
fn reasoning_levels_round_trip_in_neutral_requests() {
    for (level, name) in [
        (ReasoningLevel::Minimal, "minimal"),
        (ReasoningLevel::Low, "low"),
        (ReasoningLevel::Medium, "medium"),
        (ReasoningLevel::High, "high"),
        (ReasoningLevel::Xhigh, "xhigh"),
        (ReasoningLevel::Max, "max"),
    ] {
        let request = ModelRequest {
            disable_cache: false,
            cache_key: None,
            input_tokens: None,
            model: ModelId("fixture/model".into()),
            messages: vec![],
            tools: vec![],
            max_output: None,
            reasoning: Some(level),
        };
        assert_eq!(round_trip(&request)["reasoning"], name);
    }
}

#[test]
fn stop_reason_variants() {
    let values = [
        StopReason::EndTurn,
        StopReason::Length,
        StopReason::ToolUse,
        StopReason::Cancelled,
        StopReason::Error,
    ];
    insta::assert_json_snapshot!(values.iter().map(round_trip).collect::<Vec<_>>());
}

#[test]
fn stream_event_variants() {
    let values = [
        StreamEvent::TextDelta {
            index: 0,
            delta: "hello".into(),
        },
        StreamEvent::ThinkingDelta {
            index: 1,
            delta: "check".into(),
        },
        StreamEvent::ToolCallStart {
            index: 2,
            id: "call-1".into(),
            name: "read".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 2,
            delta: "{\"path\":".into(),
        },
        StreamEvent::ContentComplete {
            index: 2,
            content: Content::ToolCall(call()),
        },
        StreamEvent::Usage {
            usage: Usage {
                input: 5,
                output: 1,
                cache_read: 2,
                cache_write: 0,
                ..Default::default()
            },
        },
        StreamEvent::Finish {
            reason: StopReason::ToolUse,
            usage: Usage {
                input: 17,
                output: 5,
                cache_read: 11,
                cache_write: 3,
                ..Default::default()
            },
        },
    ];
    insta::assert_json_snapshot!(values.iter().map(round_trip).collect::<Vec<_>>());
}

#[test]
fn message_tool_schema_and_model_metadata() {
    let message = Message {
        role: Role::User,
        content: vec![text()],
        opaque: None,
    };
    let spec = ToolSpec {
        name: "read".into(),
        description: "Read a local file".into(),
        input_schema: json!({"type":"object", "properties":{"path":{"type":"string"}}, "required":["path"], "additionalProperties":false}),
    };
    let capabilities = ModelCapabilities {
        context_window: 128_000,
        max_output: 8192,
        reasoning: true,
        images: true,
    };
    insta::assert_json_snapshot!(json!({
        "message": round_trip(&message),
        "tool_spec": round_trip(&spec),
        "model_id": round_trip(&ModelId("local/default".into())),
        "capabilities": round_trip(&capabilities),
    }));
    let schema = schemars::schema_for!(Message);
    assert!(schema.definitions.contains_key("Content"));
}

#[test]
fn opaque_replay_data_keeps_its_exact_bytes() {
    let bytes = " \n{\"signature\":  \"AA==\",\"sequence\":[2, 1]}\t ";
    let message = Message {
        role: Role::Assistant,
        content: vec![
            Content::Thinking {
                text: String::new(),
                redacted: false,
                opaque: Some(OpaqueData(bytes.into())),
            },
            Content::Text {
                text: "answer".into(),
                opaque: Some(OpaqueData(bytes.into())),
            },
            Content::ToolCall(ToolCall {
                opaque: Some(OpaqueData(bytes.into())),
                ..call()
            }),
        ],
        opaque: Some(OpaqueData(bytes.into())),
    };
    let encoded = serde_json::to_string(&message).unwrap();
    let decoded: Message = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, message);
    assert_eq!(decoded.opaque.unwrap().0.as_bytes(), bytes.as_bytes());
    insta::assert_json_snapshot!(round_trip(&message));
}
