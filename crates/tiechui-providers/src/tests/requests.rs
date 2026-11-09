#[path = "../../../../tests/support/env.rs"]
mod env_support;

use tiechui_model::{OpaqueData, ToolResult};

use super::*;
use crate::ErrorKind;

fn image() -> Content {
    Content::Image {
        mime_type: "image/png".into(),
        data: "Zml4dHVyZQ==".into(),
    }
}

#[test]
fn per_model_maps_override_adaptive_effort_and_reject_unsupported_levels() {
    use tiechui_config::{ThinkingLevel as L, ThinkingLevelValue as V, UnsupportedLevel};
    for (id, unsupported) in [
        ("claude-opus-5-5", vec![L::Minimal]),
        ("deepseek-v4.1-flash", vec![L::Minimal, L::Medium, L::Xhigh]),
    ] {
        let mut config = config("http://fixture.invalid");
        let model = &mut config.models[0];
        model.id = id.into();
        model.thinking = Thinking::Adaptive;
        let mut map =
            std::collections::BTreeMap::from([(L::Off, V::Unsupported(UnsupportedLevel))]);
        for (key, value) in [
            (L::Low, "low"),
            (L::Medium, "medium"),
            (L::High, "high"),
            (L::Xhigh, "xhigh"),
            (L::Max, "max"),
        ] {
            map.insert(key, V::Wire(value.into()));
        }
        for key in &unsupported {
            map.insert(*key, V::Unsupported(UnsupportedLevel));
        }
        model.thinking_levels = Some(map);
        assert!(
            crate::anthropic::request::encode(&request(), model)
                .unwrap()
                .get("thinking")
                .is_none()
        );
        for (key, level, wire) in [
            (L::Minimal, ReasoningLevel::Minimal, "low"),
            (L::Low, ReasoningLevel::Low, "low"),
            (L::Medium, ReasoningLevel::Medium, "medium"),
            (L::High, ReasoningLevel::High, "high"),
            (L::Xhigh, ReasoningLevel::Xhigh, "xhigh"),
            (L::Max, ReasoningLevel::Max, "max"),
        ] {
            let mut request = request();
            request.reasoning = Some(level);
            let encoded = crate::anthropic::request::encode(&request, model);
            if unsupported.contains(&key) {
                assert_eq!(encoded.unwrap_err().kind, ErrorKind::InvalidRequest);
            } else {
                assert_eq!(encoded.unwrap()["output_config"]["effort"], wire);
            }
        }
        model.thinking_levels.as_mut().unwrap().remove(&L::Minimal);
        let mut request = request();
        request.reasoning = Some(ReasoningLevel::Minimal);
        assert_eq!(
            crate::anthropic::request::encode(&request, model).unwrap()["output_config"]["effort"],
            "low"
        );
    }
}

#[test]
fn offered_thinking_levels() {
    use ReasoningLevel as R;
    use tiechui_config::{ThinkingLevel as L, ThinkingLevelValue as V, UnsupportedLevel};
    let levels = |config: &ProviderConfig| {
        let client = crate::client_builder().no_proxy().build().unwrap();
        Anthropic::new("fixture", config, "fixture-key", client)
            .unwrap()
            .thinking_levels(&ModelId("fixture/fixture-model".into()))
    };
    let mut config = config("http://fixture.invalid");
    assert_eq!(
        levels(&config),
        [
            None,
            Some(R::Minimal),
            Some(R::Low),
            Some(R::Medium),
            Some(R::High)
        ]
    );
    config.models[0].thinking_levels = Some(std::collections::BTreeMap::from([
        (L::Off, V::Unsupported(UnsupportedLevel)),
        (L::Minimal, V::Unsupported(UnsupportedLevel)),
        (L::Medium, V::Unsupported(UnsupportedLevel)),
        (L::Max, V::Wire("max".into())),
    ]));
    assert_eq!(levels(&config), [Some(R::Low), Some(R::High), Some(R::Max)]);
    config.models[0].reasoning = false;
    assert_eq!(levels(&config), [None]);
}

fn result(id: &str, content: Vec<Content>, is_error: bool) -> Message {
    message(
        Role::Tool,
        vec![Content::ToolResult(ToolResult {
            call_id: id.into(),
            content,
            is_error,
        })],
    )
}

#[tokio::test]
async fn adaptive_thinking_maps_effort_without_interleaved_beta() {
    let server = serve(PLAIN, 6).await;
    let mut config = config(&server.uri());
    config.models[0].thinking = Thinking::Adaptive;
    let provider = Anthropic::new(
        "fixture",
        &config,
        "fixture-key",
        crate::client_builder().no_proxy().build().unwrap(),
    )
    .unwrap();
    for (level, effort) in [
        (ReasoningLevel::Minimal, "low"),
        (ReasoningLevel::Low, "low"),
        (ReasoningLevel::Medium, "medium"),
        (ReasoningLevel::High, "high"),
        (ReasoningLevel::Xhigh, "high"),
        (ReasoningLevel::Max, "high"),
    ] {
        let mut request = request();
        request.reasoning = Some(level);
        request.max_output = Some(2048);
        let events: Vec<_> = provider
            .stream(request, CancellationToken::new())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok));
        let requests = server.received_requests().await.unwrap();
        let outgoing = requests.last().unwrap();
        assert!(!outgoing.headers.contains_key("anthropic-beta"));
        let body: Value = serde_json::from_slice(&outgoing.body).unwrap();
        assert_eq!(
            body["thinking"],
            json!({"type":"adaptive", "display":"summarized"})
        );
        assert_eq!(body["output_config"], json!({"effort":effort}));
        assert_eq!(body["max_tokens"], 2048);
    }
}

