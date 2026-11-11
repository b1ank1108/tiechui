use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tiechui_model::{Content, OpaqueData, StopReason, StreamEvent, ToolCall, Usage};

use crate::error::classify;
use crate::transport::Decoder;
use crate::{ErrorKind, ProviderError};

#[derive(Default, Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct Choice {
    delta: Option<Delta>,
    finish_reason: Option<String>,
    usage: Option<WireUsage>,
}

#[derive(Default, Deserialize)]
struct Delta {
    content: Option<String>,
    reasoning_content: Option<String>,
    reasoning: Option<String>,
    reasoning_text: Option<String>,
    tool_calls: Option<Vec<ToolDelta>>,
    extra_content: Option<ExtraContent>,
}

#[derive(Deserialize)]
struct ToolDelta {
    index: Option<u32>,
    id: Option<String>,
    function: Option<FunctionDelta>,
    extra_content: Option<ExtraContent>,
}

#[derive(Deserialize)]
struct ExtraContent {
    google: Option<GoogleExtra>,
}

#[derive(Deserialize)]
struct GoogleExtra {
    thought_signature: Option<String>,
}

fn update_thought_signature(slot: &mut Option<OpaqueData>, extra: Option<ExtraContent>) {
    if let Some(sig) = extra
        .and_then(|extra| extra.google)
        .and_then(|google| google.thought_signature)
        .filter(|sig| !sig.is_empty())
    {
        *slot = Some(OpaqueData(
            json!({ "google": { "thought_signature": sig } }).to_string(),
        ));
    }
}

#[derive(Default, Deserialize)]
struct FunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct TokenDetails {
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct WireUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    completion_tokens_details: Option<TokenDetails>,
    prompt_tokens_details: Option<TokenDetails>,
    prompt_cache_hit_tokens: Option<u64>,
    cached_tokens: Option<u64>,
}

impl WireUsage {
    fn neutral(self) -> Usage {
        let cache_read = self
            .prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
            .or(self.prompt_cache_hit_tokens)
            .or(self.cached_tokens)
            .unwrap_or(0);
        let cache_write = self
            .prompt_tokens_details
            .and_then(|details| details.cache_write_tokens)
            .unwrap_or(0);
        Usage {
            partial: self.prompt_tokens.is_none() || self.completion_tokens.is_none(),
            input: self
                .prompt_tokens
                .unwrap_or(0)
                .saturating_sub(cache_read)
                .saturating_sub(cache_write),
            output: self.completion_tokens.unwrap_or(0),
            cache_read,
            cache_write,
            reasoning: self
                .completion_tokens_details
                .and_then(|details| details.reasoning_tokens)
                .unwrap_or(0),
            ..Usage::default()
        }
    }
}

enum Block {
    Text(String),
    Thinking {
        text: String,
        field: String,
    },
    Tool {
        call: ToolCall,
        fragments: String,
        buffered: Vec<String>,
        started: bool,
        stream_index: Option<u32>,
    },
}

#[derive(Default)]
pub(super) struct State {
    blocks: Vec<Block>,
    text: Option<usize>,
    text_opaque: Option<OpaqueData>,
    thinking: Option<usize>,
    tools: BTreeMap<u32, usize>,
    tool_ids: BTreeMap<String, usize>,
    reason: Option<StopReason>,
    usage: Usage,
    usage_update: Option<Usage>,
    usage_seen: bool,
    finished: bool,
}

impl State {
    fn complete(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        let reason = self
            .reason
            .ok_or_else(|| ProviderError::stream("stream ended without finish_reason"))?;
        let mut events = Vec::new();
        let mut ids = BTreeSet::new();
        for (index, block) in self.blocks.iter().enumerate() {
            let content = match block {
                Block::Text(text) => Content::Text {
                    text: text.clone(),
                    opaque: self.text_opaque.clone(),
                },
                Block::Thinking { text, field } => Content::Thinking {
                    text: text.clone(),
                    redacted: false,
                    opaque: Some(OpaqueData(field.clone())),
                },
                Block::Tool {
                    call,
                    fragments,
                    started,
                    ..
                } => {
                    if !started || !ids.insert(&call.id) {
                        return Err(ProviderError::stream(
                            "incomplete or duplicate tool-call identity",
                        ));
                    }
                    let arguments: Value = if fragments.is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(fragments).map_err(|_| {
                            ProviderError::stream("invalid completed tool-call JSON")
                        })?
                    };
                    if !arguments.is_object() {
                        return Err(ProviderError::stream(
                            "tool-call arguments must be an object",
                        ));
                    }
                    Content::ToolCall(ToolCall {
                        arguments,
                        ..call.clone()
                    })
                }
            };
            events.push(StreamEvent::ContentComplete {
                index: index as u32,
                content,
            });
        }
        self.usage.partial |= !self.usage_seen;
        events.push(StreamEvent::Finish {
            reason,
            usage: self.usage,
        });
        self.finished = true;
        Ok(events)
    }
}

