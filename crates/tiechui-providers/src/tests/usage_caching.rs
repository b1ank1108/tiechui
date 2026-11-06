use super::*;
use tiechui_config::{CacheRetention, ModelPrices};
use tiechui_model::{StopReason, Usage};

fn adapter(config: &ProviderConfig) -> Box<dyn ModelProvider> {
    let client = crate::client_builder().no_proxy().build().unwrap();
    match config.wire {
        Wire::AnthropicMessages => {
            Box::new(Anthropic::new("fixture", config, "fixture-key", client).unwrap())
        }
        Wire::OpenaiCompletions => {
            Box::new(crate::openai::Openai::new("fixture", config, "fixture-key", client).unwrap())
        }
    }
}

#[test]
fn cache_retention_requests_preserve_default_and_completions_payloads() {
    let mut config = config("http://fixture.invalid");
    let model = &mut config.models[0];
    let mut request = request();
    request
        .messages
        .insert(0, message(Role::System, vec![text("stable instructions")]));
    request.tools.push(tool("read"));
    let default = crate::anthropic::request::encode(&request, model).unwrap();
    let completions = crate::openai::request::encode(&request, model, &Default::default()).unwrap();
    for (retention, name) in [
        (CacheRetention::None, "none"),
        (CacheRetention::Long, "long"),
    ] {
        model.cache_retention = retention;
        let encoded = crate::anthropic::request::encode(&request, model).unwrap();
        insta::assert_json_snapshot!(format!("anthropic_cache_{name}"), encoded);
        let encoded = crate::openai::request::encode(&request, model, &Default::default()).unwrap();
        assert_eq!(encoded, completions);
        insta::assert_json_snapshot!(format!("completions_cache_{name}"), encoded);
    }
    model.cache_retention = CacheRetention::Short;
    assert_eq!(
        crate::anthropic::request::encode(&request, model).unwrap(),
        default
    );
    request.disable_cache = true;
    assert!(
        !crate::anthropic::request::encode(&request, model)
            .unwrap()
            .to_string()
            .contains("cache_control")
    );
    assert_eq!(
        crate::openai::request::encode(&request, model, &Default::default()).unwrap(),
        completions
    );
}

#[test]
fn prompt_cache_ttl_follows_the_declared_retention_on_messages_only() {
    let mut config = config("http://fixture.invalid");
    let model = ModelId("fixture/fixture-model".into());
    for (retention, ttl) in [
        (CacheRetention::None, None),
        (CacheRetention::Short, Some(300)),
        (CacheRetention::Long, Some(3600)),
    ] {
        config.models[0].cache_retention = retention;
        let ttl = ttl.map(std::time::Duration::from_secs);
        assert_eq!(adapter(&config).prompt_cache_ttl(&model), ttl);
        assert_eq!(
            adapter(&config).prompt_cache_ttl(&ModelId("unknown".into())),
            None
        );
        config.wire = Wire::OpenaiCompletions;
        assert_eq!(adapter(&config).prompt_cache_ttl(&model), None);
        config.wire = Wire::AnthropicMessages;
    }
}

#[test]
fn effort_switch_replays_history_with_the_current_request_effort_on_both_wires() {
    let mut config = config("http://fixture.invalid");
    let model = &mut config.models[0];
    model.thinking = Thinking::Adaptive;
    let mut request = request();
    request.reasoning = Some(ReasoningLevel::Low);
    assert_eq!(
        crate::anthropic::request::encode(&request, model).unwrap()["output_config"]["effort"],
        "low"
    );
    assert_eq!(
        crate::openai::request::encode(&request, model, &Default::default()).unwrap()["reasoning_effort"],
        "low"
    );
    request
        .messages
        .push(message(Role::Assistant, vec![text("previous answer")]));
    request
        .messages
        .push(message(Role::User, vec![text("think harder")]));
    request.reasoning = Some(ReasoningLevel::High);
    let anthropic = crate::anthropic::request::encode(&request, model).unwrap();
    let completions = crate::openai::request::encode(&request, model, &Default::default()).unwrap();
    assert_eq!(anthropic["output_config"]["effort"], "high");
    assert_eq!(completions["reasoning_effort"], "high");
    insta::assert_json_snapshot!("anthropic_effort_switch", anthropic);
    insta::assert_json_snapshot!("completions_effort_switch", completions);
}

