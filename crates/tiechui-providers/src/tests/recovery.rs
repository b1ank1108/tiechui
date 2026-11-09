use super::*;

#[tokio::test]
async fn output_clamps_at_context_edge_and_refits_thinking() {
    for wire in [
        Wire::AnthropicMessages,
        Wire::OpenaiCompletions,
        Wire::OpenaiResponses,
    ] {
        for (window, reasoning, expected, budget) in [
            (5000, None, 902, 0),
            (4098, None, 1, 0),
            (4096, None, 1, 0),
            (6000, Some(ReasoningLevel::High), 1902, 878),
            (4096, Some(ReasoningLevel::High), 1, 1024),
        ] {
            let server = MockServer::start().await;
            let (fixture, field) = match wire {
                Wire::AnthropicMessages => (PLAIN, "max_tokens"),
                Wire::OpenaiCompletions => (super::openai::PLAIN, "max_completion_tokens"),
                Wire::OpenaiResponses => (super::responses::PLAIN, "max_output_tokens"),
            };
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(fixture, "text/event-stream"))
                .mount(&server)
                .await;
            let mut config = config(&server.uri());
            config.wire = wire;
            config.models[0].context_window = window;
            let client = crate::client_builder().no_proxy().build().unwrap();
            let provider: Box<dyn ModelProvider> = match config.wire {
                Wire::AnthropicMessages => {
                    Box::new(Anthropic::new("fixture", &config, "fixture-key", client).unwrap())
                }
                Wire::OpenaiCompletions => Box::new(
                    crate::openai::Openai::new("fixture", &config, "fixture-key", client).unwrap(),
                ),
                Wire::OpenaiResponses => Box::new(
                    crate::responses::Responses::new("fixture", &config, "fixture-key", client)
                        .unwrap(),
                ),
            };
            let request = ModelRequest {
                disable_cache: false,
                input_tokens: None,
                model: ModelId("fixture/fixture-model".into()),
                messages: vec![message(Role::User, vec![text("1234567")])],
                tools: vec![],
                max_output: None,
                reasoning,
            };
            let events = provider
                .stream(request, CancellationToken::new())
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            assert!(events.iter().all(Result::is_ok));
            let body: Value =
                serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
            assert_eq!(body[field], expected);
            if wire == Wire::AnthropicMessages && reasoning.is_some() {
                assert_eq!(body["thinking"]["budget_tokens"], budget);
            }
        }
    }
}

#[tokio::test]
async fn fallback_header_payload_and_model_precede_usage() {
    let server = MockServer::start().await;
    let body = PLAIN.replace(
        "\"id\":\"fixture_plain\"",
        "\"id\":\"fixture_plain\",\"model\":\"backup\"",
    );
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .expect(1)
        .mount(&server)
        .await;
    let mut config = config(&server.uri());
    config.models[0].fallback_models = vec!["backup".into()];
    let mut backup = config.models[0].clone();
    backup.id = "backup".into();
    backup.fallback_models.clear();
    config.models.push(backup);
    let client = crate::client_builder().no_proxy().build().unwrap();
    let provider = Anthropic::new("fixture", &config, "fixture-key", client).unwrap();
    let mut request = request();
    request.reasoning = Some(ReasoningLevel::Low);
    let events: Vec<_> = provider
        .stream(request, CancellationToken::new())
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(
        events[0],
        Ok(StreamEvent::ResponseModel {
            model: ModelId("fixture/backup".into())
        })
    );
    assert!(matches!(events[1], Ok(StreamEvent::Usage { .. })));
    let body: Value =
        serde_json::from_slice(&server.received_requests().await.unwrap()[0].body).unwrap();
    assert_eq!(body["fallbacks"], json!([{"model":"backup"}]));
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].headers["anthropic-beta"],
        "interleaved-thinking-2025-05-14,server-side-fallback-2026-07-01"
    );
}
