use crate::TokenUsage;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const SUBAGENTS_EVENT: &str = "tiechui.subagents";

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum AgentState {
    Waiting,
    Running,
    Completed,
    Failed,
    Cancelled,
    Skipped,
}
impl AgentState {
    pub fn active(self) -> bool {
        matches!(self, Self::Waiting | Self::Running)
    }
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentTask {
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub fork: bool,
    #[serde(default)]
    pub started_at: i64,
    pub agent: String,
    pub task: String,
    pub state: AgentState,
    pub output: String,
    pub thinking: String,
    pub diagnostic: Option<String>,
    pub output_path: Option<String>,
    pub thinking_path: Option<String>,
    pub usage: TokenUsage,
    pub turns: u64,
    pub model: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentDispatch {
    pub id: String,
    pub session_id: String,
    pub mode: String,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub started_at: i64,
    pub tasks: Vec<AgentTask>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SubagentsView {
    pub records: Vec<AgentDispatch>,
    pub open: bool,
    #[serde(default)]
    pub pending_notifications: usize,
}

impl SubagentsView {
    pub fn background(&self) -> usize {
        self.records
            .iter()
            .flat_map(|record| &record.tasks)
            .filter(|task| task.background && task.state.active())
            .count()
    }

    pub fn has_pending_work(&self) -> bool {
        self.background() > 0 || self.pending_notifications > 0
    }
}
