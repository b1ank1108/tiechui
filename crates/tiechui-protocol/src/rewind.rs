use crate::InputContent;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A branch entry exposed to contributions, with model-only data omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BranchEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: u64,
    pub content: Option<Vec<InputContent>>,
    pub custom: Option<(String, serde_json::Value)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LineStats {
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckpointStats {
    pub files: std::collections::BTreeMap<String, Option<LineStats>>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RewindRow {
    pub session_id: String,
    pub entry_id: String,
    pub text: String,
    pub timestamp: u64,
    pub stats: Option<CheckpointStats>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RewindPage {
    Messages,
    Checking,
    Actions,
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RewindPanel {
    pub rows: Vec<RewindRow>,
    pub selected: usize,
    pub page: RewindPage,
    pub preview: Option<CheckpointStats>,
}
