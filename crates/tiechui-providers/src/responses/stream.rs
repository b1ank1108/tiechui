use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tiechui_model::{Content, OpaqueData, StopReason, StreamEvent, ToolCall, Usage};

use crate::error::classify;
use crate::transport::Decoder;
use crate::{ErrorKind, ProviderError};

#[derive(Deserialize)]
struct TokenDetails {
    cached_tokens: Option<u64>,
    cache_write_tokens: Option<u64>,
    reasoning_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct WireUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    input_tokens_details: Option<TokenDetails>,
    output_tokens_details: Option<TokenDetails>,
}

impl WireUsage {
    fn neutral(self) -> Usage {
        let (cache_read, cache_write) = self.input_tokens_details.map_or((0, 0), |details| {
            (
                details.cached_tokens.unwrap_or(0),
                details.cache_write_tokens.unwrap_or(0),
            )
        });
        Usage {
            partial: self.input_tokens.is_none() || self.output_tokens.is_none(),
            input: self
                .input_tokens
                .unwrap_or(0)
                .saturating_sub(cache_read)
                .saturating_sub(cache_write),
            output: self.output_tokens.unwrap_or(0),
            cache_read,
            cache_write,
            reasoning: self
                .output_tokens_details
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
        opaque: Option<String>,
    },
    Tool {
        call: ToolCall,
        arguments: String,
    },
}

#[derive(Default)]
pub(super) struct State {
    blocks: Vec<Block>,
    /// Output index to block index; neutral indices follow first arrival.
    items: BTreeMap<u64, usize>,
    usage: Usage,
    usage_update: Option<Usage>,
    usage_seen: bool,
    finished: bool,
}

fn joined(parts: &Value, kinds: &[&str], separator: &str) -> String {
    parts
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| kinds.iter().any(|kind| part["type"] == *kind))
        .filter_map(|part| {
            part[if part["type"] == "refusal" {
                "refusal"
            } else {
                "text"
            }]
            .as_str()
        })
        .collect::<Vec<_>>()
        .join(separator)
}

impl State {
    /// Starts the block of a known output item; other item types are skipped.
    fn add(
        &mut self,
        output: u64,
        item: &Value,
        events: &mut Vec<StreamEvent>,
    ) -> Result<(), ProviderError> {
        let block = match item["type"].as_str() {
            Some("message") => Block::Text(String::new()),
            Some("reasoning") => Block::Thinking {
                text: String::new(),
                opaque: None,
            },
            Some("function_call") => {
                let (Some(id), Some(name)) = (
                    item["call_id"].as_str().filter(|id| !id.is_empty()),
                    item["name"].as_str().filter(|name| !name.is_empty()),
                ) else {
                    return Err(ProviderError::stream("incomplete tool-call identity"));
                };
                events.push(StreamEvent::ToolCallStart {
                    index: self.blocks.len() as u32,
                    id: id.into(),
                    name: name.into(),
                });
                Block::Tool {
                    call: ToolCall {
                        id: id.into(),
                        name: name.into(),
                        arguments: json!({}),
                        opaque: None,
                    },
                    arguments: String::new(),
                }
            }
            _ => return Ok(()),
        };
        if self.items.insert(output, self.blocks.len()).is_some() {
            return Err(ProviderError::stream("duplicate output item index"));
        }
        self.blocks.push(block);
        Ok(())
    }

    fn block(&mut self, output: Option<u64>) -> Result<(u32, &mut Block), ProviderError> {
        let index = output
            .and_then(|output| self.items.get(&output).copied())
            .ok_or_else(|| ProviderError::stream("delta for an unknown output item"))?;
        Ok((index as u32, &mut self.blocks[index]))
    }

    fn record(&mut self, response: &Value) {
        if let Ok(usage) = WireUsage::deserialize(&response["usage"]) {
            self.usage_seen = true;
            self.usage = usage.neutral();
            self.usage_update = Some(self.usage);
        }
    }