#[test]
fn budget_thinking_uses_defaults_and_fits_the_output_ceiling() {
    let mut config = config("http://fixture.invalid");
    let model = &mut config.models[0];
    model.max_output = 32000;
    for (level, budget) in [
        (ReasoningLevel::Minimal, 1024),
        (ReasoningLevel::Low, 2048),
        (ReasoningLevel::Medium, 8192),
        (ReasoningLevel::High, 16384),
        (ReasoningLevel::Xhigh, 16384),
        (ReasoningLevel::Max, 16384),
    ] {
        let mut request = request();
        request.reasoning = Some(level);
        for cap in [None, Some(2048), Some(30000)] {
            request.max_output = cap;
            let body = crate::anthropic::request::encode(&request, model).unwrap();
            let ceiling = cap.map_or(32000, |cap| (cap + budget).min(32000));
            assert_eq!(body["max_tokens"], ceiling);
            assert_eq!(
                body["thinking"],
                json!({"type":"enabled", "budget_tokens":budget.min(ceiling - 1024), "display":"summarized"})
            );
            assert!(body.get("output_config").is_none());
        }
    }
    model.max_output = 4096;
    let mut request = request();
    request.reasoning = Some(ReasoningLevel::High);
    let body = crate::anthropic::request::encode(&request, model).unwrap();
    assert_eq!(body["thinking"]["budget_tokens"], 3072);
}

#[tokio::test]
async fn reasoning_none_disables_thinking_and_non_reasoning_models_reject_levels() {
    let server = serve(PLAIN, 2).await;
    for thinking in [Thinking::Budget, Thinking::Adaptive] {
        let mut config = config(&server.uri());
        config.models[0].thinking = thinking;
        let provider = Anthropic::new(
            "fixture",
            &config,
            "fixture-key",
            crate::client_builder().no_proxy().build().unwrap(),
        )
        .unwrap();
        let events: Vec<_> = provider
            .stream(request(), CancellationToken::new())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok));
        let requests = server.received_requests().await.unwrap();
        let outgoing = requests.last().unwrap();
        let body: Value = serde_json::from_slice(&outgoing.body).unwrap();
        assert_eq!(body["thinking"], json!({"type":"disabled"}));
        assert!(body.get("output_config").is_none());
        assert!(!outgoing.headers.contains_key("anthropic-beta"));
        config.models[0].reasoning = false;
        let body = crate::anthropic::request::encode(&request(), &config.models[0]).unwrap();
        assert!(body.get("thinking").is_none());
        let provider = Anthropic::new(
            "fixture",
            &config,
            "fixture-key",
            crate::client_builder().no_proxy().build().unwrap(),
        )
        .unwrap();
        let mut request = request();
        request.reasoning = Some(ReasoningLevel::Low);
        assert_eq!(
            provider
                .stream(request, CancellationToken::new())
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::InvalidRequest
        );
    }
}

