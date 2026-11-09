use super::*;
use tiechui_config::{ModelConfig, ProviderConfig, Thinking, Wire};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

const WIRES: [Wire; 2] = [Wire::AnthropicMessages, Wire::OpenaiCompletions];
/// The wires whose replayed requests the encoding tests read.
const ENCODERS: [Wire; 3] = [
    Wire::AnthropicMessages,
    Wire::OpenaiCompletions,
    Wire::OpenaiResponses,
];

#[tokio::test]
async fn context_inspection_preserves_http_request_bodies_on_both_wires() {
    use tiechui_plugin_api::{Module, ModuleConfig};
    for wire in WIRES {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(plain(wire), "text/event-stream"))
            .expect(4)
            .mount(&server)
            .await;
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        for inspect in [false, true] {
            let store =
                SessionStore::create(root.path(), root.path(), &CancellationToken::new()).unwrap();
            let thread_id = ThreadId(store.header().id.clone());
            let mut cfg = config(provider(wire, &server.uri()));
            cfg.registry
                .register_context_provider(Arc::new(StaticContext(vec![
                    "Unchanged instructions".into(),
                ])));
            tiechui_module_context::ContextModule
                .register(&mut cfg.registry, &ModuleConfig::default())
                .unwrap();
            let shutdown = CancellationToken::new();
            let mut handle = SessionActor::spawn(store, cfg, shutdown.clone());
            while !matches!(event(&mut handle).await, EventKind::CommandsUpdated { .. }) {}
            for prompt in ["first", "second"] {
                if inspect {
                    handle
                        .submissions
                        .send(Submission {
                            id: SubmissionId("inspect".into()),
                            thread_id: thread_id.clone(),
                            epoch: SessionEpoch(0),
                            op: Op::RunCommand {
                                name: "context".into(),
                                arguments: "injections".into(),
                            },
                        })
                        .await
                        .unwrap();
                    while !matches!(event(&mut handle).await, EventKind::CommandPanel { .. }) {}
                }
                handle
                    .submissions
                    .send(Submission {
                        id: SubmissionId(prompt.into()),
                        thread_id: thread_id.clone(),
                        epoch: SessionEpoch(0),
                        op: Op::UserInput {
                            content: vec![InputContent::Text {
                                text: prompt.into(),
                            }],
                            delivery: InputDelivery::FollowUp,
                        },
                    })
                    .await
                    .unwrap();
                while !matches!(event(&mut handle).await, EventKind::TurnCompleted { .. }) {}
            }
            shutdown.cancel();
            handle.task.await.unwrap().unwrap();
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].body, requests[2].body);
        assert_eq!(requests[1].body, requests[3].body);
    }
}

#[tokio::test]
async fn priced_usage_and_legacy_entries_count_after_resume_on_both_wires() {
    for wire in WIRES {
        let server = MockServer::start().await;
        let body = plain(wire)
            .replace("\"id\":\"fixture_plain\"", "\"id\":\"fixture_plain\",\"model\":\"backup\"")
            .replace("\"cache_creation_input_tokens\":5", "\"cache_creation_input_tokens\":5,\"cache_creation\":{\"ephemeral_1h_input_tokens\":3},\"output_tokens_details\":{\"thinking_tokens\":1}")
            .replace("\"completion_tokens\":2", "\"completion_tokens\":2,\"completion_tokens_details\":{\"reasoning_tokens\":1}");
        responses(
            &server,
            vec![ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")],
        )
        .await;
        let rates =
            serde_json::json!({"input":2.0,"output":10.0,"cache_read":0.2,"cache_write":2.5});
        let mut cfg: ProviderConfig = serde_json::from_value(serde_json::json!({
            "wire": wire, "base_url":server.uri(), "models":[
                {"id":"model","context_window":200000,"max_output":4096,"prices":rates},
                {"id":"backup","context_window":200000,"max_output":4096,"prices":rates}
            ]
        }))
        .unwrap();
        if wire == Wire::AnthropicMessages {
            cfg.models[0].prices.as_mut().unwrap().input = 200.0;
        }
        let adapter: Arc<dyn ModelProvider> = tiechui_providers::testing::provider("fixture", &cfg)
            .unwrap()
            .into();
        let (_root, store, events) = recovered_request(config(adapter.clone()), false).await;
        let expected = Usage {
            partial: false,
            input: 7,
            output: 2,
            cache_read: 11,
            cache_write: 5,
            reasoning: 1,
            cache_write_1h: if wire == Wire::AnthropicMessages {
                3
            } else {
                0
            },
            cost_nano_usd: Some(if wire == Wire::AnthropicMessages {
                53_200
            } else {
                48_700
            }),
        };
        assert!(events.iter().any(|event| matches!(event, EventKind::TokenUsageUpdated {total, ..} if *total == tokens(expected))));
        assert!(store.entries().iter().any(|entry| matches!(&entry.kind, EntryKind::Message {message, usage:Some(usage), ..} if message.role == Role::Assistant && *usage == expected)));
        // Legacy entries have neither model nor request identity nor the new usage fields.
        let legacy: EntryKind = serde_json::from_value(serde_json::json!({"kind":"usage", "usage":{"input":1,"output":2,"cache_read":3,"cache_write":4}})).unwrap();
        let path = store.path().to_owned();
        drop(store);
        let mut store = SessionStore::open(&path, &CancellationToken::new()).unwrap();
        store.append(legacy, &CancellationToken::new()).unwrap();
        drop(store);
        let store = SessionStore::open(&path, &CancellationToken::new()).unwrap();
        let shutdown = CancellationToken::new();
        let mut handle = SessionActor::spawn(store, config(adapter), shutdown.clone());
        let total = loop {
            if let EventKind::SessionUpdated { total, .. } = event(&mut handle).await {
                break total;
            }
        };
        assert_eq!(
            total,
            tokens(Usage {
                input: 8,
                output: 4,
                cache_read: 14,
                cache_write: 9,
                ..expected
            })
        );
        shutdown.cancel();
        handle.task.await.unwrap().unwrap();
    }
}

fn provider(wire: Wire, base: &str) -> Arc<dyn ModelProvider> {
    tiechui_providers::testing::provider(
        "fixture",
        &ProviderConfig {
            wire,
            base_url: Some(base.into()),
            api_key: None,
            api_key_env: None,
            compat: Default::default(),
            models: vec![ModelConfig {
                prices: None,
                cache_retention: Default::default(),
                fallback_models: vec![],
                id: "model".into(),
                context_window: 200_000,
                max_output: 4096,
                reasoning: false,
                thinking: Thinking::Budget,
                thinking_levels: None,
                compat: None,
                images: true,
            }],
        },
    )
    .unwrap()
    .into()
}

fn fixture_model() -> ModelRef {
    ModelRef {
        provider: "fixture".into(),
        model: ModelId("fixture/model".into()),
    }
}

fn plain(wire: Wire) -> &'static str {
    match wire {
        Wire::AnthropicMessages => {
            include_str!("../../tiechui-providers/tests/fixtures/anthropic/plain_text.sse")
        }
        Wire::OpenaiCompletions => {
            include_str!("../../tiechui-providers/tests/fixtures/openai-completions/plain_text.sse")
        }
        Wire::OpenaiResponses => {
            include_str!("../../tiechui-providers/tests/fixtures/openai-responses/plain_text.sse")
        }
    }
}

