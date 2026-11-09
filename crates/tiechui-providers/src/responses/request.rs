use std::collections::BTreeSet;

use serde_json::{Value, json};
use tiechui_config::{CompletionsCompat, ModelConfig};
use tiechui_model::{Content, ModelRequest, Role};

use crate::{ErrorKind, ProviderError};

fn invalid(message: &str) -> ProviderError {
    ProviderError::new(ErrorKind::InvalidRequest, message)
}

fn image(content: &Content, supported: bool) -> Result<Value, ProviderError> {
    match content {
        Content::Image { mime_type, data } if supported => Ok(json!({
            "type":"input_image", "image_url":format!("data:{mime_type};base64,{data}"), "detail":"auto"
        })),
        _ => Err(invalid("selected model does not support images")),
    }
}

/// A streamed reasoning item, kept without its id so it replays with `store: false`.
fn reasoning(data: &str) -> Result<Value, ProviderError> {
    serde_json::from_str::<Value>(data)
        .ok()
        .filter(|item| item["type"] == "reasoning" && item["encrypted_content"].is_string())
        .ok_or_else(|| invalid("unsupported thinking replay metadata"))
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
    let mut input = Vec::new();
    let mut systems = Vec::new();
    let mut pending = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut tool_history = false;
    for message in &request.messages {
        if message.opaque.is_some() {
            return Err(invalid("unsupported message replay metadata"));
        }
        if matches!(message.role, Role::User | Role::Assistant) && !pending.is_empty() {
            return Err(invalid(
                "tool calls must have results before the next user or assistant message",
            ));
        }
        match message.role {
            Role::System => {
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
            }
            Role::User => {
                let mut content = Vec::new();
                for block in &message.content {
                    match block {
                        Content::Text { text, opaque: None } if !text.is_empty() => {
                            content.push(json!({"type":"input_text", "text":text}))
                        }
                        Content::Text { opaque: None, .. } => {}
                        Content::Image { .. } => content.push(image(block, model.images)?),
                        _ => return Err(invalid("unsupported user content or replay metadata")),
                    }
                }
                if !content.is_empty() {
                    input.push(json!({"type":"message", "role":"user", "content":content}));
                }
            }
            Role::Assistant => {
                for block in &message.content {
                    match block {
                        Content::Text { text, opaque: None } => {
                            if !text.trim().is_empty() {
                                input.push(
                                    json!({"type":"message", "role":"assistant", "content":text}),
                                );
                            }
                        }
                        // Reasoning without encrypted content cannot be replayed.
                        Content::Thinking {
                            redacted: false,
                            opaque,
                            ..
                        } => {
                            if let Some(opaque) = opaque {
                                input.push(reasoning(&opaque.0)?);
                            }
                        }
                        Content::ToolCall(call) if call.opaque.is_none() => {
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
                            input.push(json!({"type":"function_call", "call_id":call.id, "name":call.name, "arguments":call.arguments.to_string()}));
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
                    let mut parts = Vec::new();
                    let mut images = Vec::new();
                    for block in &result.content {
                        match block {
                            Content::Text { text, opaque: None } => parts.push(text.as_str()),
                            Content::Image { .. } => images.push(image(block, model.images)?),
                            _ => {
                                return Err(invalid(
                                    "unsupported tool-result content or replay metadata",
                                ));
                            }
                        }
                    }
                    let text = parts.join("\n");
                    let output = if images.is_empty() {
                        json!(if text.trim().is_empty() {
                            "(no tool output)"
                        } else {
                            text.as_str()
                        })
                    } else {
                        let mut output = Vec::new();
                        if !text.trim().is_empty() {
                            output.push(json!({"type":"input_text", "text":text}));
                        }
                        output.append(&mut images);
                        json!(output)
                    };
                    input.push(json!({"type":"function_call_output", "call_id":result.call_id, "output":output}));
                }
            }
        }
    }
    if !pending.is_empty() {
        return Err(invalid("history ends with unresolved tool calls"));
    }
    if input.is_empty() {
        return Err(invalid(
            "at least one user or assistant message is required",
        ));
    }
    if !systems.is_empty() {
        input.insert(0, json!({"type":"message", "role":if model.reasoning && compat.supports_developer_role { "developer" } else { "system" }, "content":systems.join("\n\n")}));
    }
    let mut body =
        json!({"model":model.id, "input":input, "stream":true, "max_output_tokens":max_output});
    if compat.supports_store {
        body["store"] = json!(false);
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
            // Responses tools are strict unless told otherwise.
            tools.push(json!({"type":"function", "name":tool.name, "description":tool.description, "parameters":tool.input_schema, "strict":false}));
        }
        body["tools"] = json!(tools);
    }
    if model.reasoning {
        let mut reasoning = json!({"summary":"auto"});
        if compat.supports_reasoning_effort
            && let Some(effort) = request
                .reasoning
                .map(|level| mapped.unwrap_or(crate::thinking::effort(level)))
                .or(mapped)
        {
            reasoning["effort"] = json!(effort);
        }
        body["reasoning"] = reasoning;
        body["include"] = json!(["reasoning.encrypted_content"]);
    }
    Ok(body)
}
