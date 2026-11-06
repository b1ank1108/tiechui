//! Ephemeral, measured session data for read-only command contributions.

use crate::{
    model::Role,
    protocol::{ContextItem, ContextSnapshot, ContextView, TokenUsage},
};

#[derive(Debug, Clone)]
pub struct PromptFragment {
    pub label: String,
    pub category: String,
    pub source: String,
    pub text: String,
    /// References included in this fragment, not additional allocations.
    pub references: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct MeasuredMessage {
    pub role: Role,
    pub item: ContextItem,
}

#[derive(Debug, Clone, Default)]
pub struct RequestObservation {
    pub tools_fingerprint: u64,
    pub system: Vec<ContextItem>,
    pub references: Vec<ContextItem>,
    pub tools: Vec<ContextItem>,
    pub added: Vec<MeasuredMessage>,
    pub removed: Vec<MeasuredMessage>,
}

#[derive(Debug, Clone)]
pub struct ContextFold {
    pub before: u64,
    pub after: u64,
    pub prefix: u64,
    /// Capacity pressure overrode the contributor's cache cooldown.
    pub forced: bool,
}

#[derive(Debug, Clone)]
pub struct ResponseUsage {
    pub model: String,
    pub usage: TokenUsage,
    pub reset: bool,
    pub fold: Option<ContextFold>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionView {
    pub folds: usize,
    pub forced_folds: usize,
    pub reclaimed: u64,
    pub model: String,
    pub window: u64,
    pub input_tokens: u64,
    pub provider_usage: Option<TokenUsage>,
    pub system: Vec<ContextItem>,
    pub references: Vec<ContextItem>,
    pub tools: Vec<ContextItem>,
    pub messages: Vec<MeasuredMessage>,
    pub request: Option<RequestObservation>,
    /// Includes four preceding responses for bounded fold attribution at the 128-response boundary.
    pub responses: Vec<ResponseUsage>,
    pub omitted_responses: usize,
    pub unavailable_responses: usize,
}

pub enum CommandResult {
    Text(String),
    Panel {
        snapshot: Box<ContextSnapshot>,
        view: ContextView,
        text: String,
    },
}
