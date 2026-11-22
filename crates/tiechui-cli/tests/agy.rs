#![allow(clippy::unwrap_used)]

mod agy_support;

use agy_support::{AgyFixture, Client, runtime_entry, snapshot};
use serde_json::json;
use tiechui_protocol::{
    ApprovalRequest, DerivedReason, DerivedTurnMarker, EventKind, Item, KnownItem,
    PERMISSION_MODE_EVENT, SteeringMode, SubagentState, SubagentStatus, Turn, TurnStatus,
};

async fn submit_text(client: &mut Client, thread_id: &str, text: &str) {
    client
        .ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "user_input",
                    "delivery": "follow_up",
                    "content": [{ "kind": "text", "text": text }]
                }
            }),
        )
        .await;
}

fn turns_from_snapshot(snap: &tiechui_protocol::app::Snapshot) -> Vec<Turn> {
    snap.events
        .iter()
        .find_map(|ev| match &ev.event {
            EventKind::ThreadStarted { thread, .. } => Some(thread.turns.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn runtimes_list_checks_agy_version_gate_and_leaves_conversations_untouched() {
    let fixture = AgyFixture::new();
    let conv_dir = fixture
        .root
        .path()
        .join(".gemini/antigravity-cli/conversations");
    std::fs::create_dir_all(&conv_dir).unwrap();

    let mut missing = fixture.client_without_agy();
    let list = missing.ok("runtimes/list", json!({})).await;
    let agy_missing = runtime_entry(&list, "agy");
    assert_eq!(agy_missing["label"], "Antigravity");
    assert_eq!(agy_missing["available"], false);
    assert!(
        agy_missing["reason"]
            .as_str()
            .unwrap()
            .contains("not on PATH"),
        "{agy_missing}"
    );
    let _ = missing.finish().await;

    fixture.write_scenario(&json!({ "cliVersion": "1.5.1" }));
    let mut old_client = fixture.client_with(
        &[
            ("GEMINI_API_KEY", "secret-gemini"),
            ("GOOGLE_API_KEY", "secret-google"),
        ],
        true,
    );
    let old_list = old_client.ok("runtimes/list", json!({})).await;
    let agy_old = runtime_entry(&old_list, "agy");
    assert_eq!(agy_old["available"], false, "{agy_old}");
    assert_eq!(agy_old["version"], "1.5.1");
    assert_eq!(
        agy_old["path"],
        fixture.agy_binary_path().display().to_string()
    );
    let old_reason = agy_old["reason"].as_str().unwrap();
    assert!(
        old_reason.contains("1.5.1") && old_reason.contains("需要 1.5.2 及以上"),
        "{old_reason}"
    );
    let _ = old_client.finish().await;

    fixture.write_scenario(&json!({ "cliVersion": "1.5.14" }));
    let mut ok_client = fixture.client_with(
        &[
            ("GEMINI_API_KEY", "secret-gemini"),
            ("GOOGLE_API_KEY", "secret-google"),
        ],
        true,
    );
    let ok_list = ok_client.ok("runtimes/list", json!({})).await;
    let agy_ok = runtime_entry(&ok_list, "agy");
    assert_eq!(agy_ok["available"], true, "{agy_ok}");
    assert_eq!(agy_ok["version"], "1.5.14");
    assert_eq!(
        agy_ok["path"],
        fixture.agy_binary_path().display().to_string()
    );
    assert_eq!(agy_ok["capabilities"]["steering"], "queue");
    assert_eq!(agy_ok["capabilities"]["commands"], true);
    assert_eq!(agy_ok["capabilities"]["model"], true);
    assert_eq!(agy_ok["capabilities"]["thinking"], false);
    assert_eq!(agy_ok["capabilities"]["branch"], false);
    let _ = ok_client.finish().await;

    let entries: Vec<_> = std::fs::read_dir(&conv_dir).unwrap().collect();
    assert!(
        entries.is_empty(),
        "probe touched conversations: {entries:?}"
    );
    let spawns: Vec<_> = fixture
        .logs()
        .into_iter()
        .filter(|e| e["kind"] == "spawn")
        .collect();
    assert!(!spawns.is_empty());
    for s in spawns {
        assert_eq!(s["argv"], json!(["--version"]));
        assert_eq!(s["env"]["AGY_CLI_DISABLE_AUTO_UPDATE"], "true");
        assert!(s["env"]["GEMINI_API_KEY"].is_null());
        assert!(s["env"]["GOOGLE_API_KEY"].is_null());
    }
}

#[tokio::test]
async fn initialize_rejects_agent_info_version_below_minimum() {
    let fixture = AgyFixture::new();
    fixture.write_scenario(&json!({
        "cliVersion": "1.5.14",
        "agentVersion": "1.5.1",
    }));
    let mut client = fixture.client();
    let err = client
        .call(
            "session/open",
            json!({ "cwd": fixture.cwd, "runtime": "agy" }),
        )
        .await
        .unwrap_err();
    let msg = err["message"].as_str().unwrap_or_default();
    assert!(
        msg.contains("1.5.1") && msg.contains("需要 1.5.2 及以上"),
        "{err}"
    );
    let _ = client.finish().await;
}

#[tokio::test]
async fn permission_modes_map_to_acp_and_yolo_restarts_process_and_resumes_without_duplication() {
    let fixture = AgyFixture::new();
    fixture.write_scenario(&json!({
        "sessionId": "agy-sess-modes",
        "useGlobalTurnCounter": true,
        "resumeError": { "code": -32601, "message": "Method not found" },
        "loadReplayBefore": [
            { "update": { "sessionUpdate": "user_message_chunk", "content": { "type": "text", "text": "turn one" } } },
            { "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "replayed answer" } } }
        ],
        "turns": [
            {
                "steps": [
                    {
                        "permission": {
                            "toolCall": {
                                "toolCallId": "call_touch_ok",
                                "title": "touch approved.txt",
                                "kind": "execute",
                                "rawInput": { "CommandLine": "touch approved.txt" }
                            },
                            "options": [
                                { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
                                { "optionId": "deny_once", "name": "Deny", "kind": "reject_once" }
                            ]
                        },
                        "echoDecision": true
                    },
                    {
                        "permission": {
                            "toolCall": {
                                "toolCallId": "call_touch_no",
                                "title": "touch denied.txt",
                                "kind": "execute",
                                "rawInput": { "CommandLine": "touch denied.txt" }
                            },
                            "options": [
                                { "optionId": "allow_once", "name": "Allow", "kind": "allow_once" },
                                { "optionId": "deny_once", "name": "Deny", "kind": "reject_once" }
                            ]
                        },
                        "echoDecision": true
                    }
                ]
            },
            {
                "steps": [
                    {
                        "notify": {
                            "params": {
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": { "type": "text", "text": "yolo turn done" }
                                }
                            }
                        }
                    }
                ]
            }
        ]
    }));

    let mut client = fixture.client();
    let opened = snapshot(
        client
            .ok(
                "session/open",
                json!({ "cwd": fixture.cwd, "runtime": "agy", "mode": "rules" }),
            )
            .await,
    );
    assert_eq!(opened.runtime.as_deref(), Some("agy"));
    assert_eq!(
        opened.capabilities.as_ref().map(|c| c.steering),
        Some(SteeringMode::Queue)
    );
    let thread_id = opened.thread_id.0.clone();

    submit_text(&mut client, &thread_id, "turn one").await;

    let first_approval = client
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let first_id = first_approval
        .iter()
        .find_map(|e| match &e.event.event {
            EventKind::ApprovalRequested { approval_id, .. } => Some(approval_id.0.clone()),
            _ => None,
        })
        .unwrap();
    client
        .ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": { "kind": "approval_response", "approval_id": first_id, "decision": { "kind": "allow_once" } }
            }),
        )
        .await;

    let second_approval = client
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let second_id = second_approval
        .iter()
        .find_map(|e| match &e.event.event {
            EventKind::ApprovalRequested { approval_id, .. } => Some(approval_id.0.clone()),
            _ => None,
        })
        .unwrap();
    client
        .ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": { "kind": "approval_response", "approval_id": second_id, "decision": { "kind": "deny" } }
            }),
        )
        .await;

    client
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;

    client
        .ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": { "kind": "run_command", "name": "permissions", "arguments": "auto" }
            }),
        )
        .await;
    client
        .until(|e| {
            matches!(
                e,
                EventKind::Extension { name, data }
                if name == PERMISSION_MODE_EVENT && data == "auto"
            )
        })
        .await;

    let spawns_before_yolo: Vec<_> = fixture
        .logs()
        .into_iter()
        .filter(|e| e["kind"] == "spawn" && e["argv"][0] == "acp")
        .collect();
    assert_eq!(spawns_before_yolo.len(), 1);
    assert_eq!(spawns_before_yolo[0]["argv"], json!(["acp"]));

    client
        .ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": { "kind": "run_command", "name": "permissions", "arguments": "yolo" }
            }),
        )
        .await;
    client
        .until(|e| {
            matches!(
                e,
                EventKind::Extension { name, data }
                if name == PERMISSION_MODE_EVENT && data == "yolo"
            )
        })
        .await;

    submit_text(&mut client, &thread_id, "turn two").await;
    client
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;

    let snap = snapshot(
        client
            .ok("session/snapshot", json!({ "thread_id": thread_id }))
            .await,
    );
    let turns = turns_from_snapshot(&snap);
    assert_eq!(
        turns.len(),
        2,
        "history must not duplicate on yolo restart: {turns:?}"
    );
    let all_text: String = turns
        .iter()
        .flat_map(|t| &t.items)
        .filter_map(|i| match i {
            Item::Known(KnownItem::AssistantMessage { text, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!all_text.contains("replayed answer"), "{all_text}");
    assert!(all_text.contains("yolo turn done"), "{all_text}");

    client
        .ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": { "kind": "run_command", "name": "permissions", "arguments": "rules" }
            }),
        )
        .await;
    client
        .until(|e| {
            matches!(
                e,
                EventKind::Extension { name, data }
                if name == PERMISSION_MODE_EVENT && data == "rules"
            )
        })
        .await;

    let logs = fixture.logs();
    let acp_spawns: Vec<_> = logs
        .iter()
        .filter(|e| e["kind"] == "spawn" && e["argv"][0] == "acp")
        .collect();
    assert_eq!(acp_spawns.len(), 3, "{acp_spawns:?}");
    assert_eq!(acp_spawns[0]["argv"], json!(["acp"]));
    assert_eq!(
        acp_spawns[1]["argv"],
        json!(["acp", "--dangerously-skip-permissions"])
    );
    assert_eq!(acp_spawns[2]["argv"], json!(["acp"]));

    let set_modes: Vec<_> = logs
        .iter()
        .filter(|e| e["kind"] == "inbound" && e["msg"]["method"] == "session/set_mode")
        .map(|e| e["msg"]["params"]["modeId"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        set_modes.contains(&"accept-edits".to_owned()),
        "{set_modes:?}"
    );

    let _ = client.finish().await;
}

