use std::collections::BTreeSet;

use serde_json::{Value, json};
use tiechui_config::{
    CompletionsCompat, MaxTokensField, ModelConfig, ThinkingFormat, ThinkingLevel,
    ThinkingLevelValue,
};
use tiechui_model::{Content, ModelRequest, OpaqueData, Role};

use crate::{ErrorKind, ProviderError};

fn invalid(message: &str) -> ProviderError {
    ProviderError::new(ErrorKind::InvalidRequest, message)
}

fn decode_google_extra(opaque: &OpaqueData) -> Result<Value, ProviderError> {
    let err = || invalid("unsupported assistant content or replay metadata");
    let parsed: Value = serde_json::from_str(&opaque.0).map_err(|_| err())?;
    let valid = parsed
        .as_object()
        .filter(|obj| obj.len() == 1)
        .and_then(|obj| obj.get("google")?.as_object())
        .filter(|google| google.len() == 1)
        .and_then(|google| google.get("thought_signature")?.as_str())
        .is_some_and(|sig| !sig.is_empty());
    valid.then_some(parsed).ok_or_else(err)
}

pub(super) fn reasoning_field(field: &str) -> bool {
    matches!(field, "reasoning_content" | "reasoning" | "reasoning_text")
}

fn image(content: &Content, supported: bool) -> Result<Value, ProviderError> {
    match content {
        Content::Image { mime_type, data } if supported => Ok(json!({
            "type":"image_url", "image_url":{"url":format!("data:{mime_type};base64,{data}")}
        })),
        _ => Err(invalid("selected model does not support images")),
    }
}

fn flush_images(messages: &mut Vec<Value>, images: &mut Vec<Value>) {
    if !images.is_empty() {
        let mut content =
            vec![json!({"type":"text", "text":"Attached image(s) from tool result:"})];
        content.append(images);
        messages.push(json!({"role":"user", "content":content}));
    }
}

