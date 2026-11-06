use crate::TokenUsage;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const SUBAGENTS_EVENT: &str = "tiechui.subagents";

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentTask {
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentDispatch {
    pub id: String,
    pub session_id: String,
    pub mode: String,
    pub tasks: Vec<AgentTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SubagentsView {
    pub records: Vec<AgentDispatch>,
    pub open: bool,
}