#[tokio::test]
async fn interaction_permissions_and_form_elicitations_become_question_cards_and_return_answers() {
    let fixture = AgyFixture::new();
    fixture.write_scenario(&json!({
        "turns": [
            {
                "steps": [
                    {
                        "permission": {
                            "toolCall": {
                                "toolCallId": "interaction_9960062f",
                                "title": "Which branch strategy?"
                            },
                            "options": [
                                { "optionId": "opt_rebase", "name": "(Recommended) Rebase", "kind": "allow_once" },
                                { "optionId": "opt_merge", "name": "Merge commit", "kind": "allow_once" }
                            ]
                        },
                        "echoDecision": true
                    },
                    {
                        "elicitation": {
                            "mode": "form",
                            "message": "The agent has a question.",
                            "toolCallId": "call_ask_1",
                            "requestedSchema": {
                                "type": "object",
                                "properties": {
                                    "q1": {
                                        "type": "string",
                                        "title": "Pick one",
                                        "oneOf": [
                                            { "const": "1", "title": "(Recommended) Blue" },
                                            { "const": "2", "title": "Green" }
                                        ]
                                    },
                                    "q1_other": {
                                        "type": "string",
                                        "title": "Other",
                                        "description": "Provide a custom answer if none of the options fit"
                                    }
                                }
                            }
                        },
                        "echoDecision": true
                    },
                    {
                        "elicitation": {
                            "mode": "form",
                            "message": "The agent has a question.",
                            "toolCallId": "call_ask_2",
                            "requestedSchema": {
                                "type": "object",
                                "properties": {
                                    "q1": {
                                        "type": "string",
                                        "title": "Pick one or write in",
                                        "oneOf": [
                                            { "const": "1", "title": "Alpha" },
                                            { "const": "2", "title": "Beta" }
                                        ]
                                    },
                                    "q1_other": {
                                        "type": "string",
                                        "title": "Other"
                                    }
                                }
                            }
                        },
                        "echoDecision": true
                    }
                ]
            }
        ]
    }));

    let mut client = fixture.client();
    let opened = snapshot(
        client
            .ok(
                "session/open",
                json!({ "cwd": fixture.cwd, "runtime": "agy" }),
            )
            .await,
    );
    let thread_id = opened.thread_id.0;
    submit_text(&mut client, &thread_id, "ask me").await;

    let ev1 = client
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let (id1, qid1) = ev1
        .iter()
        .find_map(|e| match &e.event.event {
            EventKind::ApprovalRequested {
                approval_id,
                request: ApprovalRequest::Questions { questions },
                ..
            } => Some((approval_id.0.clone(), questions[0].id.clone())),
            _ => None,
        })
        .expect("interaction_ permission must map to Questions");
    client
        .ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "approval_response",
                    "approval_id": id1,
                    "decision": {
                        "kind": "accept",
                        "content": { "answers": { qid1: { "answers": ["Rebase"] } } }
                    }
                }
            }),
        )
        .await;

    let ev2 = client
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let (id2, q2) = ev2
        .iter()
        .find_map(|e| match &e.event.event {
            EventKind::ApprovalRequested {
                approval_id,
                request: ApprovalRequest::Questions { questions },
                ..
            } => Some((approval_id.0.clone(), questions.clone())),
            _ => None,
        })
        .expect("form elicitation must map to Questions");
    assert_eq!(q2.len(), 1, "q1_other companion folded into q1: {q2:?}");
    client
        .ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "approval_response",
                    "approval_id": id2,
                    "decision": {
                        "kind": "accept",
                        "content": { "answers": { "q1": { "answers": ["Blue"] } } }
                    }
                }
            }),
        )
        .await;

    let ev3 = client
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let id3 = ev3
        .iter()
        .find_map(|e| match &e.event.event {
            EventKind::ApprovalRequested {
                approval_id,
                request: ApprovalRequest::Questions { .. },
                ..
            } => Some(approval_id.0.clone()),
            _ => None,
        })
        .unwrap();
    client
        .ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "approval_response",
                    "approval_id": id3,
                    "decision": {
                        "kind": "accept",
                        "content": { "answers": { "q1": { "answers": ["custom write-in"] } } }
                    }
                }
            }),
        )
        .await;

    client
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;

    let logs = fixture.logs();
    let perm_reply = logs
        .iter()
        .find(|e| e["kind"] == "permission_reply")
        .unwrap();
    assert_eq!(
        perm_reply["reply"]["outcome"],
        json!({ "outcome": "selected", "optionId": "opt_rebase" })
    );
    let elicit_replies: Vec<_> = logs
        .iter()
        .filter(|e| e["kind"] == "elicitation_reply")
        .collect();
    assert_eq!(elicit_replies.len(), 2);
    assert_eq!(
        elicit_replies[0]["reply"],
        json!({ "action": "accept", "content": { "q1": "1" } })
    );
    assert_eq!(
        elicit_replies[1]["reply"],
        json!({ "action": "accept", "content": { "q1_other": "custom write-in" } })
    );
    let _ = client.finish().await;
}