    fn complete(&mut self, reason: StopReason) -> Result<Vec<StreamEvent>, ProviderError> {
        let mut events = Vec::new();
        let mut ids = BTreeSet::new();
        for (index, block) in self.blocks.iter().enumerate() {
            let content = match block {
                Block::Text(text) => Content::Text {
                    text: text.clone(),
                    opaque: None,
                },
                Block::Thinking { text, opaque } => Content::Thinking {
                    text: text.clone(),
                    redacted: false,
                    opaque: opaque.clone().map(OpaqueData),
                },
                Block::Tool { call, arguments } => {
                    if !ids.insert(&call.id) {
                        return Err(ProviderError::stream("duplicate tool-call identity"));
                    }
                    let arguments: Value = if arguments.is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(arguments).map_err(|_| {
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
        name: &str,
        data: &str,
        delay: Option<Duration>,
    ) -> Result<Vec<StreamEvent>, ProviderError> {
        let value: Value = serde_json::from_str(data)
            .map_err(|_| ProviderError::stream("invalid SSE event JSON"))?;
        let output = value["output_index"].as_u64();
        let mut events = Vec::new();
        match value["type"].as_str().unwrap_or(name) {
            "response.output_item.added" => {
                let output =
                    output.ok_or_else(|| ProviderError::stream("output item without an index"))?;
                self.add(output, &value["item"], &mut events)?;
            }
            // Summary parts read as paragraphs.
            "response.reasoning_summary_part.added" => {
                let (index, block) = self.block(output)?;
                if let Block::Thinking { text, .. } = block
                    && !text.is_empty()
                {
                    text.push_str("\n\n");
                    events.push(StreamEvent::ThinkingDelta {
                        index,
                        delta: "\n\n".into(),
                    });
                }
            }
            kind @ ("response.output_text.delta"
            | "response.refusal.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.function_call_arguments.delta") => {
                let delta = value["delta"].as_str().unwrap_or_default().to_owned();
                let (index, block) = self.block(output)?;
                let event = match (kind, block) {
                    (
                        "response.output_text.delta" | "response.refusal.delta",
                        Block::Text(text),
                    ) => {
                        text.push_str(&delta);
                        StreamEvent::TextDelta { index, delta }
                    }
                    (
                        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta",
                        Block::Thinking { text, .. },
                    ) => {
                        text.push_str(&delta);
                        StreamEvent::ThinkingDelta { index, delta }
                    }
                    ("response.function_call_arguments.delta", Block::Tool { arguments, .. }) => {
                        arguments.push_str(&delta);
                        StreamEvent::ToolCallArgumentsDelta { index, delta }
                    }
                    _ => return Err(ProviderError::stream("delta for another output item type")),
                };
                if !value["delta"].as_str().unwrap_or_default().is_empty() {
                    events.push(event);
                }
            }
            // The completed item is authoritative, and also starts an item never announced.
            "response.output_item.done" => {
                let output =
                    output.ok_or_else(|| ProviderError::stream("output item without an index"))?;
                let item = &value["item"];
                if !self.items.contains_key(&output) {
                    self.add(output, item, &mut events)?;
                }
                let Some(&index) = self.items.get(&output) else {
                    return Ok(events);
                };
                match &mut self.blocks[index] {
                    Block::Text(text) => {
                        if item["content"].is_array() {
                            *text = joined(&item["content"], &["output_text", "refusal"], "");
                        }
                    }
                    Block::Thinking { text, opaque } => {
                        let summary = joined(&item["summary"], &["summary_text"], "\n\n");
                        let content = joined(&item["content"], &["reasoning_text"], "\n\n");
                        if !summary.is_empty() || !content.is_empty() {
                            *text = if summary.is_empty() { content } else { summary };
                        }
                        if item["encrypted_content"].is_string() {
                            let mut replay = item.clone();
                            if let Some(fields) = replay.as_object_mut() {
                                fields.remove("id");
                                fields.remove("status");
                            }
                            *opaque = Some(replay.to_string());
                        }
                    }
                    Block::Tool { arguments, .. } => {
                        if let Some(complete) = item["arguments"].as_str() {
                            if let Some(rest) = complete.strip_prefix(arguments.as_str())
                                && !rest.is_empty()
                            {
                                events.push(StreamEvent::ToolCallArgumentsDelta {
                                    index: index as u32,
                                    delta: rest.into(),
                                });
                            }
                            *arguments = complete.into();
                        }
                    }
                }
            }
            "response.completed" => {
                self.record(&value["response"]);
                let tools = self
                    .blocks
                    .iter()
                    .any(|block| matches!(block, Block::Tool { .. }));
                return self.complete(if tools {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                });
            }
            "response.incomplete" => {
                self.record(&value["response"]);
                return match value["response"]["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => self.complete(StopReason::Length),
                    Some("content_filter") => Err(ProviderError::new(
                        ErrorKind::InvalidRequest,
                        "provider refused to complete the response",
                    )),
                    _ => Err(ProviderError::stream("unrecognized incomplete reason")),
                };
            }
            "response.failed" => {
                self.record(&value["response"]);
                let error = &value["response"]["error"];
                return Err(if error.is_object() {
                    classify(None, &json!({"error":error}), delay)
                } else {
                    ProviderError::stream("the response failed without an error")
                });
            }
            "error" => return Err(classify(None, &value, delay)),
            _ => {}
        }
        Ok(events)
    }

    fn eof(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        Err(ProviderError::stream(
            "stream ended before the response completed",
        ))
    }

    fn finished(&self) -> bool {
        self.finished
    }
}
