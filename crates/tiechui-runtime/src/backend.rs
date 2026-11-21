//! What an external agent runtime's driver implements. The driver speaks the runtime's own
//! protocol and reports normalized events; `ExternalSession` turns them into the session
//! protocol, persists them and keeps the session's turn structure.

use std::path::PathBuf;

use async_trait::async_trait;
use tiechui_protocol::{
    ApprovalDecision, ApprovalRequest, InputContent, ItemId, KnownItem, ModelInfo, SubagentState,
    ThinkingLevel, TokenUsage, TurnStatus,
};
use tokio::sync::mpsc;

/// What `Backend::start` is given.
#[derive(Debug, Clone)]
pub struct StartParams {
    pub cwd: PathBuf,
    /// The model the client asked for, in the runtime's own naming.
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    /// The permission mode: `rules`, `auto` or `yolo`.
    pub mode: Option<String>,
    /// The external thread of an earlier run of this session; resume it instead of starting one.
    pub resume: Option<String>,
}

/// What `Backend::start` reports.
#[derive(Debug, Clone)]
pub struct Started {
    /// The runtime's own id of the thread, saved in the session so a reopen resumes it.
    pub external_id: String,
    /// False when `resume` was given but the runtime started a new thread instead.
    pub resumed: bool,
    pub model: ModelInfo,
    pub thinking: ThinkingLevel,
}

/// The outcome of `Backend::steer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Steered {
    /// The running turn took the input.
    Accepted,
    /// The runtime cannot take it now (no active turn, a turn the input must not join); the
    /// session queues it to run after the turn.
    Rejected(String),
}

/// An item produced inside a sub-agent rather than by the root thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildOf {
    /// The `ToolCall` item that spawned the sub-agent.
    pub parent_call_id: ItemId,
    pub agent_id: String,
}

/// An item and where it was produced.
#[derive(Debug, Clone, PartialEq)]
pub struct BackendItem {
    pub item: KnownItem,
    /// `Some` for an item of a sub-agent. Its id must differ from every root item's and every
    /// other sub-agent's, such as `<agent id>:<item id>`.
    pub child_of: Option<ChildOf>,
}

impl BackendItem {
    pub fn root(item: KnownItem) -> Self {
        Self {
            item,
            child_of: None,
        }
    }
}

/// A sub-agent's lifecycle change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentUpdate {
    pub agent_id: String,
    pub parent_call_id: ItemId,
    pub name: Option<String>,
    pub state: SubagentState,
    pub message: Option<String>,
}

/// What a backend reports, in order. Root items stream: `ItemStarted`, any number of merged
/// `ItemDelta`s and `ItemCompleted`. Sub-agent items are only ever completed.
#[derive(Debug, Clone, PartialEq)]
pub enum BackendEvent {
    ItemStarted(BackendItem),
    ItemDelta {
        item_id: ItemId,
        delta: String,
    },
    ItemCompleted(BackendItem),
    /// Usage of one model request; send it once per request.
    Usage(TokenUsage),
    /// The model's context window in tokens, when the runtime learns it after `Started`.
    ContextWindow(u64),
    Approval {
        id: String,
        request: ApprovalRequest,
    },
    /// The runtime no longer waits for the approval.
    ApprovalWithdrawn {
        id: String,
    },
    Subagent(SubagentUpdate),
    /// The root thread's turn ended. Nothing else ends a turn.
    TurnCompleted {
        status: TurnStatus,
        error: Option<String>,
    },
    /// Nothing runs any more, neither the root thread nor sub-agents.
    Idle,
    Error(String),
}

/// A driver's connection to one external thread. Calls may wait on the runtime but must carry
/// their own timeouts. Dropping the backend must stop whatever it spawned.
#[async_trait]
pub trait Backend: Send {
    /// Starts the runtime, then starts or resumes the thread. Events arrive on the channel;
    /// when it closes the runtime is gone.
    async fn start(
        &mut self,
        params: StartParams,
    ) -> Result<(Started, mpsc::Receiver<BackendEvent>), String>;

    /// Starts a turn on the thread. The turn ends with `BackendEvent::TurnCompleted`.
    async fn start_turn(&mut self, input: &[InputContent]) -> Result<(), String>;

    /// Adds input to the running turn.
    async fn steer(&mut self, input: &[InputContent]) -> Result<Steered, String>;

    /// Asks the running turn to stop. The session waits for `TurnCompleted`.
    async fn interrupt(&mut self) -> Result<(), String>;

    /// Answers an approval the backend reported. The session calls it once per approval.
    async fn answer_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), String>;

    /// Stops the runtime and everything it started, and returns once they are gone.
    async fn shutdown(&mut self);
}
