//! History as the current model may read it: replay data
//! of other models is dropped, their tool call ids are normalized for the target wire, and
//! images become placeholders for models without vision.

use std::collections::HashMap;

use tiechui_model::{Content, Message, Role, StopReason, ToolResult};
use tiechui_session::ModelRef;

const USER_IMAGE: &str = "(image omitted: model does not support images)";
const TOOL_IMAGE: &str = "(tool image omitted: model does not support images)";

pub fn prepare(
    history: Vec<(Message, Option<ModelRef>, Option<StopReason>)>,
    current: &ModelRef,
    images: bool,
    error_images: bool,
    normalize: impl Fn(&str) -> String,
) -> Vec<Message> {
    let mut ids = HashMap::new();
    let transformed = history
        .into_iter()
        .map(|(mut message, origin, reason)| {
            match message.role {
                Role::Assistant if origin.as_ref() == Some(current) => {
                    message.content.retain(|block| {
                        !matches!(block, Content::Thinking { text, redacted: false, opaque: None } if text.trim().is_empty())
                    });
                }
                // Signatures and reasoning are valid only for the model that produced them,
                // and readable thinking sent as text would read as part of the reply.
                Role::Assistant => {
                    message.opaque = None;
                    message.content = message
                        .content
                        .into_iter()
                        .filter_map(|block| match block {
                            Content::Thinking { .. } => None,
                            Content::Text { text, .. } => Some(Content::Text { text, opaque: None }),
                            Content::ToolCall(mut call) => {
                                call.opaque = None;
                                let id = normalize(&call.id);
                                if id != call.id {
                                    ids.insert(std::mem::replace(&mut call.id, id.clone()), id);
                                }
                                Some(Content::ToolCall(call))
                            }
                            block => Some(block),
                        })
                        .collect();
                }
                Role::User if !images => {
                    message.content = placeholders(message.content, USER_IMAGE);
                }
                Role::Tool => {
                    for block in &mut message.content {
                        if let Content::ToolResult(result) = block {
                            if let Some(id) = ids.get(&result.call_id) {
                                result.call_id = id.clone();
                            }
                            if !images {
                                result.content =
                                    placeholders(std::mem::take(&mut result.content), TOOL_IMAGE);
                            }
                        }
                    }
                }
                _ => {}
            }
            (message, reason)
        });
    repair(transformed, error_images)
}

/// Close each assistant's calls before another turn, retaining system messages in order.
fn repair(
    history: impl Iterator<Item = (Message, Option<StopReason>)>,
    error_images: bool,
) -> Vec<Message> {
    let mut out = Vec::new();
    let mut pending = Vec::new();
    let mut held = Vec::new();
    let mut attachments = Vec::new();
    for (mut message, reason) in history {
        match message.role {
            Role::Assistant | Role::User => {
                close(&mut out, &mut pending, &mut attachments, &mut held);
                if message.role == Role::Assistant {
                    if matches!(reason, Some(StopReason::Cancelled | StopReason::Error)) {
                        continue;
                    }
                    pending.extend(message.content.iter().filter_map(|block| match block {
                        Content::ToolCall(call) => Some((call.id.clone(), call.name.clone())),
                        _ => None,
                    }));
                }
                out.push(message);
            }
            Role::System => {
                if pending.is_empty() {
                    close(&mut out, &mut pending, &mut attachments, &mut held);
                    out.push(message);
                } else {
                    held.push(message);
                }
            }
            Role::Tool => {
                message.content.retain_mut(|block| {
                    let Content::ToolResult(result) = block else {
                        return false;
                    };
                    let Some(index) = pending.iter().position(|(id, _)| id == &result.call_id)
                    else {
                        return false;
                    };
                    let (_, name) = pending.remove(index);
                    if !error_images && result.is_error {
                        let images: Vec<_> = result
                            .content
                            .iter()
                            .filter(|block| matches!(block, Content::Image { .. }))
                            .cloned()
                            .collect();
                        if !images.is_empty() {
                            attachments.push(Content::Text {
                                text: format!(
                                    "Images from the error result of tool call {} ({name}):",
                                    result.call_id
                                ),
                                opaque: None,
                            });
                            attachments.extend(images);
                            result.content = placeholders(
                                std::mem::take(&mut result.content),
                                "(image attached after the tool results)",
                            );
                        }
                    }
                    true
                });
                if !message.content.is_empty() {
                    out.push(message);
                }
            }
        }
    }
    close(&mut out, &mut pending, &mut attachments, &mut held);
    out
}

