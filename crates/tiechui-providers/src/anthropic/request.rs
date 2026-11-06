use std::collections::BTreeSet;

use serde_json::{Value, json};
use tiechui_config::{CacheRetention, ModelConfig, Thinking, ThinkingLevelValue};
use tiechui_model::{Content, ModelRequest, ReasoningLevel, Role};

use crate::{ErrorKind, ProviderError};

fn invalid(message: &str) -> ProviderError {
    ProviderError::new(ErrorKind::InvalidRequest, message)
}

fn cache(retention: CacheRetention) -> Option<Value> {
    match retention {
        CacheRetention::None => None,
        CacheRetention::Short => Some(json!({"type": "ephemeral"})),
        CacheRetention::Long => Some(json!({"type": "ephemeral", "ttl": "1h"})),
    }
}

pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn text_or_image(content: &Content, images: bool) -> Result<Option<Value>, ProviderError> {
    match content {
        Content::Text { text, opaque: None } => {
            Ok((!text.trim().is_empty()).then(|| json!({"type": "text", "text": text})))
        }
        Content::Image { mime_type, data } if images => Ok(Some(json!({
            "type": "image", "source": {"type": "base64", "media_type": mime_type, "data": data},
        }))),
        Content::Image { .. } => Err(invalid("selected model does not support images")),
        _ => Err(invalid(
            "expected text or image content without extra replay metadata",
        )),
    }
}