#[tokio::test]
async fn usage_details_tolerate_unknown_fields_and_preserve_partial_updates() {
    for wire in [Wire::AnthropicMessages, Wire::OpenaiCompletions] {
        let fixture = match wire {
            Wire::AnthropicMessages => PLAIN
                .replace("\"cache_creation_input_tokens\":5", "\"cache_creation_input_tokens\":5,\"cache_creation\":{\"ephemeral_1h_input_tokens\":3,\"future\":true},\"output_tokens_details\":{\"thinking_tokens\":1,\"future\":true}"),
            Wire::OpenaiCompletions => super::openai::PLAIN
                .replace("\"completion_tokens\":2", "\"completion_tokens\":2,\"completion_tokens_details\":{\"reasoning_tokens\":1,\"future\":true}"),
        };
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(fixture, "text/event-stream"))
            .mount(&server)
            .await;
        let mut config = config(&server.uri());
        config.wire = wire;
        let provider = adapter(&config);
        let events = provider
            .stream(request(), CancellationToken::new())
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await;
        let Ok(StreamEvent::Finish {
            reason: StopReason::EndTurn,
            usage,
        }) = events.last().unwrap()
        else {
            panic!("{events:?}");
        };
        assert_eq!(usage.reasoning, 1);
        assert_eq!(usage.output, 2);
        assert_eq!(
            usage.cache_write_1h,
            if wire == Wire::AnthropicMessages {
                3
            } else {
                0
            }
        );
        assert_eq!(usage.cost_nano_usd, None);
    }
}

#[test]
fn cost_uses_only_configured_returned_model_prices_and_counts_reasoning_once() {
    for wire in [Wire::AnthropicMessages, Wire::OpenaiCompletions] {
        let mut config = config("http://fixture.invalid");
        config.wire = wire;
        let mut priced = config.models[0].clone();
        priced.id = "priced".into();
        priced.prices = Some(ModelPrices {
            input: 2.0,
            output: 10.0,
            cache_read: 0.2,
            cache_write: 2.5,
        });
        config.models.push(priced);
        let provider = adapter(&config);
        let usage = Usage {
            partial: false,
            input: 100,
            output: 20,
            cache_read: 50,
            cache_write: 10,
            cache_write_1h: 4,
            reasoning: 12,
            cost_nano_usd: None,
        };
        assert_eq!(
            provider.cost_nano_usd(&ModelId("fixture/priced".into()), usage),
            Some(441_000)
        );
        assert_eq!(
            provider.cost_nano_usd(&ModelId("fixture/fixture-model".into()), usage),
            None
        );
        assert_eq!(
            provider.cost_nano_usd(&ModelId("fixture/unknown".into()), usage),
            None
        );
    }
}

#[tokio::test]
async fn missing_required_usage_fields_are_partial_but_explicit_zero_is_complete() {
    for wire in [Wire::AnthropicMessages, Wire::OpenaiCompletions] {
        for partial in [false, true] {
            let fixture = match wire {
                Wire::AnthropicMessages => {
                    let usage = if partial {
                        r#"{"input_tokens":0}"#
                    } else {
                        r#"{"input_tokens":0,"output_tokens":0}"#
                    };
                    format!(
                        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"role\":\"assistant\",\"content\":[],\"usage\":{usage}}}}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
                    )
                }
                Wire::OpenaiCompletions => {
                    let usage = if partial {
                        r#"{"prompt_tokens":0}"#
                    } else {
                        r#"{"prompt_tokens":0,"completion_tokens":0}"#
                    };
                    format!(
                        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"ok\"}},\"finish_reason\":\"stop\"}}],\"usage\":{usage}}}\n\ndata: [DONE]\n\n"
                    )
                }
            };
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_raw(fixture, "text/event-stream"))
                .mount(&server)
                .await;
            let mut config = config(&server.uri());
            config.wire = wire;
            let events = adapter(&config)
                .stream(request(), CancellationToken::new())
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await;
            let Ok(StreamEvent::Finish { usage, .. }) = events.last().unwrap() else {
                panic!("{events:?}");
            };
            assert_eq!(usage.partial, partial);
        }
    }
}
