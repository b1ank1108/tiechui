use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tiechui_model::{Content, ModelId, OpaqueData, StopReason, StreamEvent, ToolCall, Usage};

use crate::error::classify;
use crate::{ErrorKind, ProviderError};

#[derive(Deserialize, Default)]
struct WireUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_creation: Option<CacheCreation>,
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Deserialize)]
struct CacheCreation {
    ephemeral_1h_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct OutputDetails {
    thinking_tokens: Option<u64>,
}

impl WireUsage {
    fn update(self, usage: &mut Usage) {
        if let Some(value) = self.input_tokens {
            usage.input = value;
        }
        if let Some(value) = self.output_tokens {
            usage.output = value;
        }
        if let Some(value) = self.cache_read_input_tokens {
            usage.cache_read = value;
        }
        if let Some(value) = self.cache_creation_input_tokens {
            usage.cache_write = value;
        }
        if let Some(value) = self
            .cache_creation
            .and_then(|details| details.ephemeral_1h_input_tokens)
        {
            usage.cache_write_1h = value;
        }
        if let Some(value) = self
            .output_tokens_details
            .and_then(|details| details.thinking_tokens)
        {
            usage.reasoning = value;
        }
    }
}

#[derive(Deserialize)]
struct StartMessage {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Deserialize)]
struct MessageDelta {
    stop_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireEvent {
    MessageStart {
        message: StartMessage,
    },
    ContentBlockStart {
        index: u32,
        content_block: WireBlock,
    },
    ContentBlockDelta {
        index: u32,
        delta: Value,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        delta: MessageDelta,
        usage: Option<WireUsage>,
    },
    MessageStop,
    Error {
        error: Value,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireBlock {
    Text {
        #[serde(default)]
        text: String,
    },
    Thinking {
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        signature: String,
    },
    RedactedThinking {
        data: String,
    },
    Fallback,
    ToolUse {
        id: String,
        name: String,
        #[serde(default = "empty_object")]
        input: Value,
    },
    #[serde(other)]
    Unsupported,
}

fn empty_object() -> Value {
    json!({})
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum WireDelta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Unsupported,
}

enum Block {
    Text(String),
    Thinking { text: String, signature: String },
    RedactedThinking(String),
    Tool { call: ToolCall, fragments: String },
}

#[derive(Default)]
pub(super) struct State {
    pub(super) requested_model: Option<ModelId>,
    started: bool,
    finished: bool,
    seen: BTreeSet<u32>,
    ignored: BTreeSet<u32>,
    blocks: BTreeMap<u32, Block>,
    usage: Usage,
    usage_update: Option<Usage>,
    input_seen: bool,
    output_seen: bool,
    reason: Option<StopReason>,
}

impl State {
    pub(super) fn new(model: ModelId) -> Self {
        Self {
            requested_model: Some(model),
            ..Self::default()
        }
    }
    pub(super) fn finished(&self) -> bool {
        self.finished
    }

    pub(super) fn accept(
        &mut self,
        name: &str,
        data: &str,
        retry_after: Option<Duration>,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        if !matches!(
            name,
            "message"
                | "message_start"
                | "content_block_start"
                | "content_block_delta"
                | "content_block_stop"
                | "message_delta"
                | "message_stop"
                | "error"
        ) {
            return Ok(Vec::new());
        }
        let event: WireEvent = serde_json::from_str(data)
            .map_err(|_| ProviderError::stream("invalid SSE event JSON"))?;
        let mut events = Vec::new();
        match event {
            WireEvent::Error { error } => return Err(classify(None, &error, retry_after)),
            WireEvent::Unknown => {}
            WireEvent::MessageStart { message } if !self.started => {
                self.started = true;
                if let Some(model) = message.model.filter(|model| !model.is_empty())
                    && let Some((provider, requested)) = self
                        .requested_model
                        .as_ref()
                        .and_then(|id| id.0.split_once('/'))
                    && model != requested
                {
                    events.push(StreamEvent::ResponseModel {
                        model: ModelId(format!("{provider}/{model}")),
                    });
                }
                if let Some(usage) = message.usage {
                    self.input_seen |= usage.input_tokens.is_some();
                    self.output_seen |= usage.output_tokens.is_some();
                    usage.update(&mut self.usage);
                    self.usage.partial = !self.input_seen || !self.output_seen;
                    events.push(StreamEvent::Usage { usage: self.usage });
                }
            }
            _ if !self.started => return Err(ProviderError::stream("event before message_start")),
            WireEvent::MessageStart { .. } => {
                return Err(ProviderError::stream("duplicate message_start"));
            }
            WireEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                match content_block {
                    WireBlock::Fallback if !self.seen.is_empty() => {
                        return Err(ProviderError::stream(
                            "Anthropic performed an unsupported mid-output model fallback",
                        ));
                    }
                    WireBlock::Fallback | WireBlock::Unsupported => {
                        self.ignored.insert(index);
                        return Ok(events);
                    }
                    _ => {}
                }
                self.ignored.remove(&index);
                if !self.seen.insert(index) {
                    return Err(ProviderError::stream("duplicate content block index"));
                }
                let block = match content_block {
                    WireBlock::Text { text } => {
                        if !text.is_empty() {
                            events.push(StreamEvent::TextDelta {
                                index,
                                delta: text.clone(),
                            });
                        }
                        Block::Text(text)
                    }
                    WireBlock::Thinking {
                        thinking,
                        signature,
                    } => {
                        if !thinking.is_empty() {
                            events.push(StreamEvent::ThinkingDelta {
                                index,
                                delta: thinking.clone(),
                            });
                        }
                        Block::Thinking {
                            text: thinking,
                            signature,
                        }
                    }
                    WireBlock::RedactedThinking { data } => Block::RedactedThinking(data),
                    WireBlock::ToolUse { id, name, input } => {
                        if !super::request::valid_id(&id) || name.is_empty() || !input.is_object() {
                            return Err(ProviderError::stream("invalid tool-call block"));
                        }
                        events.push(StreamEvent::ToolCallStart {
                            index,
                            id: id.clone(),
                            name: name.clone(),
                        });
                        Block::Tool {
                            call: ToolCall {
                                id,
                                name,
                                arguments: input,
                                opaque: None,
                            },
                            fragments: String::new(),
                        }
                    }
                    WireBlock::Fallback | WireBlock::Unsupported => unreachable!(),
                };
                self.blocks.insert(index, block);
            }
            WireEvent::ContentBlockDelta { index, delta } => {
                if self.ignored.contains(&index) {
                    return Ok(events);
                }
                let block = self
                    .blocks
                    .get_mut(&index)
                    .ok_or_else(|| ProviderError::stream("delta for inactive content block"))?;
                let delta: WireDelta = serde_json::from_value(delta)
                    .map_err(|_| ProviderError::stream("invalid content delta"))?;
                match (block, delta) {
                    (_, WireDelta::Unsupported) => {}
                    (Block::Text(text), WireDelta::TextDelta { text: delta }) => {
                        text.push_str(&delta);
                        events.push(StreamEvent::TextDelta { index, delta });
                    }
                    (
                        Block::Thinking { text, .. },
                        WireDelta::ThinkingDelta { thinking: delta },
                    ) => {
                        text.push_str(&delta);
                        events.push(StreamEvent::ThinkingDelta { index, delta });
                    }
                    (
                        Block::Thinking { signature, .. },
                        WireDelta::SignatureDelta { signature: delta },
                    ) => signature.push_str(&delta),
                    (
                        Block::Tool { fragments, .. },
                        WireDelta::InputJsonDelta {
                            partial_json: delta,
                        },
                    ) => {
                        fragments.push_str(&delta);
                        events.push(StreamEvent::ToolCallArgumentsDelta { index, delta });
                    }
                    _ => {
                        return Err(ProviderError::stream(
                            "content delta does not match its block",
                        ));
                    }
                }
            }
            WireEvent::ContentBlockStop { index } => {
                if self.ignored.contains(&index) {
                    return Ok(events);
                }
                let block = self
                    .blocks
                    .remove(&index)
                    .ok_or_else(|| ProviderError::stream("stop for inactive content block"))?;
                let content = match block {
                    Block::Text(text) => Content::Text { text, opaque: None },
                    Block::Thinking { text, signature } => Content::Thinking {
                        text,
                        redacted: false,
                        opaque: (!signature.is_empty()).then_some(OpaqueData(signature)),
                    },
                    Block::RedactedThinking(data) => Content::Thinking {
                        text: String::new(),
                        redacted: true,
                        opaque: Some(OpaqueData(data)),
                    },
                    Block::Tool {
                        mut call,
                        fragments,
                    } => {
                        if !fragments.is_empty() {
                            call.arguments = serde_json::from_str(&fragments).map_err(|_| {
                                ProviderError::stream("invalid completed tool-call JSON")
                            })?;
                        }
                        if !call.arguments.is_object() {
                            return Err(ProviderError::stream(
                                "tool-call arguments must be an object",
                            ));
                        }
                        Content::ToolCall(call)
                    }
                };
                events.push(StreamEvent::ContentComplete { index, content });
            }
            WireEvent::MessageDelta { delta, usage } => {
                if let Some(usage) = usage {
                    self.input_seen |= usage.input_tokens.is_some();
                    self.output_seen |= usage.output_tokens.is_some();
                    usage.update(&mut self.usage);
                    self.usage.partial = !self.input_seen || !self.output_seen;
                    self.usage_update = Some(self.usage);
                }
                if let Some(reason) = delta.stop_reason {
                    self.reason = Some(match reason.as_str() {
                        "end_turn" | "stop_sequence" | "pause_turn" => StopReason::EndTurn,
                        "max_tokens" => StopReason::Length,
                        "tool_use" => StopReason::ToolUse,
                        "refusal" | "sensitive" => {
                            return Err(ProviderError::new(
                                ErrorKind::InvalidRequest,
                                "provider refused to complete the response",
                            ));
                        }
                        _ => return Err(ProviderError::stream("unrecognized stop reason")),
                    });
                }
            }
            WireEvent::MessageStop => {
                if !self.blocks.is_empty() {
                    return Err(ProviderError::stream(
                        "message_stop before all content blocks completed",
                    ));
                }
                let reason = self
                    .reason
                    .ok_or_else(|| ProviderError::stream("message_stop without a stop reason"))?;
                self.finished = true;
                self.usage.partial = !self.input_seen || !self.output_seen;
                events.push(StreamEvent::Finish {
                    reason,
                    usage: self.usage,
                });
            }
        }
        Ok(events)
    }
}

impl crate::transport::Decoder for State {
    fn take_usage(&mut self) -> Option<Usage> {
        self.usage_update.take()
    }
    fn accept(
        &mut self,
        name: &str,
        data: &str,
        delay: Option<Duration>,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        self.accept(name, data, delay)
    }

    fn eof(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        Err(ProviderError::stream("stream ended before message_stop"))
    }

    fn finished(&self) -> bool {
        self.finished()
    }
}