#[tokio::test]
async fn unauthenticated_401_errors_show_login_guidance_without_stack_traces() {
    let fixture = AgyFixture::new();
    fixture.write_scenario(&json!({
        "newError": {
            "code": -32603,
            "message": "Internal error: {\"error\":{\"code\":401,\"message\":\"Request had invalid authentication credentials.\",\"status\":\"UNAUTHENTICATED\"}}\n    at Object.createSession (/dist/server.js:412:19)\n    at processTicksAndRejections (node:internal/process/task_queues:95:5)"
        }
    }));
    let mut client = fixture.client();
    let open_err = client
        .call(
            "session/open",
            json!({ "cwd": fixture.cwd, "runtime": "agy" }),
        )
        .await
        .unwrap_err();
    let msg = open_err["message"].as_str().unwrap_or_default();
    assert!(msg.contains("请在终端运行 agy"), "{open_err}");
    assert!(msg.contains("完成登录"), "{open_err}");
    assert!(!msg.contains("at Object.createSession"), "{open_err}");
    assert!(!msg.contains("UNAUTHENTICATED"), "{open_err}");
    let _ = client.finish().await;

    fixture.write_scenario(&json!({
        "turns": [
            {
                "steps": [
                    {
                        "notify": {
                            "params": {
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": {
                                        "type": "text",
                                        "text": "⚠ UNAUTHENTICATED (code 401): OAuth token expired\n    at StreamRunner.step (/dist/runner.js:88:11)"
                                    }
                                }
                            }
                        }
                    }
                ]
            }
        ]
    }));
    let mut client2 = fixture.client();
    let opened = snapshot(
        client2
            .ok(
                "session/open",
                json!({ "cwd": fixture.cwd, "runtime": "agy" }),
            )
            .await,
    );
    let thread_id = opened.thread_id.0;
    submit_text(&mut client2, &thread_id, "hi").await;
    let turn_events = client2
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    let err_msg = turn_events
        .iter()
        .find_map(|e| match &e.event.event {
            EventKind::Error { message } => Some(message.clone()),
            _ => None,
        })
        .expect("401 chunk must emit Error");
    assert!(err_msg.contains("请在终端运行 agy"), "{err_msg}");
    assert!(err_msg.contains("完成登录"), "{err_msg}");
    assert!(!err_msg.contains("StreamRunner"), "{err_msg}");
    assert_eq!(
        turn_events.last().and_then(|e| match &e.event.event {
            EventKind::TurnCompleted { turn } => Some(turn.status),
            _ => None,
        }),
        Some(TurnStatus::Failed)
    );
    let _ = client2.finish().await;
}