async fn encode(wire: Wire, messages: Vec<Message>) -> serde_json::Value {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(plain(wire), "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let events: Vec<_> = provider(wire, &server.uri())
        .stream(
            ModelRequest {
                disable_cache: false,
                cache_key: None,
                input_tokens: None,
                model: fixture_model().model,
                messages,
                tools: vec![],
                reasoning: None,
                max_output: None,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .collect()
        .await;
    assert!(events.iter().all(Result::is_ok));
    serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap()
}

fn load(name: &str) -> (tempfile::TempDir, SessionStore) {
    let fixture = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/fixtures/replay/{name}.jsonl")),
    )
    .unwrap();
    let (header, entries) = fixture.split_once('\n').unwrap();
    let mut header: serde_json::Value = serde_json::from_str(header).unwrap();
    let temp = tempfile::tempdir().unwrap();
    header["cwd"] = json!(temp.path().canonicalize().unwrap());
    let path = temp.path().join("session.jsonl");
    std::fs::write(&path, format!("{header}\n{entries}")).unwrap();
    let store = SessionStore::open_read_only(&path, &CancellationToken::new()).unwrap();
    (temp, store)
}

#[tokio::test]
async fn interrupted_disk_sessions_encode_on_each_wire() {
    for wire in ENCODERS {
        let adapter = provider(wire, "http://127.0.0.1:1");
        for (name, expected) in [
            ("esc-stream", vec![]),
            ("error-stream", vec![]),
            ("esc-tool", vec![("a", "Operation aborted")]),
            ("killed-before-result", vec![("a", "No result provided")]),
            (
                "parallel-crash",
                vec![
                    ("a", "contents a"),
                    ("b", "No result provided"),
                    ("c", "No result provided"),
                ],
            ),
            (
                "systems-and-orphans",
                vec![("b", "contents b"), ("a", "contents a")],
            ),
        ] {
            let (_temp, store) = load(name);
            let messages = replay::prepare(
                store.build_context_with_origins(&HashSet::new()),
                &fixture_model(),
                true,
                adapter.error_tool_images(),
                |id| adapter.normalize_tool_call_id(id),
            );
            if name == "systems-and-orphans" {
                assert_eq!(
                    messages.iter().map(|m| m.role).collect::<Vec<_>>(),
                    [
                        Role::User,
                        Role::Assistant,
                        Role::Tool,
                        Role::Tool,
                        Role::System,
                        Role::System
                    ]
                );
            }
            let body = encode(wire, messages).await;
            let messages = body["messages"]
                .as_array()
                .or(body["input"].as_array())
                .unwrap();
            let results: Vec<_> = match wire {
                Wire::AnthropicMessages => messages
                    .iter()
                    .flat_map(|m| m["content"].as_array().unwrap())
                    .filter(|b| b["type"] == "tool_result")
                    .map(|b| {
                        (
                            b["tool_use_id"].as_str().unwrap(),
                            b["content"][0]["text"].as_str().unwrap(),
                        )
                    })
                    .collect(),
                Wire::OpenaiCompletions => messages
                    .iter()
                    .filter(|m| m["role"] == "tool")
                    .map(|m| {
                        (
                            m["tool_call_id"].as_str().unwrap(),
                            m["content"].as_str().unwrap(),
                        )
                    })
                    .collect(),
                Wire::OpenaiResponses => messages
                    .iter()
                    .filter(|m| m["type"] == "function_call_output")
                    .map(|m| {
                        (
                            m["call_id"].as_str().unwrap(),
                            m["output"].as_str().unwrap(),
                        )
                    })
                    .collect(),
            };
            assert_eq!(results, expected, "{name}: {body}");
            assert!(!body.to_string().contains("partial"));
            assert!(!body.to_string().contains("orphan result"));
            if matches!(name, "killed-before-result" | "parallel-crash")
                && let Wire::AnthropicMessages = wire
            {
                let blocks = messages.last().unwrap()["content"].as_array().unwrap();
                assert_eq!(blocks.last().unwrap()["is_error"], true);
            }
        }
    }
}

#[tokio::test]
async fn error_images_follow_all_results_on_each_wire() {
    for wire in ENCODERS {
        let adapter = provider(wire, "http://127.0.0.1:1");
        let (_temp, store) = load("error-images");
        let messages = replay::prepare(
            store.build_context_with_origins(&HashSet::new()),
            &fixture_model(),
            true,
            adapter.error_tool_images(),
            |id| adapter.normalize_tool_call_id(id),
        );
        let body = encode(wire, messages).await;
        let messages = body["messages"]
            .as_array()
            .or(body["input"].as_array())
            .unwrap();
        match wire {
            Wire::AnthropicMessages => {
                assert_eq!(messages.len(), 3);
                let blocks = messages[2]["content"].as_array().unwrap();
                assert_eq!(
                    blocks
                        .iter()
                        .map(|b| b["type"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["tool_result", "tool_result", "text", "image", "image"]
                );
                assert_eq!(
                    blocks[0]["content"][1]["text"],
                    "(image attached after the tool results)"
                );
                assert_eq!(
                    blocks[2]["text"],
                    "Images from the error result of tool call a (read):"
                );
            }
            Wire::OpenaiCompletions => {
                assert_eq!(messages[2]["role"], "tool");
                assert_eq!(messages[3]["role"], "tool");
                assert_eq!(messages[4]["role"], "user");
                assert_eq!(messages[4]["content"][1]["type"], "image_url");
                assert_eq!(messages[4]["content"][2]["type"], "image_url");
            }
            // Function call outputs carry images, so they stay with the error result.
            Wire::OpenaiResponses => {
                assert_eq!(
                    messages[3]["output"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|part| part["type"].as_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["input_text", "input_image", "input_image"]
                );
                assert_eq!(messages[4]["output"], "contents b");
            }
        }
    }
}

fn config(adapter: Arc<dyn ModelProvider>) -> SessionConfig {
    SessionConfig {
        compaction: Default::default(),
        model: fixture_model(),
        thinking: None,
        providers: BTreeMap::from([("fixture".into(), adapter)]),
        registry: Registry::default(),
        retry: Arc::new(BoundedRetry {
            max_retries: 0,
            ..Default::default()
        }),
        interactions: None,
        epoch: SessionEpoch(0),
    }
}

async fn event(handle: &mut SessionHandle) -> EventKind {
    tokio::time::timeout(Duration::from_secs(5), handle.events.recv())
        .await
        .unwrap()
        .unwrap()
        .event
}

#[tokio::test]
async fn failed_request_resumes_manually_without_a_user_message_on_both_wires() {
    for wire in WIRES {
        let server = MockServer::start().await;
        responses(
            &server,
            vec![
                ResponseTemplate::new(400)
                    .set_body_json(json!({"error":{"message":"invalid request"}})),
                ResponseTemplate::new(200).set_body_raw(plain(wire), "text/event-stream"),
            ],
        )
        .await;
        let adapter = provider(wire, &server.uri());
        let (_root, store, events) = recovered_request(config(adapter.clone()), false).await;
        assert!(
            matches!(events.last(), Some(EventKind::TurnCompleted { turn }) if turn.status == TurnStatus::Failed)
        );
        let path = store.path().to_owned();
        drop(store);
        let cancel = CancellationToken::new();
        let mut store = SessionStore::open(&path, &cancel).unwrap();
        store
            .append(
                EntryKind::Custom {
                    kind: "ui.elapsed".into(),
                    data: json!(10),
                },
                &cancel,
            )
            .unwrap();
        let thread_id = ThreadId(store.header().id.clone());
        let mut handle = SessionActor::spawn(store, config(adapter), cancel.clone());
        loop {
            if let EventKind::RetryAvailable { available } = event(&mut handle).await {
                assert!(available);
                break;
            }
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
        handle
            .submissions
            .send(Submission {
                id: SubmissionId("retry".into()),
                thread_id,
                epoch: SessionEpoch(0),
                op: Op::Retry,
            })
            .await
            .unwrap();
        loop {
            if let EventKind::TurnCompleted { turn } = event(&mut handle).await {
                assert_eq!(turn.status, TurnStatus::Completed);
                break;
            }
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].body_json::<serde_json::Value>().unwrap()["messages"],
            requests[1].body_json::<serde_json::Value>().unwrap()["messages"]
        );
        cancel.cancel();
        handle.task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn context_clamping_uses_successful_usage_and_only_estimates_trailing_messages() {
    fn message_entry(
        role: Role,
        text: String,
        reason: Option<StopReason>,
        usage: Option<Usage>,
    ) -> EntryKind {
        EntryKind::Message {
            reasoning: None,
            message: Message {
                role,
                content: vec![text_content(text)],
                opaque: None,
            },
            model: (role == Role::Assistant).then(fixture_model),
            stop_reason: reason,
            usage,
            details: None,
        }
    }
    let usage = Usage {
        input: 5000,
        output: 100,
        cache_read: 200,
        cache_write: 300,
        ..Default::default()
    };
    for wire in WIRES {
        for (cjk, success, legacy, failed, empty_failure, expected) in [
            (false, true, false, None, false, 302),
            (true, true, false, None, false, 302),
            (true, true, true, None, false, 302),
            (true, false, false, None, false, 3044),
            (true, true, false, Some(StopReason::Error), false, 302),
            (true, true, false, Some(StopReason::Cancelled), false, 302),
            (true, false, false, Some(StopReason::Error), false, 3044),
            (true, false, false, Some(StopReason::Cancelled), false, 3044),
            (true, false, false, Some(StopReason::Error), true, 3043),
            (true, true, false, Some(StopReason::Error), true, 301),
        ] {
            let server = MockServer::start().await;
            responses(
                &server,
                vec![ResponseTemplate::new(200).set_body_raw(plain(wire), "text/event-stream")],
            )
            .await;
            let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            let cancel = CancellationToken::new();
            let mut store =
                SessionStore::create(&root.path().join("home"), root.path(), &cancel).unwrap();
            store
                .append(
                    message_entry(
                        Role::User,
                        if cjk {
                            "汉".repeat(10000)
                        } else {
                            "short".into()
                        },
                        None,
                        None,
                    ),
                    &cancel,
                )
                .unwrap();
            if success {
                store
                    .append(
                        EntryKind::Usage {
                            usage,
                            request_id: None,
                            model: Some(fixture_model()),
                        },
                        &cancel,
                    )
                    .unwrap();
                store
                    .append(
                        message_entry(
                            Role::Assistant,
                            "ok".into(),
                            Some(StopReason::EndTurn),
                            (!legacy).then_some(usage),
                        ),
                        &cancel,
                    )
                    .unwrap();
            }
            if let Some(reason) = failed {
                let failed_usage = Usage {
                    input: 9500,
                    ..Usage::default()
                };
                store
                    .append(
                        EntryKind::Usage {
                            usage: failed_usage,
                            request_id: None,
                            model: Some(fixture_model()),
                        },
                        &cancel,
                    )
                    .unwrap();
                store
                    .append(
                        message_entry(
                            Role::Assistant,
                            if empty_failure { "ok" } else { "failed" }.into(),
                            Some(if empty_failure {
                                StopReason::EndTurn
                            } else {
                                reason
                            }),
                            Some(if empty_failure {
                                Usage::default()
                            } else {
                                failed_usage
                            }),
                        ),
                        &cancel,
                    )
                    .unwrap();
            }
            let path = store.path().to_owned();
            drop(store);
            let store = SessionStore::open(&path, &cancel).unwrap();
            let thread_id = ThreadId(store.header().id.clone());
            let provider_config = serde_json::from_value(json!({
                "wire": wire, "base_url": server.uri(),
                "models": [{"id":"model", "context_window":10000, "max_output":4096}]
            }))
            .unwrap();
            let provider = tiechui_providers::testing::provider("fixture", &provider_config)
                .unwrap()
                .into();
            let mut handle = SessionActor::spawn(store, config(provider), cancel.clone());
            handle
                .submissions
                .send(Submission {
                    id: SubmissionId("trailing".into()),
                    thread_id,
                    epoch: SessionEpoch(0),
                    op: Op::UserInput {
                        content: vec![InputContent::Text {
                            text: "1234567".into(),
                        }],
                        delivery: InputDelivery::Steer,
                    },
                })
                .await
                .unwrap();
            loop {
                if let EventKind::TurnCompleted { turn } = event(&mut handle).await {
                    assert_eq!(turn.status, TurnStatus::Completed);
                    break;
                }
            }
            cancel.cancel();
            handle.task.await.unwrap().unwrap();
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 1);
            let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
            let field = if wire == Wire::AnthropicMessages {
                "max_tokens"
            } else {
                "max_completion_tokens"
            };
            assert_eq!(
                body[field], expected,
                "{wire:?}, cjk={cjk}, success={success}, legacy={legacy}, failed={failed:?}"
            );
            assert!(body.get("input_tokens").is_none());
            let saved = SessionStore::open_read_only(&path, &CancellationToken::new()).unwrap();
            assert!(matches!(
                &saved.entries().last().unwrap().kind,
                EntryKind::Message {
                    usage: Some(Usage {
                        input: 7,
                        output: 2,
                        cache_read: 11,
                        cache_write: 5,
                        ..
                    }),
                    ..
                }
            ));
        }
    }
}

async fn recovered_request(
    mut config: SessionConfig,
    interrupt: bool,
) -> (tempfile::TempDir, SessionStore, Vec<EventKind>) {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let cancel = CancellationToken::new();
    let store = SessionStore::create(&root.path().join("home"), root.path(), &cancel).unwrap();
    let path = store.path().to_owned();
    let thread_id = ThreadId(store.header().id.clone());
    config.retry = Arc::new(BoundedRetry {
        max_retries: 2,
        initial_delay: Duration::from_millis(if interrupt { 30000 } else { 1 }),
        max_delay: Duration::from_secs(60),
    });
    let mut handle = SessionActor::spawn(store, config, cancel.clone());
    handle
        .submissions
        .send(Submission {
            id: SubmissionId("input".into()),
            thread_id: thread_id.clone(),
            epoch: SessionEpoch(0),
            op: Op::UserInput {
                content: vec![InputContent::Text {
                    text: "hello".into(),
                }],
                delivery: InputDelivery::Steer,
            },
        })
        .await
        .unwrap();
    let mut events = Vec::new();
    loop {
        let next = event(&mut handle).await;
        if interrupt
            && let EventKind::RequestRetry {
                turn_id,
                delay_ms: Some(_),
                ..
            } = &next
        {
            handle
                .submissions
                .send(Submission {
                    id: SubmissionId("esc".into()),
                    thread_id: thread_id.clone(),
                    epoch: SessionEpoch(0),
                    op: Op::Interrupt {
                        turn_id: turn_id.clone(),
                    },
                })
                .await
                .unwrap();
        }
        let done = matches!(next, EventKind::TurnCompleted { .. });
        events.push(next);
        if done {
            break;
        }
    }
    cancel.cancel();
    handle.task.await.unwrap().unwrap();
    // Reopen the actual transcript, rather than inspecting actor memory.
    let store = SessionStore::open_read_only(&path, &CancellationToken::new()).unwrap();
    (root, store, events)
}

async fn responses(server: &MockServer, replies: Vec<ResponseTemplate>) {
    let replies = Mutex::new(VecDeque::from(replies));
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected retry")
        })
        .mount(server)
        .await;
}

#[tokio::test]
async fn both_wires_retry_http_classes_limits_and_interrupt_wait() {
    for wire in WIRES {
        for status in [408, 409, 429, 500, 502, 503, 504, 529] {
            let server = MockServer::start().await;
            let error = ResponseTemplate::new(status)
                .set_body_json(json!({"error":{"message":"temporary failure"}}))
                .insert_header("retry-after-ms", "2");
            responses(&server, vec![error.clone(), error.clone(), error]).await;
            let (_root, store, events) =
                recovered_request(config(provider(wire, &server.uri())), false).await;
            assert_eq!(server.received_requests().await.unwrap().len(), 3);
            let attempts: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    EventKind::RequestRetry {
                        attempt,
                        max_retries,
                        delay_ms: Some(ms),
                        ..
                    } => Some((*attempt, *max_retries, *ms)),
                    _ => None,
                })
                .collect();
            assert_eq!(attempts, [(1, Some(2), 2), (2, Some(2), 2)]);
            assert!(
                matches!(events.last(), Some(EventKind::TurnCompleted { turn }) if turn.status == TurnStatus::Failed)
            );
            assert_eq!(store.build_context_with_origins(&HashSet::new()).len(), 1);
        }
        for (status, code, retry_after) in [
            (401, "authentication_error", "0"),
            (400, "context_length_exceeded", "0"),
            (429, "insufficient_quota", "0"),
            (429, "rate_limit_error", "61000"),
        ] {
            let server = MockServer::start().await;
            responses(
                &server,
                vec![
                    ResponseTemplate::new(status)
                        .set_body_json(json!({"error":{"type":code,"message":code}}))
                        .insert_header("retry-after-ms", retry_after),
                ],
            )
            .await;
            let (_, _, events) =
                recovered_request(config(provider(wire, &server.uri())), false).await;
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, EventKind::RequestRetry { .. }))
            );
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
        }
        let server = MockServer::start().await;
        responses(&server, vec![ResponseTemplate::new(503)]).await;
        let start = Instant::now();
        let (_root, store, events) =
            recovered_request(config(provider(wire, &server.uri())), true).await;
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(
            matches!(events.last(), Some(EventKind::TurnCompleted { turn }) if turn.status == TurnStatus::Interrupted)
        );
        assert_eq!(store.build_context().len(), 1);
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn both_wires_recover_structured_gateway_errors_with_identical_context() {
    for wire in WIRES {
        let server = MockServer::start().await;
        responses(
            &server,
            vec![
                ResponseTemplate::new(400).set_body_json(json!({"error":{
                    "type":"upstream_error", "code":"upstream_400", "param":"",
                    "message":"400 · Request contains an invalid argument."
                }})),
                ResponseTemplate::new(200).set_body_raw(plain(wire), "text/event-stream"),
            ],
        )
        .await;
        let (_root, _store, events) =
            recovered_request(config(provider(wire, &server.uri())), false).await;
        assert!(events.iter().any(|event| matches!(
            event,
            EventKind::RequestRetry {
                attempt: 1,
                delay_ms: Some(2000),
                ..
            }
        )));
        assert!(
            matches!(events.last(), Some(EventKind::TurnCompleted { turn }) if turn.status == TurnStatus::Completed)
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].body_json::<serde_json::Value>().unwrap()["messages"],
            requests[1].body_json::<serde_json::Value>().unwrap()["messages"]
        );
    }
}