pub(crate) fn encode(
    request: &ModelRequest,
    model: &ModelConfig,
    compat: &CompletionsCompat,
) -> Result<Value, ProviderError> {
    let max_output = request.max_output.unwrap_or(model.max_output);
    if max_output == 0 || max_output > model.max_output {
        return Err(invalid(
            "max_output must be positive and within the configured model limit",
        ));
    }
    let max_output = crate::estimate::clamp(request, model.context_window, max_output);
    if request.reasoning.is_some() && !model.reasoning {
        return Err(invalid("reasoning requires a reasoning model"));
    }
    let mapped = crate::thinking::mapped(model, request.reasoning)?;
    let mut messages = Vec::new();
    let mut systems = Vec::new();
    let mut pending = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut tool_images = Vec::new();
    let mut tool_history = false;
    for message in &request.messages {
        if message.opaque.is_some() {
            return Err(invalid("unsupported message replay metadata"));
        }
        if message.role == Role::System {
            let mut parts = Vec::new();
            for block in &message.content {
                match block {
                    Content::Text { text, opaque: None } => parts.push(text.as_str()),
                    _ => return Err(invalid("system messages must contain text only")),
                }
            }
            let text = parts.join("\n");
            if !text.is_empty() {
                systems.push(text);
            }
            continue;
        }
        if message.role != Role::Tool {
            if !pending.is_empty() {
                return Err(invalid(
                    "tool calls must have results before the next user or assistant message",
                ));
            }
            flush_images(&mut messages, &mut tool_images);
        }
        match message.role {
            Role::User => {
                let mut content = Vec::new();
                for block in &message.content {
                    match block {
                        Content::Text { text, opaque: None } if !text.is_empty() => {
                            content.push(json!({"type":"text", "text":text}))
                        }
                        Content::Text { opaque: None, .. } => {}
                        Content::Image { .. } => content.push(image(block, model.images)?),
                        _ => return Err(invalid("unsupported user content or replay metadata")),
                    }
                }
                if !content.is_empty() {
                    messages.push(json!({"role":"user", "content":content}));
                }
            }
            Role::Assistant => {
                let mut text_parts = Vec::new();
                let mut text_extra = None;
                let mut thinking = Vec::new();
                let mut field = None;
                let mut calls = Vec::new();
                for block in &message.content {
                    match block {
                        Content::Text { text, opaque } => {
                            if let Some(opaque) = opaque {
                                text_extra = Some(decode_google_extra(opaque)?);
                            }
                            if !text.trim().is_empty() {
                                text_parts.push(text.as_str());
                            }
                        }
                        Content::Thinking {
                            text,
                            redacted: false,
                            opaque,
                        } => {
                            if let Some(opaque) = opaque {
                                if !reasoning_field(&opaque.0) {
                                    return Err(invalid("unsupported thinking replay metadata"));
                                }
                                field.get_or_insert(opaque.0.as_str());
                            }
                            if !text.trim().is_empty() {
                                thinking.push(text.as_str());
                            }
                        }
                        Content::ToolCall(call) => {
                            let extra =
                                call.opaque.as_ref().map(decode_google_extra).transpose()?;
                            if call.id.is_empty()
                                || call.name.is_empty()
                                || !call.arguments.is_object()
                                || !seen.insert(call.id.clone())
                            {
                                return Err(invalid(
                                    "tool calls require unique ids, names, and object arguments",
                                ));
                            }
                            pending.insert(call.id.clone());
                            tool_history = true;
                            let mut item = json!({"id":call.id, "type":"function", "function":{"name":call.name, "arguments":call.arguments.to_string()}});
                            if let Some(extra) = extra {
                                item["extra_content"] = extra;
                            }
                            calls.push(item);
                        }
                        _ => {
                            return Err(invalid(
                                "unsupported assistant content or replay metadata",
                            ));
                        }
                    }
                }
                let text = text_parts.concat();
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut outgoing = json!({"role":"assistant", "content":if text.is_empty() { Value::Null } else { json!(text) }});
                if let Some(extra) = text_extra {
                    outgoing["extra_content"] = extra;
                }
                if !calls.is_empty() {
                    outgoing["tool_calls"] = json!(calls);
                }
                if !thinking.is_empty()
                    && let Some(field) = field.or_else(|| {
                        compat
                            .requires_reasoning_content_on_assistant_messages
                            .then_some("reasoning_content")
                    })
                {
                    outgoing[field] = json!(thinking.join("\n"));
                }
                if model.reasoning
                    && compat.requires_reasoning_content_on_assistant_messages
                    && outgoing.get("reasoning_content").is_none()
                {
                    outgoing["reasoning_content"] = json!("");
                }
                messages.push(outgoing);
            }
            Role::Tool => {
                for block in &message.content {
                    let Content::ToolResult(result) = block else {
                        return Err(invalid("tool messages must contain tool results"));
                    };
                    if !pending.remove(&result.call_id) {
                        return Err(invalid("tool result has no pending call or is duplicated"));
                    }
                    let mut parts = Vec::new();
                    let mut has_image = false;
                    for block in &result.content {
                        match block {
                            Content::Text { text, opaque: None } => parts.push(text.as_str()),
                            Content::Image { .. } => {
                                tool_images.push(image(block, model.images)?);
                                has_image = true;
                            }
                            _ => {
                                return Err(invalid(
                                    "unsupported tool-result content or replay metadata",
                                ));
                            }
                        }
                    }
                    let text = parts.join("\n");
                    let text = if !text.trim().is_empty() {
                        text.as_str()
                    } else if has_image {
                        "(see attached image)"
                    } else {
                        "(no tool output)"
                    };
                    messages.push(
                        json!({"role":"tool", "tool_call_id":result.call_id, "content":text}),
                    );
                }
            }
            Role::System => unreachable!("system messages were handled above"),
        }
    }
    if !pending.is_empty() {
        return Err(invalid("history ends with unresolved tool calls"));
    }
    flush_images(&mut messages, &mut tool_images);
    if messages.is_empty() {
        return Err(invalid(
            "at least one user or assistant message is required",
        ));
    }
    if !systems.is_empty() {
        messages.insert(0, json!({"role":if model.reasoning && compat.supports_developer_role { "developer" } else { "system" }, "content":systems.join("\n\n")}));
    }
    let mut body = json!({"model":model.id, "messages":messages, "stream":true});
    body[match compat.max_tokens_field {
        MaxTokensField::MaxTokens => "max_tokens",
        MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
    }] = json!(max_output);
    if compat.supports_store {
        body["store"] = json!(false);
    }
    if compat.supports_usage_in_streaming {
        body["stream_options"] = json!({"include_usage":true});
    }
    if !request.tools.is_empty() || tool_history {
        let mut tools = Vec::new();
        let mut names = BTreeSet::new();
        for tool in &request.tools {
            if tool.name.is_empty() || !names.insert(&tool.name) || !tool.input_schema.is_object() {
                return Err(invalid(
                    "tools require unique names and object input schemas",
                ));
            }
            let mut function = json!({"name":tool.name, "description":tool.description, "parameters":tool.input_schema});
            if compat.supports_strict_mode {
                function["strict"] = json!(false);
            }
            tools.push(json!({"type":"function", "function":function}));
        }
        body["tools"] = json!(tools);
    }
    if model.reasoning {
        if compat.thinking_format == ThinkingFormat::Deepseek {
            if request.reasoning.is_some() {
                body["thinking"] = json!({"type":"enabled"});
            } else if !matches!(
                model
                    .thinking_levels
                    .as_ref()
                    .and_then(|map| map.get(&ThinkingLevel::Off)),
                Some(ThinkingLevelValue::Unsupported(_))
            ) {
                body["thinking"] = json!({"type":"disabled"});
            }
        }
        if compat.supports_reasoning_effort {
            if let Some(level) = request.reasoning {
                body["reasoning_effort"] = json!(mapped.unwrap_or(crate::thinking::effort(level)));
            } else if compat.thinking_format == ThinkingFormat::Openai
                && let Some(mapped) = mapped
            {
                body["reasoning_effort"] = json!(mapped);
            }
        }
    }
    Ok(body)
}