#[tokio::test]
async fn child_env_scrubs_google_keys_sets_disable_auto_update_and_filters_logout_command() {
    let fixture = AgyFixture::new();
    fixture.write_scenario(&json!({
        "commands": [
            { "name": "plan", "description": "Create plan" },
            { "name": "logout", "description": "Sign out" },
            { "name": "/LOGOUT", "description": "Sign out upper" },
            { "name": "goal", "description": "Run until done", "input": { "hint": "<task>" } }
        ]
    }));
    let mut client = fixture.client_with(
        &[
            ("GEMINI_API_KEY", "leak-gemini"),
            ("GOOGLE_API_KEY", "leak-google"),
        ],
        true,
    );
    let opened = snapshot(
        client
            .ok(
                "session/open",
                json!({ "cwd": fixture.cwd, "runtime": "agy" }),
            )
            .await,
    );
    let thread_id = opened.thread_id.0;
    let cmd_events = client
        .until(|e| matches!(e, EventKind::CommandsUpdated { commands } if !commands.is_empty()))
        .await;
    let names: Vec<String> = cmd_events
        .iter()
        .filter_map(|e| match &e.event.event {
            EventKind::CommandsUpdated { commands } if !commands.is_empty() => {
                Some(commands.iter().map(|c| c.name.clone()).collect::<Vec<_>>())
            }
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(names, vec!["plan", "goal"]);

    client
        .ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": { "kind": "run_command", "name": "logout", "arguments": "" }
            }),
        )
        .await;
    let err_ev = client.until(|e| matches!(e, EventKind::Error { .. })).await;
    assert!(
        err_ev.iter().any(|e| matches!(
            &e.event.event,
            EventKind::Error { message } if message.contains("logout")
        )),
        "{err_ev:?}"
    );

    let acp_spawn = fixture
        .logs()
        .into_iter()
        .find(|e| e["kind"] == "spawn" && e["argv"][0] == "acp")
        .unwrap();
    assert_eq!(acp_spawn["env"]["AGY_CLI_DISABLE_AUTO_UPDATE"], "true");
    assert!(acp_spawn["env"]["GEMINI_API_KEY"].is_null());
    assert!(acp_spawn["env"]["GOOGLE_API_KEY"].is_null());
    let _ = client.finish().await;
}