#[tokio::test]
async fn replay_redacted_thinking_request_preserves_opaque_data() {
    let server = serve(REDACTED, 1).await;
    let content = collect(&server.uri(), request())
        .await
        .into_iter()
        .filter_map(|event| match event.unwrap() {
            StreamEvent::ContentComplete { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    let mut request = request();
    request.messages.extend([
        message(Role::Assistant, content),
        message(Role::User, vec![text("Continue")]),
    ]);
    insta::assert_json_snapshot!(captured(request).await);
}

#[tokio::test]
async fn first_turn_request() {
    let mut request = request();
    request.messages = vec![
        message(
            Role::System,
            vec![
                text("You are a coding assistant."),
                text("Use tools carefully."),
            ],
        ),
        message(Role::User, vec![text("Describe this file."), image()]),
        message(Role::System, vec![text("Work in the project directory.")]),
    ];
    request.tools = vec![tool("read"), tool("write")];
    request.max_output = Some(2048);
    insta::assert_json_snapshot!(captured(request).await);
}

#[tokio::test]
async fn tool_results_request() {
    let mut request = request();
    request.tools = vec![tool("read")];
    request.messages.extend([
        message(
            Role::Assistant,
            vec![
                text("Reading both files."),
                call("call_a", "a.png"),
                call("call_b", "missing.txt"),
            ],
        ),
        result("call_a", vec![text("Image contents:"), image()], false),
        result("call_b", vec![text("File not found.")], true),
    ]);
    insta::assert_json_snapshot!(captured(request).await);
}

#[tokio::test]
async fn replay_thinking_request_preserves_streamed_signature() {
    let server = serve(THINKING, 1).await;
    let content = collect(&server.uri(), request())
        .await
        .into_iter()
        .filter_map(|event| match event.unwrap() {
            StreamEvent::ContentComplete { content, .. } => Some(content),
            _ => None,
        })
        .collect();
    let mut request = request();
    request.tools = vec![tool("read")];
    request.reasoning = Some(ReasoningLevel::Minimal);
    request.messages.extend([
        message(Role::Assistant, content),
        result("call_read", vec![text("sample contents")], false),
    ]);
    let server = serve(PLAIN, 1).await;
    assert!(
        collect(&server.uri(), request)
            .await
            .iter()
            .all(Result::is_ok)
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers["anthropic-beta"],
        "interleaved-thinking-2025-05-14"
    );
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body["messages"][1]["content"][0]["signature"],
        "sig-fixture \t"
    );
    insta::assert_json_snapshot!(body);
}

#[tokio::test]
async fn signature_only_thinking_is_preserved_and_unsigned_thinking_becomes_text() {
    let mut request = request();
    request.messages.push(message(
        Role::Assistant,
        vec![
            Content::Thinking {
                text: String::new(),
                redacted: false,
                opaque: Some(OpaqueData(" exact signature\n".into())),
            },
            Content::Thinking {
                text: "Unsigned thought".into(),
                redacted: false,
                opaque: None,
            },
            Content::Thinking {
                text: String::new(),
                redacted: false,
                opaque: None,
            },
        ],
    ));
    let body = captured(request).await;
    assert_eq!(
        body["messages"][1]["content"],
        json!([
            {"type":"thinking","thinking":"","signature":" exact signature\n"},
            {"type":"text","text":"Unsigned thought"},
        ])
    );
    // The encoder does not search backward for a cacheable user block after an assistant prefill.
    assert!(
        body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none()
    );
}

#[tokio::test]
async fn invalid_requests_are_rejected_before_http() {
    let server = MockServer::start().await;
    let provider = provider(&server.uri());
    let mut cases = Vec::new();
    let mut value = request();
    value.model = ModelId("missing/model".into());
    cases.push(value);
    let mut value = request();
    value.max_output = Some(0);
    cases.push(value);
    let mut value = request();
    value.max_output = Some(4097);
    cases.push(value);
    let mut value = request();
    value.messages.clear();
    cases.push(value);
    let mut value = request();
    value
        .messages
        .push(message(Role::Assistant, vec![call("pending", "a")]));
    cases.push(value);
    let mut value = request();
    value
        .messages
        .push(result("orphan", vec![text("result")], false));
    cases.push(value);
    let mut value = request();
    value.messages.extend([
        message(Role::Assistant, vec![call("invalid/id", "a")]),
        result("invalid/id", vec![text("result")], false),
    ]);
    cases.push(value);
    let mut value = request();
    value.tools = vec![tool("read"), tool("read")];
    cases.push(value);
    for request in cases {
        let error = provider
            .stream(request, CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, ErrorKind::InvalidRequest);
    }
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(
        provider.stream(request(), cancel).await.err().unwrap().kind,
        ErrorKind::Cancelled
    );
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(
        provider
            .capabilities(&ModelId("fixture/fixture-model".into()))
            .context_window,
        200_000
    );
    assert_eq!(
        provider
            .capabilities(&ModelId("missing/model".into()))
            .context_window,
        0
    );
}

#[tokio::test]
async fn factory_reads_only_the_named_environment_key() {
    let server = serve(PLAIN, 1).await;
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "tests::requests::factory_environment_child",
            "--nocapture",
        ])
        .env_clear()
        .envs(env_support::windows_env())
        .env("TIECHUI_FACTORY_TEST_URL", server.uri())
        .env("TIECHUI_FACTORY_TEST_KEY", "fixture-key")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn factory_environment_child() {
    let Ok(url) = std::env::var("TIECHUI_FACTORY_TEST_URL") else {
        return;
    };
    let mut config = config(&url);
    config.api_key_env = Some("TIECHUI_FACTORY_TEST_KEY".into());
    let provider = crate::from_config("fixture", &config, &Default::default()).unwrap();
    let events: Vec<_> = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(events.len(), 6);
    assert!(events.iter().all(Result::is_ok));

    config.api_key_env = Some("TIECHUI_FACTORY_MISSING_KEY".into());
    assert_eq!(
        crate::from_config("fixture", &config, &Default::default())
            .err()
            .unwrap()
            .kind,
        ErrorKind::Auth
    );
    config.api_key_env = None;
    assert_eq!(
        crate::from_config("fixture", &config, &Default::default())
            .err()
            .unwrap()
            .kind,
        ErrorKind::Auth
    );
    config.wire = Wire::OpenaiCompletions;
    assert_eq!(
        crate::from_config("fixture", &config, &Default::default())
            .err()
            .unwrap()
            .kind,
        ErrorKind::Auth
    );
    config.api_key_env = Some("TIECHUI_FACTORY_TEST_KEY".into());
    assert_eq!(
        crate::from_config("fixture", &config, &Default::default())
            .unwrap()
            .capabilities(&request().model)
            .context_window,
        200_000
    );
}