fn close(
    out: &mut Vec<Message>,
    pending: &mut Vec<(String, String)>,
    attachments: &mut Vec<Content>,
    held: &mut Vec<Message>,
) {
    for (call_id, _) in pending.drain(..) {
        out.push(Message {
            role: Role::Tool,
            content: vec![Content::ToolResult(ToolResult {
                call_id,
                is_error: true,
                content: vec![Content::Text {
                    text: "No result provided".into(),
                    opaque: None,
                }],
            })],
            opaque: None,
        });
    }
    if !attachments.is_empty() {
        out.push(Message {
            role: Role::User,
            content: std::mem::take(attachments),
            opaque: None,
        });
    }
    out.append(held);
}

/// One placeholder per run of images.
fn placeholders(content: Vec<Content>, placeholder: &str) -> Vec<Content> {
    let mut out: Vec<Content> = Vec::with_capacity(content.len());
    for block in content {
        if !matches!(block, Content::Image { .. }) {
            out.push(block);
        } else if !matches!(out.last(), Some(Content::Text { text, .. }) if text == placeholder) {
            out.push(Content::Text {
                text: placeholder.into(),
                opaque: None,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tiechui_model::{ModelId, OpaqueData, ToolCall};

    use super::*;

    fn model(provider: &str) -> ModelRef {
        ModelRef {
            provider: provider.into(),
            model: ModelId(format!("{provider}/m")),
        }
    }

    fn text(text: &str) -> Content {
        Content::Text {
            text: text.into(),
            opaque: None,
        }
    }

    fn image() -> Content {
        Content::Image {
            mime_type: "image/png".into(),
            data: "AA==".into(),
        }
    }

    fn message(role: Role, content: Vec<Content>) -> Message {
        Message {
            role,
            content,
            opaque: None,
        }
    }

    fn assistant() -> Message {
        let signed = Some(OpaqueData("sig".into()));
        Message {
            role: Role::Assistant,
            content: vec![
                Content::Thinking {
                    text: "plan".into(),
                    redacted: false,
                    opaque: signed.clone(),
                },
                Content::Thinking {
                    text: String::new(),
                    redacted: true,
                    opaque: signed.clone(),
                },
                Content::Thinking {
                    text: " ".into(),
                    redacted: false,
                    opaque: None,
                },
                Content::Text {
                    text: "answer".into(),
                    opaque: signed.clone(),
                },
                Content::ToolCall(ToolCall {
                    id: "call|1".into(),
                    name: "read".into(),
                    arguments: json!({}),
                    opaque: signed.clone(),
                }),
            ],
            opaque: signed,
        }
    }

    fn result() -> Message {
        message(
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call_id: "call|1".into(),
                content: vec![text("out"), image(), image(), text("tail"), image()],
                is_error: false,
            })],
        )
    }

    fn normalize(id: &str) -> String {
        id.replace('|', "_")
    }

    #[test]
    fn the_same_model_keeps_replay_data_and_drops_only_empty_thinking() {
        let current = model("a");
        let out = prepare(
            vec![
                (assistant(), Some(current.clone()), None),
                (result(), None, None),
            ],
            &current,
            true,
            true,
            normalize,
        );
        let mut expected = assistant();
        expected.content.remove(2);
        assert_eq!(out, [expected, result()]);
    }

    #[test]
    fn another_model_loses_thinking_and_signatures_and_its_call_ids_are_normalized() {
        let out = prepare(
            vec![
                (assistant(), Some(model("b")), None),
                (result(), None, None),
            ],
            &model("a"),
            true,
            true,
            normalize,
        );
        assert_eq!(
            out[0],
            message(
                Role::Assistant,
                vec![
                    text("answer"),
                    Content::ToolCall(ToolCall {
                        id: "call_1".into(),
                        name: "read".into(),
                        arguments: json!({}),
                        opaque: None,
                    }),
                ],
            )
        );
        let Content::ToolResult(result) = &out[1].content[0] else {
            panic!("tool result expected");
        };
        assert_eq!(result.call_id, "call_1");
    }

    #[test]
    fn images_become_one_placeholder_per_run_without_vision() {
        let user = message(Role::User, vec![image(), image(), text("look")]);
        let history = vec![
            (user.clone(), None, None),
            (assistant(), Some(model("a")), None),
            (result(), None, None),
        ];
        let current = model("a");
        assert_eq!(
            prepare(history.clone(), &current, true, true, normalize)[0],
            user
        );
        let out = prepare(history, &current, false, true, normalize);
        assert_eq!(out[0].content, [text(USER_IMAGE), text("look")]);
        let Content::ToolResult(result) = &out[2].content[0] else {
            panic!("tool result expected");
        };
        assert_eq!(
            result.content,
            [
                text("out"),
                text(TOOL_IMAGE),
                text("tail"),
                text(TOOL_IMAGE)
            ]
        );
    }
}