#[tokio::test]
async fn models_fallback_set_model_and_config_update_preserve_native_config() {
    let fixture = AgyFixture::new();
    let config_before = std::fs::read_to_string(fixture.home.join("config.toml")).unwrap();
    fixture.write_scenario(&json!({
        "sessionId": "agy-sess-models",
        "omitSessionModels": true,
        "cliModels": [
            "gemini-4-neon-high\tGemini 4 Neon (High)",
            "gemini-3.8-flash-low\tGemini 3.8 Flash (Low)",
            "claude-opus-5-5-high\tClaude Opus 5.5 (High)"
        ]
    }));

    let mut client = fixture.client();
    let opened = snapshot(
        client
            .ok(
                "session/open",
                json!({ "cwd": fixture.cwd, "runtime": "agy" }),
            )
            .await,
    );
    let thread_id = opened.thread_id.0.clone();
    let session_path = opened.path.clone();

    let model_events = client
        .until(|e| matches!(e, EventKind::ModelsUpdated { models } if models.len() >= 3))
        .await;
    let listed_ids: Vec<String> = model_events
        .iter()
        .filter_map(|e| match &e.event.event {
            EventKind::ModelsUpdated { models } if models.len() >= 3 => {
                Some(models.iter().map(|m| m.id.clone()).collect::<Vec<_>>())
            }
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(
        listed_ids,
        vec![
            "gemini-4-neon-high",
            "gemini-3.8-flash-low",
            "claude-opus-5-5-high"
        ]
    );

    client
        .ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": { "kind": "set_model", "provider": "agy", "model": "gemini-3.8-flash-low" }
            }),
        )
        .await;
    client
        .until(|e| {
            matches!(
                e,
                EventKind::SessionUpdated { model, .. } if model.id == "gemini-3.8-flash-low"
            )
        })
        .await;

    client
        .ok(
            "config/update",
            json!({ "key": "model", "value": "agy/gemini-3.8-flash-low" }),
        )
        .await;
    assert_eq!(
        std::fs::read_to_string(fixture.home.join("config.toml")).unwrap(),
        config_before,
        "external runtime model selection must not overwrite native config.toml"
    );
    let bogus_err = client
        .call(
            "config/update",
            json!({ "key": "model", "value": "undeclared_provider/foo" }),
        )
        .await
        .unwrap_err();
    assert!(
        bogus_err["message"]
            .as_str()
            .unwrap_or_default()
            .contains("is not declared"),
        "{bogus_err}"
    );

    client
        .ok("session/close", json!({ "thread_id": thread_id }))
        .await;

    fixture.write_scenario(&json!({
        "sessionId": "agy-sess-models",
        "resumeModels": {
            "currentModelId": "gemini-4-neon-high",
            "availableModels": [
                { "modelId": "gemini-4-neon-high", "name": "Gemini 4 Neon (High)" },
                { "modelId": "gemini-3.8-flash-low", "name": "Gemini 3.8 Flash (Low)" }
            ]
        }
    }));

    let reopened = snapshot(
        client
            .ok("session/open", json!({ "path": session_path }))
            .await,
    );
    let reopened_model = reopened
        .events
        .iter()
        .find_map(|ev| match &ev.event {
            EventKind::SessionUpdated { model, .. } => Some(model.id.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(reopened_model, "gemini-3.8-flash-low");

    let set_model_calls: Vec<_> = fixture
        .logs()
        .into_iter()
        .filter(|e| e["kind"] == "inbound" && e["msg"]["method"] == "session/set_model")
        .map(|e| e["msg"]["params"]["modelId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        set_model_calls,
        vec!["gemini-3.8-flash-low", "gemini-3.8-flash-low"]
    );
    let _ = client.finish().await;
}

#[tokio::test]
async fn normalizes_real_agy_tools_subagent_cards_and_reactive_wakeup_across_reopen() {
    let fixture = AgyFixture::new();
    let abs_hello = fixture.cwd.join("src/hello.ts").display().to_string();
    let abs_new = fixture.cwd.join("src/created.ts").display().to_string();
    fixture.write_scenario(&json!({
        "sessionId": "agy-sess-tools",
        "turns": [
            {
                "steps": [
                    {
                        "notify": {
                            "params": {
                                "update": {
                                    "sessionUpdate": "tool_call",
                                    "toolCallId": "call_read_1",
                                    "name": "read_file",
                                    "title": format!("Read {abs_hello}"),
                                    "kind": "read",
                                    "status": "completed",
                                    "locations": [{ "path": abs_hello, "line": 1 }],
                                    "rawInput": { "Path": abs_hello, "StartLine": 1, "EndLine": 5 },
                                    "content": [{ "type": "content", "content": { "type": "text", "text": "1: export const x = 1;" } }]
                                }
                            }
                        }
                    },
                    {
                        "notify": {
                            "params": {
                                "update": {
                                    "sessionUpdate": "tool_call",
                                    "toolCallId": "call_edit_1",
                                    "name": "edit_file",
                                    "title": format!("Edit {abs_hello}"),
                                    "kind": "edit",
                                    "status": "completed",
                                    "locations": [{ "path": abs_hello }],
                                    "rawInput": { "Path": abs_hello, "Target": "x = 1", "Replacement": "x = 2" },
                                    "rawOutput": {},
                                    "content": [{ "type": "diff", "path": abs_hello, "oldText": "export const x = 1;\n", "newText": "export const x = 2;\n" }]
                                }
                            }
                        }
                    },
                    {
                        "notify": {
                            "params": {
                                "update": {
                                    "sessionUpdate": "tool_call",
                                    "toolCallId": "call_write_1",
                                    "name": "write_file",
                                    "title": format!("Write {abs_new}"),
                                    "kind": "edit",
                                    "status": "completed",
                                    "locations": [{ "path": abs_new }],
                                    "rawInput": { "Path": abs_new, "Content": "export const y = 3;\n", "Overwrite": true },
                                    "rawOutput": {},
                                    "content": [{ "type": "diff", "path": abs_new, "oldText": "", "newText": "export const y = 3;\n" }]
                                }
                            }
                        }
                    },
                    {
                        "notify": {
                            "params": {
                                "update": {
                                    "sessionUpdate": "tool_call",
                                    "toolCallId": "call_cmd_1",
                                    "name": "run_command",
                                    "title": "printf 'AGY_OK\\n'",
                                    "kind": "execute",
                                    "status": "completed",
                                    "rawInput": { "CommandLine": "printf 'AGY_OK\\n'", "WaitMsBeforeAsync": 5000, "toolSummary": "Print ok" },
                                    "rawOutput": "Created At: 2026-11-22T13:09:49+08:00\nCompleted At: 2026-11-22T13:09:49+08:00\nCommand exited with code 0.\n\nCommand output:\nAGY_OK\r\n",
                                    "content": [{ "type": "content", "content": { "type": "text", "text": "Created At: 2026-11-22T13:09:49+08:00\nCompleted At: 2026-11-22T13:09:49+08:00\nCommand exited with code 0.\n\nCommand output:\nAGY_OK\r\n" } }]
                                }
                            }
                        }
                    },
                    {
                        "notify": {
                            "params": {
                                "update": {
                                    "sessionUpdate": "tool_call",
                                    "toolCallId": "call_sub_1",
                                    "name": "invoke_subagent",
                                    "title": "Run invoke_subagent",
                                    "kind": "other",
                                    "status": "in_progress",
                                    "rawInput": { "Prompt": "Reply PONG_SUB", "Role": "Tiny Ping", "TypeName": "research" }
                                }
                            }
                        }
                    },
                    {
                        "notify": {
                            "params": {
                                "update": {
                                    "sessionUpdate": "tool_call_update",
                                    "toolCallId": "call_sub_1",
                                    "status": "completed",
                                    "rawOutput": "PONG_SUB",
                                    "content": [{ "type": "content", "content": { "type": "text", "text": "PONG_SUB" } }]
                                }
                            }
                        }
                    },
                    {
                        "notify": {
                            "params": {
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": { "type": "text", "text": "All done." }
                                }
                            }
                        }
                    }
                ],
                "afterEndTurn": [
                    { "sleepMs": 40 },
                    {
                        "notify": {
                            "params": {
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": { "type": "text", "text": "Background wakeup output." }
                                }
                            }
                        }
                    }
                ]
            }
        ]
    }));

    let mut client = fixture.client();
    let opened = snapshot(
        client
            .ok(
                "session/open",
                json!({ "cwd": fixture.cwd, "runtime": "agy" }),
            )
            .await,
    );
    let thread_id = opened.thread_id.0.clone();
    let session_path = opened.path.clone();

    submit_text(&mut client, &thread_id, "run tools and subagent").await;

    let first_turn_events = client
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    let saw_running_subagent = first_turn_events.iter().any(|e| match &e.event.event {
        EventKind::ItemCompleted { item, .. } => serde_json::to_value(item)
            .ok()
            .is_some_and(|v| v["kind"] == "runtime.subagent_status" && v["state"] == "running"),
        _ => false,
    });
    assert!(saw_running_subagent, "subagent must report running first");

    client
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;

    let live_snap = snapshot(
        client
            .ok("session/snapshot", json!({ "thread_id": thread_id }))
            .await,
    );
    let live_turns = turns_from_snapshot(&live_snap);
    assert_eq!(live_turns.len(), 2, "{live_turns:?}");

    let root_items = &live_turns[0].items;
    let find_call = |id: &str| {
        root_items.iter().find_map(|item| match item {
            Item::Known(KnownItem::ToolCall {
                id: cid,
                name,
                arguments,
            }) if cid.0 == id => Some((name.as_str(), arguments)),
            _ => None,
        })
    };
    let find_result = |call_id: &str| {
        root_items.iter().find_map(|item| match item {
            Item::Known(KnownItem::ToolResult {
                call_id: cid,
                output,
                is_error,
                ..
            }) if cid.0 == call_id => Some((output, *is_error)),
            _ => None,
        })
    };

    let (read_name, read_args) = find_call("call_read_1").unwrap();
    assert_eq!(read_name, "read");
    assert_eq!(read_args["path"], "src/hello.ts");

    let (edit_name, edit_args) = find_call("call_edit_1").unwrap();
    assert_eq!(edit_name, "edit");
    assert_eq!(edit_args["path"], "src/hello.ts");
    let (edit_out, edit_err) = find_result("call_edit_1").unwrap();
    assert!(!edit_err);
    assert_eq!(edit_out["content"][0]["text"], "");

    let (write_name, write_args) = find_call("call_write_1").unwrap();
    assert_eq!(write_name, "write");
    assert_eq!(write_args["path"], "src/created.ts");
    assert_eq!(write_args["content"], "export const y = 3;\n");

    let (bash_name, bash_args) = find_call("call_cmd_1").unwrap();
    assert_eq!(bash_name, "bash");
    assert_eq!(bash_args["command"], "printf 'AGY_OK\\n'");
    let (bash_out, bash_err) = find_result("call_cmd_1").unwrap();
    assert!(!bash_err);
    assert_eq!(bash_out["content"][0]["text"], "AGY_OK");

    let sub_status: SubagentStatus = root_items
        .iter()
        .find_map(|item| {
            let val = serde_json::to_value(item).ok()?;
            (val["kind"] == "runtime.subagent_status")
                .then(|| serde_json::from_value(val).ok())
                .flatten()
        })
        .expect("subagent status item must exist");
    assert_eq!(sub_status.parent_call_id.0, "call_sub_1");
    assert_eq!(sub_status.name.as_deref(), Some("Tiny Ping"));
    assert_eq!(sub_status.state, SubagentState::Completed);
    assert_eq!(sub_status.message.as_deref(), Some("PONG_SUB"));

    let derived_marker: DerivedTurnMarker =
        serde_json::from_value(serde_json::to_value(&live_turns[1].items[0]).unwrap()).unwrap();
    assert_eq!(derived_marker.derived_from, live_turns[0].id);
    assert_eq!(derived_marker.reason, DerivedReason::Runtime);

    client
        .ok("session/close", json!({ "thread_id": thread_id }))
        .await;
    let reopened = snapshot(
        client
            .ok("session/open", json!({ "path": session_path }))
            .await,
    );
    let reopened_turns = turns_from_snapshot(&reopened);
    assert_eq!(reopened_turns.len(), 2);
    let shape = |turns: &[Turn]| {
        turns
            .iter()
            .map(|t| {
                (
                    t.status,
                    t.items
                        .iter()
                        .map(|i| {
                            serde_json::to_value(i).unwrap()["kind"]
                                .as_str()
                                .unwrap()
                                .to_owned()
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&reopened_turns), shape(&live_turns));

    let _ = client.finish().await;
}
