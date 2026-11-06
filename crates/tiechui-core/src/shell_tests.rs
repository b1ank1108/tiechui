use super::*;
use tiechui_plugin_api::{Module, protocol::SHELLS_EVENT};

async fn shells(scripts: Vec<Script>) -> Harness {
    Harness::with_contributions(
        scripts,
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| {
            tiechui_module_exec::ExecModule { background: true }
                .register(registry, &Default::default())
                .unwrap();
        },
    )
    .await
}

fn bash(id: &str, arguments: serde_json::Value) -> Content {
    Content::ToolCall(ToolCall {
        id: id.into(),
        name: "bash".into(),
        arguments,
        opaque: None,
    })
}

fn settled(event: &EventKind, id: &str, state: &str) -> bool {
    matches!(event, EventKind::Extension { name, data } if name == SHELLS_EVENT
        && data["tasks"].as_array().is_some_and(|tasks| tasks.iter().any(|t| t["id"] == id && t["state"] == state)))
}

#[tokio::test]
async fn background_completion_wakes_the_model_once_after_the_turn() {
    let mut harness = shells(vec![
        reply(vec![bash(
            "start",
            json!({"command":"sleep 0.3; printf ok","background":true}),
        )]),
        answer("started"),
        answer("noted"),
    ])
    .await;
    harness.input("serve", InputDelivery::FollowUp).await;
    harness.request().await;
    let second = harness.request().await;
    assert!(
        matches!(&results(&second)[0].content[0], Content::Text { text, .. } if text.starts_with("Started background shell sh1"))
    );
    harness.end(TurnStatus::Completed).await;
    let third = harness.request().await;
    let notice = users(&third).pop().unwrap();
    assert!(
        notice.starts_with("A background shell task finished:\n- sh1 (exit 0 · "),
        "{notice}"
    );
    harness.end(TurnStatus::Completed).await;
    assert!(
        harness
            .seen
            .iter()
            .any(|event| settled(&event.event, "sh1", "completed"))
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(harness.requests.try_recv().is_err(), "notified once");
    harness.valid_transcript().await;
    harness.stop().await;
}

#[tokio::test]
async fn user_shell_moves_to_the_background_and_session_end_reaps_it() {
    let mut harness = shells(vec![answer("ready")]).await;
    // Startup hooks drive the actor; one turn guarantees `!` arrives while idle.
    harness.input("hi", InputDelivery::FollowUp).await;
    harness.request().await;
    harness.end(TurnStatus::Completed).await;
    let pid = harness._root.path().join("pid");
    harness
        .send(Op::RunShell {
            command: format!("printf $$ > '{}'; exec sleep 30", pid.display()),
        })
        .await;
    harness.started_turn().await;
    for _ in 0..500 {
        if std::fs::read_to_string(&pid).is_ok_and(|text| !text.is_empty()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The turn is busy in the foreground command; the control path does not wait for it.
    harness
        .send(Op::RunCommand {
            name: "shells".into(),
            arguments: "background sh1".into(),
        })
        .await;
    harness.end(TurnStatus::Completed).await;
    // The snapshot follows on the next runtime tick.
    let moved = |event: &EventKind| {
        settled(event, "sh1", "running")
            && matches!(event, EventKind::Extension { data, .. }
                if data["tasks"][0]["mode"] == "background" && data["tasks"][0]["user"] == true)
    };
    if !harness.seen.iter().any(|event| moved(&event.event)) {
        harness.until(moved).await;
    }
    assert!(harness.seen.iter().any(|event| matches!(&event.event,
        EventKind::ItemCompleted { item: Item::Known(KnownItem::ToolResult { output, .. }), .. }
        if output["content"][0]["text"].as_str().is_some_and(|text| text.contains("moved this command to the background as sh1")))));
    let pid: i32 = std::fs::read_to_string(&pid).unwrap().parse().unwrap();
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    assert!(harness.requests.try_recv().is_err());
    harness.stop().await;
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "session end reaps the group"
    );
}
