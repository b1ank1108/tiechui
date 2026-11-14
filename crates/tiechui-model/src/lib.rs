//! Neutral model messages and stream data, independent of client protocol items.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Replay data whose bytes must not be parsed, normalized, or edited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct OpaqueData(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ModelId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelCapabilities {
    pub context_window: u64,
    pub max_output: u64,
    pub reasoning: bool,
    pub images: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelRequest {
    /// Disable explicit prompt-cache writes for one-off requests such as summaries.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub disable_cache: bool,
    /// Cap explicit prompt-cache writes at the short retention, for short-lived sessions such as subagents.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub short_cache: bool,
    /// Stable session key that providers may use to route prompt caching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    pub model: ModelId,
    pub messages: Vec<Message>,
    /// Session estimate of input tokens: latest successful assistant usage plus
    /// estimated trailing messages. Adapters estimate all input when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub tools: Vec<ToolSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningLevel>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningLevel {
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningLevel {
    pub const ALL: [Self; 6] = [
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Xhigh,
        Self::Max,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Content>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opaque: Option<OpaqueData>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opaque: Option<OpaqueData>,
    },
    Image {
        mime_type: String,
        /// Base64-encoded image bytes.
        data: String,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        redacted: bool,
        /// Replay data, including any signature, even when the text is empty.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        opaque: Option<OpaqueData>,
    },
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opaque: Option<OpaqueData>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ToolResult {
    pub call_id: String,
    pub content: Vec<Content>,
    pub is_error: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    /// A provider omitted required accounting fields. Counts remain usable as partial data.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partial: bool,
    /// Uncached input tokens; cache reads and writes are counted separately.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Subset of output, never added to the total token count.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning: u64,
    /// Subset of cache writes retained for one hour; the remainder is short-lived.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write_1h: u64,
    /// Estimated USD in billionths, computed only from configured prices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_nano_usd: Option<u64>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    Length,
    ToolUse,
    Cancelled,
    Error,
}

/// Deltas are ordered by arrival and addressed by content-block index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StreamEvent {
    /// Qualified identity returned by the server when it differs from the request.
    ResponseModel {
        model: ModelId,
    },
    TextDelta {
        index: u32,
        delta: String,
    },
    ThinkingDelta {
        index: u32,
        delta: String,
    },
    ToolCallStart {
        index: u32,
        id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        index: u32,
        /// A fragment that need not be valid JSON until the call is complete.
        delta: String,
    },
    /// The authoritative block, including any opaque replay data.
    ContentComplete {
        index: u32,
        content: Content,
    },
    /// Cumulative usage for this request, replacing the previous snapshot.
    Usage {
        usage: Usage,
    },
    Finish {
        reason: StopReason,
        usage: Usage,
    },
}