pub(crate) fn encode(request: &ModelRequest, model: &ModelConfig) -> Result<Value, ProviderError> {
    let cache = cache(if request.disable_cache {
        CacheRetention::None
    } else {
        model.cache_retention
    });
    let mut max_output = request.max_output.unwrap_or(model.max_output);
    if max_output == 0 || max_output > model.max_output {
        return Err(invalid(
            "max_output must be positive and within the configured model limit",
        ));
    }
    max_output = crate::estimate::clamp(request, model.context_window, max_output);
    if request.reasoning.is_some() && !model.reasoning {
        return Err(invalid("reasoning requires a reasoning model"));
    }
    let mapped = crate::thinking::mapped(model, request.reasoning)?;
    let thinking = if let Some(level) = request.reasoning {
        match model.thinking {
            Thinking::Adaptive => {
                let effort = mapped.unwrap_or(match level {
                    ReasoningLevel::Minimal | ReasoningLevel::Low => "low",
                    ReasoningLevel::Medium => "medium",
                    ReasoningLevel::High | ReasoningLevel::Xhigh | ReasoningLevel::Max => "high",
                });
                Some((
                    json!({"type":"adaptive", "display":"summarized"}),
                    Some(effort),
                ))
            }
            Thinking::Budget => {
                let budget = match level {
                    ReasoningLevel::Minimal => 1024,
                    ReasoningLevel::Low => 2048,
                    ReasoningLevel::Medium => 8192,
                    ReasoningLevel::High | ReasoningLevel::Xhigh | ReasoningLevel::Max => 16384,
                };
                if let Some(cap) = request.max_output {
                    max_output = cap.saturating_add(budget).min(model.max_output);
                }
                max_output = crate::estimate::clamp(request, model.context_window, max_output);
                let budget = budget.min(max_output.saturating_sub(1024));
                let budget = if budget == 0 { 1024 } else { budget };
                Some((
                    json!({"type":"enabled", "budget_tokens":budget, "display":"summarized"}),
                    None,
                ))
            }
        }
    } else {
        (model.reasoning
            && !matches!(
                model
                    .thinking_levels
                    .as_ref()
                    .and_then(|map| map.get(&tiechui_config::ThinkingLevel::Off)),
                Some(ThinkingLevelValue::Unsupported(_))
            ))
        .then(|| (json!({"type":"disabled"}), None))
    };
    let mut systems = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    let mut pending = BTreeSet::new();
    let mut seen_calls = BTreeSet::new();
    let mut previous_tool = false;
    for message in &request.messages {
        if message.opaque.is_some() {
            return Err(invalid("unsupported message replay metadata"));
        }
        if message.role == Role::System {
            let mut text = Vec::new();
            for block in &message.content {
                match block {
                    Content::Text {
                        text: part,
                        opaque: None,
                    } => text.push(part.as_str()),
                    _ => return Err(invalid("system messages must contain text only")),
                }
            }
            let text = text.join("\n");
            if !text.is_empty() {
                systems.push(text);
            }
            continue;
        }
        if message.role != Role::Tool && !pending.is_empty() {
            return Err(invalid(
                "tool calls must have results before the next user or assistant message",
            ));
        }
        let mut blocks = Vec::new();
        match message.role {
            Role::User => {
                for block in &message.content {
                    if let Some(block) = text_or_image(block, model.images)? {
                        blocks.push(block);
                    }
                }
            }
            Role::Assistant => {
                for block in &message.content {
                    match block {
                        Content::Text { text, opaque: None } if !text.trim().is_empty() => {
                            blocks.push(json!({"type":"text", "text":text}))
                        }
                        Content::Text { opaque: None, .. } => {}
                        Content::Thinking {
                            redacted: true,
                            opaque,
                            ..
                        } => {
                            let data = opaque
                                .as_ref()
                                .ok_or_else(|| invalid("redacted thinking requires replay data"))?;
                            blocks.push(json!({"type":"redacted_thinking", "data":data.0}));
                        }
                        Content::Thinking {
                            text,
                            opaque,
                            redacted: false,
                        } => {
                            if let Some(signature) =
                                opaque.as_ref().filter(|value| !value.0.trim().is_empty())
                            {
                                blocks.push(json!({"type":"thinking", "thinking":text, "signature":signature.0}));
                            } else if !text.trim().is_empty() {
                                blocks.push(json!({"type":"text", "text":text}));
                            }
                        }
                        Content::ToolCall(call) if call.opaque.is_none() => {
                            if !valid_id(&call.id)
                                || call.name.is_empty()
                                || !call.arguments.is_object()
                            {
                                return Err(invalid(
                                    "tool calls require a valid id, a name, and object arguments",
                                ));
                            }
                            if !seen_calls.insert(call.id.clone()) {
                                return Err(invalid("duplicate tool-call id in history"));
                            }
                            pending.insert(call.id.clone());
                            blocks.push(json!({"type":"tool_use", "id":call.id, "name":call.name, "input":call.arguments}));
                        }
                        _ => {
                            return Err(invalid(
                                "unsupported assistant content or replay metadata",
                            ));
                        }
                    }
                }
            }
            Role::Tool => {
                for block in &message.content {
                    let Content::ToolResult(result) = block else {
                        return Err(invalid("tool messages must contain tool results"));
                    };
                    if !pending.remove(&result.call_id) {
                        return Err(invalid("tool result has no pending call or is duplicated"));
                    }
                    let mut content = Vec::new();
                    for block in &result.content {
                        if result.is_error && matches!(block, Content::Image { .. }) {
                            return Err(invalid("images in error tool results are not supported"));
                        }
                        if let Some(block) = text_or_image(block, model.images)? {
                            content.push(block);
                        }
                    }
                    blocks.push(json!({"type":"tool_result", "tool_use_id":result.call_id, "content":content, "is_error":result.is_error}));
                }
            }
            Role::System => unreachable!("system messages were handled above"),
        }
        if blocks.is_empty() {
            continue;
        }
        if matches!(message.role, Role::Tool | Role::User) && previous_tool {
            if let Some(content) = messages
                .last_mut()
                .and_then(|message| message.get_mut("content"))
                .and_then(Value::as_array_mut)
            {
                content.extend(blocks);
            }
        } else {
            messages.push(json!({"role":if message.role == Role::Assistant { "assistant" } else { "user" }, "content":blocks}));
        }
        previous_tool = message.role == Role::Tool;
    }
    if !pending.is_empty() {
        return Err(invalid("history ends with unresolved tool calls"));
    }
    if messages.is_empty() {
        return Err(invalid(
            "at least one user or assistant message is required",
        ));
    }
    if let Some(cache) = &cache
        && let Some(message) = messages.last_mut()
        && message["role"] == "user"
        && let Some(last) = message["content"]
            .as_array_mut()
            .and_then(|content| content.last_mut())
    {
        last["cache_control"] = cache.clone();
    }
    let mut body =
        json!({"model":model.id, "messages":messages, "max_tokens":max_output, "stream":true});
    if !model.fallback_models.is_empty() {
        body["fallbacks"] = json!(
            model
                .fallback_models
                .iter()
                .map(|model| json!({"model":model}))
                .collect::<Vec<_>>()
        );
    }
    if !systems.is_empty() {
        let mut block = json!({"type":"text", "text":systems.join("\n\n")});
        if let Some(cache) = &cache {
            block["cache_control"] = cache.clone();
        }
        body["system"] = json!([block]);
    }
    if !request.tools.is_empty() {
        let mut names = BTreeSet::new();
        let mut tools = Vec::new();
        for tool in &request.tools {
            if tool.name.is_empty() || !names.insert(&tool.name) || !tool.input_schema.is_object() {
                return Err(invalid(
                    "tools require unique names and object input schemas",
                ));
            }
            tools.push(json!({"name":tool.name, "description":tool.description, "input_schema":tool.input_schema, "eager_input_streaming":true}));
        }
        if let Some(cache) = &cache
            && let Some(last) = tools.last_mut()
        {
            last["cache_control"] = cache.clone();
        }
        body["tools"] = Value::Array(tools);
    }
    if let Some((thinking, effort)) = thinking {
        body["thinking"] = thinking;
        if let Some(effort) = effort {
            body["output_config"] = json!({"effort":effort});
        }
    }
    Ok(body)
}