impl Decoder for State {
    fn take_usage(&mut self) -> Option<Usage> {
        self.usage_update.take()
    }
    fn accept(
        &mut self,
        _name: &str,
        data: &str,
        delay: Option<Duration>,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        if data.trim() == "[DONE]" {
            return self.complete();
        }
        let value: Value = serde_json::from_str(data)
            .map_err(|_| ProviderError::stream("invalid SSE event JSON"))?;
        if value.get("error").is_some_and(|error| !error.is_null()) {
            return Err(classify(None, &value, delay));
        }
        let chunk: Chunk = serde_json::from_value(value)
            .map_err(|_| ProviderError::stream("invalid completion chunk"))?;
        let mut events = Vec::new();
        let choice = chunk.choices.into_iter().next();
        let has_usage = chunk.usage.is_some();
        // Usage-only chunks arrive after finish_reason, before the optional [DONE].
        if let Some(usage) = chunk.usage {
            self.usage_seen = true;
            self.usage = usage.neutral();
            self.usage_update = Some(self.usage);
        }
        let Some(choice) = choice else {
            return Ok(events);
        };
        if !has_usage && let Some(usage) = choice.usage {
            self.usage_seen = true;
            self.usage = usage.neutral();
            self.usage_update = Some(self.usage);
        }
        if let Some(delta) = choice.delta {
            let has_content = delta.content.as_ref().is_some_and(|text| !text.is_empty())
                || delta
                    .reasoning_content
                    .as_ref()
                    .is_some_and(|text| !text.is_empty())
                || delta
                    .reasoning
                    .as_ref()
                    .is_some_and(|text| !text.is_empty())
                || delta
                    .reasoning_text
                    .as_ref()
                    .is_some_and(|text| !text.is_empty())
                || delta
                    .tool_calls
                    .as_ref()
                    .is_some_and(|calls| !calls.is_empty());
            if self.reason.is_some() && has_content {
                return Err(ProviderError::stream("content after finish_reason"));
            }
            let reasoning = [
                ("reasoning_content", delta.reasoning_content),
                ("reasoning", delta.reasoning),
                ("reasoning_text", delta.reasoning_text),
            ]
            .into_iter()
            .find(|(_, text)| text.as_ref().is_some_and(|text| !text.is_empty()));
            if let Some((field, Some(text))) = reasoning {
                let index = *self.thinking.get_or_insert_with(|| {
                    self.blocks.push(Block::Thinking {
                        text: String::new(),
                        field: field.into(),
                    });
                    self.blocks.len() - 1
                });
                if let Block::Thinking { text: full, .. } = &mut self.blocks[index] {
                    full.push_str(&text);
                }
                events.push(StreamEvent::ThinkingDelta {
                    index: index as u32,
                    delta: text,
                });
            }
            if let Some(text) = delta.content.filter(|text| !text.is_empty()) {
                let index = *self.text.get_or_insert_with(|| {
                    self.blocks.push(Block::Text(String::new()));
                    self.blocks.len() - 1
                });
                if let Block::Text(full) = &mut self.blocks[index] {
                    full.push_str(&text);
                }
                events.push(StreamEvent::TextDelta {
                    index: index as u32,
                    delta: text,
                });
            }
            update_thought_signature(&mut self.text_opaque, delta.extra_content);
            for tool in delta.tool_calls.unwrap_or_default() {
                // An existing index takes priority; later ids alias its first identity.
                let index = tool
                    .index
                    .and_then(|index| self.tools.get(&index).copied())
                    .or_else(|| {
                        tool.id
                            .as_ref()
                            .and_then(|id| self.tool_ids.get(id).copied())
                    })
                    .unwrap_or_else(|| {
                        self.blocks.push(Block::Tool {
                            call: ToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: json!({}),
                                opaque: None,
                            },
                            fragments: String::new(),
                            buffered: Vec::new(),
                            started: false,
                            stream_index: tool.index,
                        });
                        let index = self.blocks.len() - 1;
                        if let Some(stream_index) = tool.index {
                            self.tools.insert(stream_index, index);
                        }
                        index
                    });
                let Block::Tool {
                    call,
                    fragments,
                    buffered,
                    started,
                    stream_index,
                } = &mut self.blocks[index]
                else {
                    unreachable!()
                };
                if stream_index.is_none()
                    && let Some(incoming) = tool.index
                {
                    *stream_index = Some(incoming);
                    self.tools.insert(incoming, index);
                }
                update_thought_signature(&mut call.opaque, tool.extra_content);
                if let Some(id) = tool.id.filter(|id| !id.is_empty()) {
                    self.tool_ids.insert(id.clone(), index);
                    if call.id.is_empty() {
                        call.id = id;
                    }
                }
                let function = tool.function.unwrap_or_default();
                if let Some(name) = function.name.filter(|name| !name.is_empty())
                    && call.name.is_empty()
                {
                    call.name = name;
                }
                if let Some(delta) = function.arguments.filter(|text| !text.is_empty()) {
                    fragments.push_str(&delta);
                    buffered.push(delta);
                }
                if !*started && !call.id.is_empty() && !call.name.is_empty() {
                    *started = true;
                    events.push(StreamEvent::ToolCallStart {
                        index: index as u32,
                        id: call.id.clone(),
                        name: call.name.clone(),
                    });
                }
                if *started {
                    for delta in buffered.drain(..) {
                        events.push(StreamEvent::ToolCallArgumentsDelta {
                            index: index as u32,
                            delta,
                        });
                    }
                }
            }
        }
        if let Some(reason) = choice.finish_reason.filter(|reason| !reason.is_empty()) {
            self.reason = Some(match reason.as_str() {
                "stop" | "end" => StopReason::EndTurn,
                "length" => StopReason::Length,
                "tool_calls" | "function_call" => StopReason::ToolUse,
                "content_filter" => {
                    return Err(ProviderError::new(
                        ErrorKind::InvalidRequest,
                        "provider refused to complete the response",
                    ));
                }
                _ => return Err(ProviderError::stream("unrecognized stop reason")),
            });
        }
        Ok(events)
    }

    fn eof(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        self.complete()
    }
    fn finished(&self) -> bool {
        self.finished
    }
}