struct CountTool {
    spec: ToolSpec,
    runs: AtomicUsize,
}

#[async_trait]
impl Tool for CountTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &serde_json::Value) -> Concurrency {
        Concurrency::Shared
    }
    async fn run(&self, _: ToolCall, _: ToolContext) -> Result<ToolOutput, ToolError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text("completed once"))
    }
}

#[tokio::test]
async fn each_wire_recovers_stream_after_completed_tool_batch() {
    for wire in ENCODERS {
        let server = MockServer::start().await;
        let tool_response = match wire {
            Wire::AnthropicMessages => {
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"once\",\"name\":\"count\",\"input\":{}}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
            }
            Wire::OpenaiCompletions => {
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"once\",\"function\":{\"name\":\"count\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n"
            }
            Wire::OpenaiResponses => {
                "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"function_call\",\"call_id\":\"once\",\"name\":\"count\",\"arguments\":\"{}\"}}\n\ndata: {\"type\":\"response.completed\",\"response\":{}}\n\n"
            }
        };
        let failed = match wire {
            Wire::AnthropicMessages => plain(wire)
                .split("event: message_delta")
                .next()
                .unwrap()
                .to_owned(),
            Wire::OpenaiCompletions => {
                "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n".into()
            }
            Wire::OpenaiResponses => {
                "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"message\"}}\n\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"delta\":\"partial\"}\n\n".into()
            }
        };
        responses(
            &server,
            vec![tool_response.to_owned(), failed, plain(wire).into()]
                .into_iter()
                .map(|body| ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
                .collect(),
        )
        .await;
        let tool = Arc::new(CountTool {
            spec: ToolSpec {
                name: "count".into(),
                description: "Count executions".into(),
                input_schema: json!({"type":"object"}),
            },
            runs: AtomicUsize::new(0),
        });
        let mut config = config(provider(wire, &server.uri()));
        config.registry.register_tool(tool.clone()).unwrap();
        let (_root, store, events) = recovered_request(config, false).await;
        assert!(
            matches!(events.last(), Some(EventKind::TurnCompleted { turn }) if turn.status == TurnStatus::Completed)
        );
        assert_eq!(tool.runs.load(Ordering::SeqCst), 1);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].body, requests[2].body);
        assert!(String::from_utf8_lossy(&requests[2].body).contains("completed once"));
        assert!(store.entries().iter().any(|entry| matches!(&entry.kind, EntryKind::Custom { kind, .. } if kind == "request_omission")));
    }
}

