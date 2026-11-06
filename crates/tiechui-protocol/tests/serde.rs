use std::fmt::Debug;

use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tiechui_protocol::{rpc::*, *};

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) -> Value {
    let json = serde_json::to_value(value).expect("fixture serializes");
    let decoded: T = serde_json::from_value(json.clone()).expect("fixture deserializes");
    assert_eq!(&decoded, value);
    json
}

fn input() -> Vec<InputContent> {
    vec![
        InputContent::Text {
            text: "hello 世界".into(),
        },
        InputContent::Image {
            mime_type: "image/png".into(),
            data: "AA==".into(),
        },
    ]
}

fn item() -> Item {
    Item::Known(KnownItem::AssistantMessage {
        id: ItemId("item-1".into()),
        text: "hello".into(),
    })
}

fn turn() -> Turn {
    Turn {
        id: TurnId("turn-1".into()),
        status: TurnStatus::InProgress,
        items: vec![item()],
    }
}

fn event(event: EventKind) -> Event {
    Event {
        submission_id: Some(SubmissionId("submission-1".into())),
        thread_id: ThreadId("thread-1".into()),
        epoch: SessionEpoch(7),
        event,
    }
}

#[test]
fn structured_command_panel_round_trips() {
    let snapshot = ContextSnapshot {
        model: "fixture/model".into(),
        window: 200_000,
        messages: vec![ContextItem {
            label: "user #1".into(),
            text: "hello".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    for view in [
        ContextView::Overview,
        ContextView::System,
        ContextView::Tools,
        ContextView::Injections,
        ContextView::Messages,
        ContextView::Cache,
    ] {
        round_trip(&event(EventKind::CommandPanel {
            name: "context".into(),
            panel: Box::new(snapshot.clone()),
            view,
            text: "Summary only".into(),
        }));
    }
}

#[test]
fn input_and_approval_variants() {
    insta::assert_json_snapshot!(json!({
        "input": input().iter().map(round_trip).collect::<Vec<_>>(),
        "decisions": ([
            ApprovalDecision::AllowOnce,
            ApprovalDecision::AllowSession { both_directions: true },
            ApprovalDecision::Deny { reason: Some("use git diff".into()) },
            ApprovalDecision::Accept { content: Some(json!({"name": "x"})) },
            ApprovalDecision::Decline,
            ApprovalDecision::Cancel,
        ].iter().map(round_trip).collect::<Vec<_>>()),
        "delivery": ([InputDelivery::Steer, InputDelivery::FollowUp].iter().map(round_trip).collect::<Vec<_>>()),
    }));
}

#[test]
fn questions_and_retry_round_trip() {
    let request = ApprovalRequest::Questions {
        questions: vec![Question {
            id: "detail".into(),
            header: "Detail".into(),
            question: "What should change?".into(),
            options: None,
        }],
    };
    assert_eq!(round_trip(&request)["kind"], "questions");
    assert_eq!(round_trip(&Op::Retry), json!({"kind":"retry"}));
    assert_eq!(
        round_trip(&EventKind::RetryAvailable { available: true }),
        json!({"kind":"retry_available","available":true})
    );
}

#[test]
fn operation_variants() {
    let values = [
        Op::UserInput {
            content: input(),
            delivery: InputDelivery::Steer,
        },
        Op::UserInput {
            content: input(),
            delivery: InputDelivery::FollowUp,
        },
        Op::Interrupt {
            turn_id: TurnId("turn-1".into()),
        },
        Op::SetModel {
            provider: "test".into(),
            model: "test/model".into(),
        },
        Op::SetThinking {
            level: ThinkingLevel::High,
        },
        Op::SetThinking {
            level: ThinkingLevel::Off,
        },
        Op::Branch {
            entry_id: "entry-1".into(),
        },
        Op::Reload,
        Op::ApprovalResponse {
            approval_id: ApprovalId("approval-1".into()),
            decision: ApprovalDecision::AllowOnce,
        },
    ];
    insta::assert_json_snapshot!(values.iter().map(round_trip).collect::<Vec<_>>());
}

#[test]
fn turn_status_variants() {
    let values = [
        TurnStatus::InProgress,
        TurnStatus::Completed,
        TurnStatus::Interrupted,
        TurnStatus::Failed,
    ];
    insta::assert_json_snapshot!(values.iter().map(round_trip).collect::<Vec<_>>());
}

#[test]
fn thinking_levels_are_typed_lowercase_operations() {
    for (level, name) in [
        (ThinkingLevel::Off, "off"),
        (ThinkingLevel::Minimal, "minimal"),
        (ThinkingLevel::Low, "low"),
        (ThinkingLevel::Medium, "medium"),
        (ThinkingLevel::High, "high"),
        (ThinkingLevel::Xhigh, "xhigh"),
        (ThinkingLevel::Max, "max"),
    ] {
        assert_eq!(round_trip(&level), json!(name));
        assert_eq!(
            round_trip(&Op::SetThinking { level }),
            json!({"kind":"set_thinking", "level":name})
        );
    }
    for level in [json!("invalid"), Value::Null, json!("High")] {
        assert!(
            serde_json::from_value::<Op>(json!({"kind":"set_thinking", "level":level})).is_err()
        );
    }
}

#[test]
fn known_item_variants() {
    let values = [
        KnownItem::UserMessage {
            id: ItemId("user-1".into()),
            content: input(),
        },
        KnownItem::AssistantMessage {
            id: ItemId("assistant-1".into()),
            text: "answer".into(),
        },
        KnownItem::Thinking {
            id: ItemId("thinking-1".into()),
            text: "checking".into(),
        },
        KnownItem::ToolCall {
            id: ItemId("call-1".into()),
            name: "read".into(),
            arguments: json!({"path":"fixture.txt"}),
        },
        KnownItem::ToolResult {
            id: ItemId("result-1".into()),
            call_id: ItemId("call-1".into()),
            output: json!({"text":"file content"}),
            is_error: false,
        },
    ];
    let values: Vec<_> = values
        .into_iter()
        .map(|value| {
            round_trip(&value);
            round_trip(&Item::Known(value))
        })
        .collect();
    insta::assert_json_snapshot!(values);
}

#[test]
fn unknown_item_kind_survives_unchanged() {
    let original = json!({
        "kind": "future.widget",
        "id": "future-1",
        "payload": {"entries":[null, false, "opaque"], "revision": 18446744073709551615u64},
        "extra": {"unknown": true},
    });
    let item: Item = serde_json::from_value(original.clone()).unwrap();
    match &item {
        Item::Custom { kind, payload } => {
            assert_eq!(kind, "future.widget");
            assert_eq!(payload["id"], "future-1");
            assert_eq!(payload["payload"], original["payload"]);
        }
        Item::Known(_) => panic!("future kind must remain custom"),
    }
    assert_eq!(round_trip(&item), original);
    insta::assert_json_snapshot!(round_trip(&item));
}

#[test]
fn future_fields_on_known_items_are_not_discarded() {
    let original =
        json!({"kind":"assistant_message", "id":"item-1", "text":"hello", "future_field": [1,2]});
    let item: Item = serde_json::from_value(original.clone()).unwrap();
    assert!(matches!(item, Item::Custom { .. }));
    assert_eq!(round_trip(&item), original);
}

#[test]
fn event_variants() {
    let values = [
        EventKind::ThreadStarted {
            replay: false,
            thread: Thread {
                id: ThreadId("thread-1".into()),
                epoch: SessionEpoch(7),
                turns: vec![],
            },
        },
        EventKind::TurnStarted { turn: turn() },
        EventKind::ItemStarted {
            turn_id: TurnId("turn-1".into()),
            item: item(),
        },
        EventKind::ItemDelta {
            turn_id: TurnId("turn-1".into()),
            item_id: ItemId("item-1".into()),
            delta: " world".into(),
        },
        EventKind::ItemCompleted {
            turn_id: TurnId("turn-1".into()),
            item: item(),
        },
        EventKind::TokenUsageUpdated {
            turn_id: TurnId("turn-1".into()),
            last: TokenUsage {
                input: 17,
                output: 5,
                cache_read: 11,
                cache_write: 3,
                ..Default::default()
            },
            total: TokenUsage {
                input: 170,
                output: 50,
                cache_read: 110,
                cache_write: 30,
                ..Default::default()
            },
        },
        EventKind::TurnCompleted {
            turn: Turn {
                status: TurnStatus::Completed,
                ..turn()
            },
        },
        EventKind::ApprovalRequested {
            turn_id: Some(TurnId("turn-1".into())),
            approval_id: ApprovalId("approval-1".into()),
            request: ApprovalRequest::Permission {
                actor: None,
                call_id: "call-1".into(),
                tool: "bash".into(),
                arguments: json!({"command": "rm -rf build"}),
                surface: "bash".into(),
                values: vec!["rm -rf build".into()],
                matched_pattern: Some("rm *".into()),
                origin: "user".into(),
                session_patterns: vec!["rm *".into()],
            },
        },
        EventKind::ApprovalRequested {
            turn_id: None,
            approval_id: ApprovalId("approval-2".into()),
            request: ApprovalRequest::Elicitation {
                server: "docs".into(),
                request: json!({"message": "Pick one", "requestedSchema": {"type": "object", "properties": {}}}),
            },
        },
        EventKind::ApprovalResolved {
            approval_id: ApprovalId("approval-2".into()),
        },
        EventKind::Error {
            message: "fixture failure".into(),
        },
    ];
    let values: Vec<_> = values
        .into_iter()
        .map(|kind| {
            round_trip(&kind);
            round_trip(&event(kind))
        })
        .collect();
    insta::assert_json_snapshot!(values);
}

#[test]
fn protocol_message_variants() {
    let values = [
        ProtocolMessage::Submission(Submission {
            id: SubmissionId("submission-1".into()),
            thread_id: ThreadId("thread-1".into()),
            epoch: SessionEpoch(7),
            op: Op::UserInput {
                content: input(),
                delivery: InputDelivery::FollowUp,
            },
        }),
        ProtocolMessage::Event(Event {
            submission_id: None,
            ..event(EventKind::TurnStarted { turn: turn() })
        }),
        ProtocolMessage::JsonRpc(JsonRpcMessage::Notification(Notification {
            jsonrpc: Version::V2,
            method: "event".into(),
            params: None,
        })),
    ];
    insta::assert_json_snapshot!(values.iter().map(round_trip).collect::<Vec<_>>());
}

#[test]
fn rpc_version_id_and_params_variants() {
    let ids = [Id::String("request-1".into()), Id::Number(42), Id::Null];
    let params = [
        Params::Array(vec![json!("hello"), json!(2)]),
        Params::Object(serde_json::from_value(json!({"thread_id":"thread-1"})).unwrap()),
    ];
    insta::assert_json_snapshot!(json!({
        "version": round_trip(&Version::V2),
        "ids": ids.iter().map(round_trip).collect::<Vec<_>>(),
        "params": params.iter().map(round_trip).collect::<Vec<_>>(),
    }));
}

#[test]
fn rpc_envelope_variants() {
    let values = [
        JsonRpcMessage::Request(Request {
            jsonrpc: Version::V2,
            id: Id::String("request-1".into()),
            method: "submit".into(),
            params: Some(Params::Array(vec![json!("hello")])),
        }),
        JsonRpcMessage::Notification(Notification {
            jsonrpc: Version::V2,
            method: "event".into(),
            params: Some(Params::Object(
                serde_json::from_value(json!({"epoch":7})).unwrap(),
            )),
        }),
        JsonRpcMessage::Success(Success {
            jsonrpc: Version::V2,
            id: Id::Number(42),
            result: Value::Null,
        }),
        JsonRpcMessage::Failure(Failure {
            jsonrpc: Version::V2,
            id: Id::Null,
            error: Error {
                code: -32600,
                message: "Invalid Request".into(),
                data: Some(Value::Null),
            },
        }),
    ];
    insta::assert_json_snapshot!(values.iter().map(round_trip).collect::<Vec<_>>());
}

#[test]
fn rpc_envelopes_reject_invalid_shapes() {
    for invalid in [
        json!({"jsonrpc":"1.0","id":1,"method":"submit"}),
        json!({"id":1,"method":"submit"}),
        json!({"jsonrpc":"2.0","id":1,"method":"submit","params":5}),
        json!({"jsonrpc":"2.0","id":1,"method":"submit","params":null}),
        json!({"jsonrpc":"2.0","id":1,"result":null,"error":{"code":-1,"message":"bad"}}),
        json!({"jsonrpc":"2.0","result":null}),
        json!({"jsonrpc":"2.0","id":1}),
    ] {
        assert!(
            serde_json::from_value::<JsonRpcMessage>(invalid.clone()).is_err(),
            "{invalid}"
        );
    }
    let request: JsonRpcMessage =
        serde_json::from_value(json!({"jsonrpc":"2.0","id":null,"method":"submit"})).unwrap();
    assert!(matches!(
        request,
        JsonRpcMessage::Request(Request { id: Id::Null, .. })
    ));
    let notification: JsonRpcMessage =
        serde_json::from_value(json!({"jsonrpc":"2.0","method":"event"})).unwrap();
    assert!(matches!(notification, JsonRpcMessage::Notification(_)));
}
