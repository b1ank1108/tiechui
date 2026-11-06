use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tiechui_model::{Message, ModelId, ReasoningLevel, StopReason, Usage};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub format_version: u32,
    pub id: String,
    pub cwd: std::path::PathBuf,
    /// Unix milliseconds.
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub model: ModelId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    /// None attaches this entry to the session header (the implicit tree root).
    pub parent_id: Option<String>,
    /// Unix milliseconds.
    pub timestamp: u64,
    #[serde(flatten)]
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EntryKind {
    Message {
        message: Message,
        /// This assistant request's final usage, including zero when none was reported.
        /// Absent on older entries; accounting remains in the Usage entries.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Required for assistant messages; records origin without transforming replay.
        model: Option<ModelRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
        /// Requested effort that produced this assistant; absent in older transcripts.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<ReasoningLevel>,
        /// Tool-result presentation data; never part of model context.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
    },
    ModelChange {
        model: ModelRef,
    },
    ThinkingChange {
        /// None means thinking is off.
        level: Option<ReasoningLevel>,
    },
    Usage {
        usage: Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<ModelRef>,
        /// First usage entry of this request; later snapshots replace its accounting.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    Compaction {
        summary: String,
        #[serde(default)]
        tokens_before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// `restored[].text` is replayed after the summary.
        #[serde(default)]
        details: Value,
        #[serde(default)]
        from_extension: bool,
        /// None, or this compaction's own ID, retains no earlier entries.
        first_kept_entry_id: Option<String>,
        /// Checkpoint of the current system message, before the summary.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_message: Option<Message>,
    },
    Label {
        target_id: String,
        label: Option<String>,
    },
    CustomMessage {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        hidden: bool,
        #[serde(rename = "custom_kind")]
        kind: String,
        text: String,
        details: Value,
    },
    Custom {
        #[serde(rename = "custom_kind")]
        kind: String,
        data: Value,
    },
    // Validation rejects known tags here, so malformed known payloads are errors.
    #[serde(untagged)]
    Unknown {
        kind: String,
        #[serde(flatten)]
        fields: Map<String, Value>,
    },
}

impl EntryKind {
    pub fn context_message(&self) -> Option<(Message, Option<ModelRef>, Option<StopReason>)> {
        match self {
            Self::Message {
                message,
                model,
                stop_reason,
                ..
            } => Some((message.clone(), model.clone(), *stop_reason)),
            Self::CustomMessage { text, .. } => Some((
                Message {
                    role: tiechui_model::Role::User,
                    content: vec![tiechui_model::Content::Text {
                        text: text.clone(),
                        opaque: None,
                    }],
                    opaque: None,
                },
                None,
                None,
            )),
            _ => None,
        }
    }
}