#[tokio::test]
async fn fallback_model_attributes_assistant_and_all_usage_after_resume() {
    let server = MockServer::start().await;
    let body = plain(Wire::AnthropicMessages).replace(
        "\"id\":\"fixture_plain\"",
        "\"id\":\"fixture_plain\",\"model\":\"backup\"",
    );
    responses(
        &server,
        vec![ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")],
    )
    .await;
    let (_root, store, events) = recovered_request(
        config(provider(Wire::AnthropicMessages, &server.uri())),
        false,
    )
    .await;
    assert!(
        matches!(events.last(), Some(EventKind::TurnCompleted { turn }) if turn.status == TurnStatus::Completed)
    );
    let mut usage_count = 0;
    for entry in store.entries() {
        match &entry.kind {
            EntryKind::Usage { model, .. } => {
                usage_count += 1;
                assert_eq!(model.as_ref().unwrap().model.0, "fixture/backup");
            }
            EntryKind::Message { message, model, .. } if message.role == Role::Assistant => {
                assert_eq!(model.as_ref().unwrap().model.0, "fixture/backup");
            }
            _ => {}
        }
    }
    assert_eq!(usage_count, 2);
}

#[tokio::test]
async fn both_wires_keep_partial_usage_on_cut_and_cancel_without_counting_finish_twice() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (wire, zero_start) in [
        (Wire::AnthropicMessages, false),
        (Wire::OpenaiCompletions, false),
        (Wire::AnthropicMessages, true),
    ] {
        for outcome in [
            TurnStatus::Completed,
            TurnStatus::Failed,
            TurnStatus::Interrupted,
        ] {
            let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
            let cancel = CancellationToken::new();
            let store =
                SessionStore::create(&root.path().join("home"), root.path(), &cancel).unwrap();
            let path = store.path().to_owned();
            let thread_id = ThreadId(store.header().id.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let fixture = if zero_start {
                include_str!(
                    "../../tiechui-providers/tests/fixtures/anthropic/zero_initial_usage.sse"
                )
            } else {
                plain(wire)
            };
            let body = match outcome {
                TurnStatus::Completed => fixture,
                _ => match wire {
                    Wire::AnthropicMessages => fixture.split("event: message_stop").next().unwrap(),
                    Wire::OpenaiCompletions => fixture.split("data: [DONE]").next().unwrap(),
                    Wire::OpenaiResponses => {
                        unreachable!("Responses usage arrives only at completion")
                    }
                },
            }
            .to_owned();
            let (release, wait) = oneshot::channel();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.windows(4).any(|b| b == b"\r\n\r\n") {
                    let mut bytes = [0; 4096];
                    let count = socket.read(&mut bytes).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&bytes[..count]);
                }
                // A premature socket close is a transport failure, including on Completions
                // where a clean SSE EOF following finish_reason would complete the request.
                let length = body.len()
                    + if outcome == TurnStatus::Completed {
                        0
                    } else {
                        100
                    };
                socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}").as_bytes()).await.unwrap();
                let _ = wait.await;
            });
            let adapter = provider(wire, &base);
            let mut handle = SessionActor::spawn(store, config(adapter.clone()), cancel.clone());
            handle
                .submissions
                .send(Submission {
                    id: SubmissionId("input".into()),
                    thread_id: thread_id.clone(),
                    epoch: SessionEpoch(0),
                    op: Op::UserInput {
                        content: vec![InputContent::Text {
                            text: "hello".into(),
                        }],
                        delivery: InputDelivery::Steer,
                    },
                })
                .await
                .unwrap();
            let expected = tokens(Usage {
                input: 7,
                output: 2,
                cache_read: 11,
                cache_write: 5,
                ..Default::default()
            });
            let active_turn = loop {
                if let EventKind::TurnStarted { turn } = event(&mut handle).await {
                    break turn.id;
                }
            };
            // Accounting is durable before request completion, even though clients have
            // not yet received a usage event.
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let store =
                        SessionStore::open_read_only(&path, &CancellationToken::new()).unwrap();
                    if store.entries().iter().any(|entry| {
                        matches!(entry.kind,
                        EntryKind::Usage { usage, .. } if tokens(usage) == expected)
                    }) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            if outcome != TurnStatus::Completed {
                while let Ok(event) = handle.events.try_recv() {
                    assert!(!matches!(
                        event.event,
                        EventKind::TokenUsageUpdated { .. } | EventKind::TurnCompleted { .. }
                    ));
                }
            }
            if outcome == TurnStatus::Interrupted {
                handle
                    .submissions
                    .send(Submission {
                        id: SubmissionId("esc".into()),
                        thread_id,
                        epoch: SessionEpoch(0),
                        op: Op::Interrupt {
                            turn_id: active_turn,
                        },
                    })
                    .await
                    .unwrap();
            }
            let mut release = Some(release);
            if outcome == TurnStatus::Failed {
                release.take().unwrap().send(()).unwrap();
            }
            let mut usage_events = 0;
            loop {
                match event(&mut handle).await {
                    EventKind::TokenUsageUpdated { last, total, .. } => {
                        usage_events += 1;
                        assert_eq!(last, expected);
                        assert_eq!(total, expected);
                    }
                    EventKind::TurnCompleted { turn } => {
                        assert_eq!(turn.status, outcome);
                        break;
                    }
                    _ => {}
                }
            }
            assert_eq!(usage_events, 1);
            if let Some(release) = release {
                release.send(()).unwrap();
            }
            server.await.unwrap();
            cancel.cancel();
            handle.task.await.unwrap().unwrap();
            let store = SessionStore::open(&path, &CancellationToken::new()).unwrap();
            let snapshots: Vec<_> = store
                .entries()
                .iter()
                .filter_map(|entry| match &entry.kind {
                    EntryKind::Usage {
                        usage, request_id, ..
                    } => Some((usage, request_id)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                snapshots.len(),
                if matches!(wire, Wire::AnthropicMessages) && !zero_start {
                    2
                } else {
                    1
                }
            );
            assert!(
                snapshots
                    .iter()
                    .all(|(usage, _)| **usage != Usage::default())
            );
            assert!(snapshots[0].1.is_none());
            if snapshots.len() == 2 {
                assert!(snapshots[1].1.is_some());
            }
            let stop = CancellationToken::new();
            let mut resumed = SessionActor::spawn(store, config(adapter), stop.clone());
            loop {
                if let EventKind::SessionUpdated { context, total, .. } = event(&mut resumed).await
                {
                    assert_eq!(context, Some(expected));
                    assert_eq!(total, expected);
                    break;
                }
            }
            stop.cancel();
            resumed.task.await.unwrap().unwrap();
        }
    }
}

