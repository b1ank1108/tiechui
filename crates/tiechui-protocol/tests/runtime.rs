use serde_json::json;
use tiechui_protocol::*;

#[test]
fn native_capabilities_allow_every_gated_entry() {
    let native = RuntimeCapabilities::native();
    for op in [
        Op::Retry,
        Op::Compact { instructions: None },
        Op::RunShell {
            command: "ls".into(),
        },
    ] {
        assert_eq!(native.unsupported_op(&op), None);
    }
    for method in [
        "session/branch",
        "session/fork",
        "session/rewind",
        "btw/ask",
    ] {
        assert_eq!(native.unsupported_method(method), None);
    }
}

#[test]
fn a_runtime_lacking_a_capability_refuses_exactly_its_entries() {
    let caps = RuntimeCapabilities {
        commands: true,
        ..RuntimeCapabilities::minimal()
    };
    assert_eq!(caps.unsupported_op(&Op::Retry), Some("retry"));
    assert_eq!(
        caps.unsupported_op(&Op::RunCommand {
            name: "x".into(),
            arguments: String::new()
        }),
        None
    );
    assert_eq!(
        caps.unsupported_op(&Op::SetThinking {
            level: ThinkingLevel::High
        }),
        Some("set_thinking")
    );
    assert_eq!(caps.unsupported_op(&Op::Reload), None);
    assert_eq!(
        caps.unsupported_method("session/fork"),
        Some("session/fork")
    );
    assert_eq!(caps.unsupported_method("btw/open"), Some("btw/open"));
    assert_eq!(caps.unsupported_method("turn/undo"), None);
    assert_eq!(caps.unsupported_method("session/list"), None);
}

#[test]
fn capabilities_serialize_with_a_lowercase_steering_mode() {
    let value = serde_json::to_value(RuntimeCapabilities::minimal()).unwrap();
    assert_eq!(value["steering"], "inject");
    assert_eq!(value["branch"], false);
    let queue: RuntimeCapabilities = serde_json::from_value(json!({
        "steering": "queue", "branch": false, "fork": false, "rewind": false, "retry": false,
        "compact": false, "side_chat": false, "commands": false, "thinking": false,
        "model": false, "shell": false,
    }))
    .unwrap();
    assert_eq!(queue.steering, SteeringMode::Queue);
}

#[test]
fn subagent_and_derived_turn_items_are_custom_items_that_round_trip() {
    let child = SubagentItem {
        id: ItemId("a1:text-1".into()),
        parent_call_id: ItemId("spawn-a1".into()),
        agent_id: "a1".into(),
        item: KnownItem::AssistantMessage {
            id: ItemId("a1:text-1".into()),
            text: "hi".into(),
        },
    }
    .into_item();
    let wire = serde_json::to_value(&child).unwrap();
    assert_eq!(wire["kind"], SUBAGENT_ITEM_KIND);
    assert_eq!(wire["id"], "a1:text-1");
    assert_eq!(wire["parent_call_id"], "spawn-a1");
    assert_eq!(wire["item"]["kind"], "assistant_message");
    let decoded: Item = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded, child);
    assert_eq!(SubagentItem::from_item(&decoded).unwrap().agent_id, "a1");
    assert_eq!(SubagentStatus::from_item(&decoded), None);

    let status = SubagentStatus {
        id: SubagentStatus::item_id("a1"),
        parent_call_id: ItemId("spawn-a1".into()),
        agent_id: "a1".into(),
        name: None,
        state: SubagentState::Waiting,
        message: None,
    }
    .into_item();
    let wire = serde_json::to_value(&status).unwrap();
    assert_eq!(
        wire,
        json!({"kind": "runtime.subagent_status", "id": "subagent-status:a1",
            "parent_call_id": "spawn-a1", "agent_id": "a1", "state": "waiting"})
    );

    let marker = DerivedTurnMarker {
        id: ItemId("t-derived".into()),
        derived_from: TurnId("t0".into()),
        reason: DerivedReason::Subagent,
    }
    .into_item();
    assert_eq!(
        serde_json::to_value(&marker).unwrap(),
        json!({"kind": "runtime.derived_turn", "id": "t-derived",
            "derived_from": "t0", "reason": "subagent"})
    );
    assert!(DerivedTurnMarker::from_item(&marker).is_some());
}

#[test]
fn open_params_and_snapshots_from_servers_without_runtimes_decode() {
    let params: app::SessionOpenParams = serde_json::from_value(json!({"cwd": "/w"})).unwrap();
    assert_eq!(params.runtime, None);
    let snapshot: app::Snapshot = serde_json::from_value(json!({
        "thread_id": "t", "path": "/p", "cwd": "/w", "seq": 0, "events": [],
    }))
    .unwrap();
    assert_eq!(snapshot.runtime, None);
    assert_eq!(snapshot.capabilities, None);
    let wire = serde_json::to_value(&snapshot).unwrap();
    assert!(wire.get("runtime").is_none() && wire.get("capabilities").is_none());
}
