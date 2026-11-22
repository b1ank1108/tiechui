use serde::Deserialize;
use serde_json::{Value, json};
use tiechui_protocol::InputContent;

/// One line from `claude`. Every field is optional: which ones a frame carries depends on its
/// `type` (and `subtype`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Incoming {
    #[serde(rename = "type")]
    pub kind: String,
    pub subtype: Option<String>,
    /// Set on every frame a sub-agent produced: the call that spawned the sub-agent.
    pub parent_tool_use_id: Option<String>,
    /// The sub-agent that produced the frame; the `task_*` frames call it `task_id`.
    pub agent_id: Option<String>,
    /// `system` `task_started` and `task_notification`: a background or sub-agent task, the call
    /// that started it, and how it ended.
    pub task_id: Option<String>,
    pub task_type: Option<String>,
    pub tool_use_id: Option<String>,
    pub subagent_type: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
    pub summary: Option<String>,
    /// `control_request` and `control_response`.
    pub request_id: Option<String>,
    pub request: Option<Request>,
    pub response: Option<ControlResponse>,
    /// `stream_event`.
    pub event: Option<StreamEvent>,
    pub api_message_id: Option<String>,
    /// `assistant` and `user`.
    pub message: Option<Message>,
    /// `result`.
    pub is_error: bool,
    pub result: Option<String>,
    pub errors: Vec<String>,
    pub terminal_reason: Option<String>,
    #[serde(rename = "modelUsage")]
    pub model_usage: std::collections::BTreeMap<String, ModelUsage>,
}

/// What answering a `control_request` takes, for one whose other fields do not parse.
#[derive(Debug, Deserialize)]
pub struct RequestIdOnly {
    #[serde(rename = "type")]
    pub kind: String,
    pub request_id: String,
}

/// `can_use_tool` carries the tool, its input, the id of its `tool_use` block, a short
/// description and the permission updates Claude suggests for allowing it again
/// (`[{"type": "setMode", "mode": "acceptEdits", "destination": "session"}]` for a file write).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Request {
    pub subtype: String,
    pub tool_name: Option<String>,
    pub input: Value,
    pub tool_use_id: Option<String>,
    pub description: Option<String>,
    pub permission_suggestions: Value,
    /// Set when one of Claude's sub-agents asks.
    pub agent_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ControlResponse {
    pub subtype: String,
    pub request_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct StreamEvent {
    #[serde(rename = "type")]
    pub kind: String,
    pub index: u64,
    pub content_block: Option<Block>,
    pub delta: Option<Delta>,
    pub usage: Option<Usage>,
    pub message: Option<StreamMessage>,
}

/// `message_start`'s message. Its usage is the only one that breaks the cache write down.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct StreamMessage {
    pub id: String,
    pub usage: Usage,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Delta {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    pub thinking: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Message {
    pub id: Option<String>,
    /// A string for plain user text, a list of blocks otherwise.
    pub content: Value,
}

impl Message {
    pub fn blocks(&self) -> Vec<Block> {
        match &self.content {
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|block| serde_json::from_value(block.clone()).ok())
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// A content block of any kind: `text`, `thinking`, `tool_use` or `tool_result`.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Block {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
    pub thinking: String,
    pub id: String,
    pub name: String,
    pub input: Value,
    pub tool_use_id: String,
    pub content: Value,
    pub is_error: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_creation: CacheCreation,
    pub output_tokens_details: OutputDetails,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CacheCreation {
    pub ephemeral_1h_input_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct OutputDetails {
    pub thinking_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ModelUsage {
    #[serde(rename = "contextWindow")]
    pub context_window: u64,
}

pub fn initialize_request(id: &str) -> Value {
    json!({"type": "control_request", "request_id": id, "request": {"subtype": "initialize"}})
}

pub fn interrupt_request(id: &str) -> Value {
    json!({"type": "control_request", "request_id": id, "request": {"subtype": "interrupt"}})
}

pub fn user_message(content: &[InputContent]) -> Value {
    let blocks: Vec<Value> = content
        .iter()
        .map(|part| match part {
            InputContent::Text { text } => json!({"type": "text", "text": text}),
            InputContent::Image { mime_type, data } => json!({"type": "image",
                "source": {"type": "base64", "media_type": mime_type, "data": data}}),
        })
        .collect();
    json!({"type": "user", "message": {"role": "user", "content": blocks}})
}

pub fn control_success(request_id: &str, response: Value) -> Value {
    json!({"type": "control_response", "response": {
        "subtype": "success", "request_id": request_id, "response": response}})
}

pub fn control_error(request_id: &str, error: &str) -> Value {
    json!({"type": "control_response", "response": {
        "subtype": "error", "request_id": request_id, "error": error}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_with_unknown_keys_and_without_optional_ones_parses() {
        let incoming: Incoming = serde_json::from_value(json!({
            "type": "assistant",
            "added_later": {"nested": [1, 2]},
            "message": {"id": "msg_1", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "caller": "direct"}]},
        }))
        .unwrap();
        assert_eq!(incoming.kind, "assistant");
        assert_eq!(incoming.subtype, None);
        assert!(incoming.errors.is_empty());
        let blocks = incoming.message.unwrap().blocks();
        assert_eq!(blocks.len(), 1);
        assert_eq!(
            (blocks[0].id.as_str(), blocks[0].name.as_str()),
            ("toolu_1", "Bash")
        );
        assert_eq!(blocks[0].input, Value::Null);
    }
}