#[tokio::test]
async fn edit_and_write_details_survive_resume_but_never_enter_requests() {
    use tiechui_plugin_api::Module;
    let tool = |id: &str, name: &str, arguments| {
        reply(vec![Content::ToolCall(ToolCall {
            id: id.into(),
            name: name.into(),
            arguments,
            opaque: None,
        })])
    };
    let mut h = Harness::with_contributions(
        vec![
            tool(
                "write",
                "write",
                json!({"path":"sample.txt","content":"before\n"}),
            ),
            tool(
                "edit",
                "edit",
                json!({"path":"sample.txt","edits":[{"oldText":"before","newText":"after"}]}),
            ),
            answer("done"),
            answer("resumed"),
        ],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| {
            tiechui_module_editing::EditingModule
                .register(registry, &Default::default())
                .unwrap();
        },
    )
    .await;
    h.input("edit", InputDelivery::Steer).await;
    h.end(TurnStatus::Completed).await;
    let details: HashMap<_, _> = h
        .seen
        .iter()
        .filter_map(|event| match &event.event {
            EventKind::ItemCompleted {
                item:
                    Item::Known(KnownItem::ToolResult {
                        call_id, output, ..
                    }),
                ..
            } => Some((call_id.clone(), output["details"].clone())),
            _ => None,
        })
        .collect();
    assert_eq!(details.len(), 2);
    assert!(
        details[&ItemId("edit".into())]["diff"]
            .as_str()
            .unwrap()
            .contains("after")
    );
    h.send(Op::Reload).await;
    let initial = h
        .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
    h.epoch = initial.epoch;
    let EventKind::ThreadStarted { thread, .. } = initial.event else {
        unreachable!()
    };
    for item in thread.turns.iter().flat_map(|turn| &turn.items) {
        if let Item::Known(KnownItem::ToolResult {
            call_id, output, ..
        }) = item
        {
            assert_eq!(output["details"], details[call_id]);
        }
    }
    let store = h.snapshot().await;
    for entry in store.entries() {
        if let EntryKind::Message {
            message,
            details: stored_details,
            ..
        } = &entry.kind
            && let [Content::ToolResult(result)] = message.content.as_slice()
        {
            assert_eq!(
                serde_json::to_value(entry).unwrap()["details"],
                details[&ItemId(result.call_id.clone())]
            );
            assert_eq!(
                stored_details.as_ref(),
                details
                    .get(&ItemId(result.call_id.clone()))
                    .filter(|value| !value.is_null())
            );
        }
    }
    for wire in WIRES {
        let adapter = provider(wire, "http://127.0.0.1:1");
        let messages = replay::prepare(
            store.build_context_with_origins(&HashSet::new()),
            &fixture_model(),
            true,
            adapter.error_tool_images(),
            |id| adapter.normalize_tool_call_id(id),
        );
        let body = encode(wire, messages).await;
        assert!(!body.to_string().contains("\"details\""));
        assert!(!body.to_string().contains("\"diff\""));
    }
    // Old messages with no presentation metadata still produce a usable transcript.
    let (_temp, legacy) = load("esc-tool");
    assert!(legacy.entries().iter().all(|entry| !matches!(
        &entry.kind,
        EntryKind::Message {
            details: Some(_),
            ..
        }
    )));
    h.stop().await;
}

