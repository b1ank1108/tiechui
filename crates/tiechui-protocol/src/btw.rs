//! Ephemeral side conversation presentation; transported as a module extension event.

use crate::{InputContent, ModelInfo, ThinkingLevel, TokenUsage};
use serde::{Deserialize, Serialize};

pub const BTW_EVENT: &str = "btw";
pub const BTW_SHARE: &str = "tiechui.btw.shared";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BtwAction {
    Send { content: Vec<InputContent> },
    Stop,
    Close,
    Share,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BtwEvent {
    Update {
        view: BtwView,
        open: bool,
    },
    Closed,
    Models {
        models: Vec<ModelInfo>,
        current: Option<ModelInfo>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtwView {
    pub id: String,
    pub model: ModelInfo,
    pub thinking: ThinkingLevel,
    pub messages: Vec<BtwMessage>,
    pub busy: bool,
    pub context: Option<TokenUsage>,
    pub total: TokenUsage,
    pub notice: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtwMessage {
    pub user: bool,
    pub text: String,
    pub images: Vec<InputContent>,
    pub thinking: String,
    pub complete: bool,
    pub interrupted: bool,
    pub error: Option<String>,
}