#[tokio::test]
async fn revised_usage_replaces_earlier_snapshot_in_live_and_restored_totals() {
    let first = Usage {
        input: 12,
        output: 0,
        cache_read: 18,
        cache_write: 7,
        cache_write_1h: 5,
        reasoning: 3,
        ..Default::default()
    };
    let final_usage = Usage {
        input: 12,
        output: 9,
        cache_read: 19,
        cache_write: 0,
        cache_write_1h: 0,
        reasoning: 4,
        ..Default::default()
    };
    let mut h = Harness::new(vec![Script::events(vec![
        StreamEvent::Usage {
            usage: Usage::default(),
        },
        StreamEvent::Usage { usage: first },
        StreamEvent::Usage {
            usage: Usage::default(),
        },
        StreamEvent::Usage { usage: first },
        StreamEvent::Usage { usage: final_usage },
        StreamEvent::Finish {
            reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ])])
    .await;
    h.input("task", InputDelivery::Steer).await;
    h.end(TurnStatus::Completed).await;
    let totals: Vec<_> = h
        .seen
        .iter()
        .filter_map(|event| match event.event {
            EventKind::TokenUsageUpdated { total, .. } => Some(total),
            _ => None,
        })
        .collect();
    assert_eq!(totals, [tokens(final_usage)]);
    let store = h.snapshot().await;
    let snapshots: Vec<_> = store
        .entries()
        .iter()
        .filter_map(|entry| match entry.kind {
            EntryKind::Usage { usage, .. } => Some(usage),
            _ => None,
        })
        .collect();
    assert_eq!(snapshots, [first, final_usage]);
    h.send(Op::Reload).await;
    let state = h
        .until(|event| matches!(event, EventKind::SessionUpdated { .. }))
        .await;
    let EventKind::SessionUpdated { total, context, .. } = state.event else {
        unreachable!()
    };
    assert_eq!(total, tokens(final_usage));
    assert_eq!(context, Some(tokens(final_usage)));
    h.stop().await;
}

#[tokio::test]
async fn all_zero_usage_creates_no_accounting_or_usage_event() {
    let mut h = Harness::new(vec![Script::events(vec![
        StreamEvent::Usage {
            usage: Usage::default(),
        },
        StreamEvent::Finish {
            reason: StopReason::EndTurn,
            usage: Usage::default(),
        },
    ])])
    .await;
    h.input("task", InputDelivery::Steer).await;
    h.end(TurnStatus::Completed).await;
    assert!(
        !h.seen
            .iter()
            .any(|event| matches!(event.event, EventKind::TokenUsageUpdated { .. }))
    );
    assert!(
        !h.snapshot()
            .await
            .entries()
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::Usage { .. }))
    );
    h.send(Op::Reload).await;
    let state = h
        .until(|event| matches!(event, EventKind::SessionUpdated { .. }))
        .await;
    let EventKind::SessionUpdated { context, total, .. } = state.event else {
        unreachable!()
    };
    assert_eq!(context, None);
    assert_eq!(total, tokens(Usage::default()));
    h.stop().await;
}

#[tokio::test]
async fn stream_errors_and_unsuccessful_finishes_emit_last_usage_once() {
    let usage = Usage {
        input: 7,
        output: 2,
        cache_read: 11,
        cache_write: 5,
        ..Default::default()
    };
    for (last, status) in [
        (
            Some(Step::Error(provider_error(
                ErrorKind::InvalidRequest,
                "stream error",
            ))),
            TurnStatus::Failed,
        ),
        (None, TurnStatus::Failed),
        (
            Some(Step::Event(StreamEvent::Finish {
                reason: StopReason::Cancelled,
                usage: Usage::default(),
            })),
            TurnStatus::Interrupted,
        ),
        (
            Some(Step::Event(StreamEvent::Finish {
                reason: StopReason::Error,
                usage: Usage::default(),
            })),
            TurnStatus::Failed,
        ),
    ] {
        let mut steps = VecDeque::from([Step::Event(StreamEvent::Usage { usage })]);
        steps.extend(last);
        let mut h = Harness::with_retry(
            vec![Script { error: None, steps }],
            Arc::new(BoundedRetry {
                max_retries: 0,
                ..Default::default()
            }),
        )
        .await;
        h.input("task", InputDelivery::Steer).await;
        h.end(status).await;
        let events: Vec<_> = h
            .seen
            .iter()
            .filter_map(|event| match event.event {
                EventKind::TokenUsageUpdated { last, total, .. } => Some((last, total)),
                _ => None,
            })
            .collect();
        assert_eq!(events, [(tokens(usage), tokens(usage))]);
        h.stop().await;
    }
}

#[tokio::test]
async fn range_projection_without_state_preserves_http_request_bodies_on_both_wires() {
    use tiechui_plugin_api::{Module, ModuleConfig};
    for wire in WIRES {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(plain(wire), "text/event-stream"))
            .expect(4)
            .mount(&server)
            .await;
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        for inspect in [false, true] {
            let store =
                SessionStore::create(root.path(), root.path(), &CancellationToken::new()).unwrap();
            let thread_id = ThreadId(store.header().id.clone());
            let mut cfg = config(provider(wire, &server.uri()));
            cfg.registry
                .register_context_provider(Arc::new(StaticContext(vec![
                    "Unchanged instructions".into(),
                ])));
            tiechui_module_context::ContextModule
                .register(&mut cfg.registry, &ModuleConfig::default())
                .unwrap();
            tiechui_module_editing::EditingModule
                .register(&mut cfg.registry, &ModuleConfig::default())
                .unwrap();
            let module = tiechui_module_compaction::CompactionModule;
            if inspect {
                module
                    .register(&mut cfg.registry, &ModuleConfig::default())
                    .unwrap();
            } else {
                let mut declarations = Registry::default();
                module
                    .register(&mut declarations, &ModuleConfig::default())
                    .unwrap();
                for spec in declarations.tools().specs() {
                    cfg.registry
                        .register_tool(declarations.tools().get(&spec.name).unwrap())
                        .unwrap();
                }
            }
            let shutdown = CancellationToken::new();
            let mut handle = SessionActor::spawn(store, cfg, shutdown.clone());
            while !matches!(event(&mut handle).await, EventKind::CommandsUpdated { .. }) {}
            for prompt in ["first", "second"] {
                if inspect {
                    handle
                        .submissions
                        .send(Submission {
                            id: SubmissionId("inspect".into()),
                            thread_id: thread_id.clone(),
                            epoch: SessionEpoch(0),
                            op: Op::RunCommand {
                                name: "context".into(),
                                arguments: "injections".into(),
                            },
                        })
                        .await
                        .unwrap();
                    while !matches!(event(&mut handle).await, EventKind::CommandPanel { .. }) {}
                }
                handle
                    .submissions
                    .send(Submission {
                        id: SubmissionId(prompt.into()),
                        thread_id: thread_id.clone(),
                        epoch: SessionEpoch(0),
                        op: Op::UserInput {
                            content: vec![InputContent::Text {
                                text: prompt.into(),
                            }],
                            delivery: InputDelivery::FollowUp,
                        },
                    })
                    .await
                    .unwrap();
                while !matches!(event(&mut handle).await, EventKind::TurnCompleted { .. }) {}
            }
            shutdown.cancel();
            handle.task.await.unwrap().unwrap();
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].body, requests[2].body);
        assert_eq!(requests[1].body, requests[3].body);
    }
}
