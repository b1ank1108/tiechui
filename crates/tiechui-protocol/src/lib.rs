//! Client-facing data contracts. Model messages and execution belong to other crates.

mod plugin;
mod resources;
pub use plugin::*;
pub use resources::*;
mod btw;
mod shells;
mod subagents;
pub use shells::*;
pub use subagents::*;
mod program;
pub use program::*;
mod rewind;
pub use btw::*;
pub use rewind::*;
pub mod app;
mod context;
pub mod rpc;
pub mod workspace;
pub use context::*;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The session's permission mode as a string: `rules`, `auto` or `yolo`.
pub const PERMISSION_MODE_EVENT: &str = "tiechui.permissions.mode";

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ThreadId(pub String);

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct TurnId(pub String);

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ItemId(pub String);

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SubmissionId(pub String);

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ApprovalId(pub String);

/// Identifies a session generation so stale asynchronous results can be discarded.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SessionEpoch(pub u64);

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputContent {
    Text {
        text: String,
    },
    Image {
        mime_type: String,
        /// Base64-encoded image bytes.
        data: String,
    },
}

/// Both modes start a turn when the thread is idle.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InputDelivery {
    /// Apply the input at the next tool boundary of the current turn.
    Steer,
    /// Run the input after the current turn completes.
    FollowUp,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompactionReason {
    Manual,
    Threshold,
    Overflow,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompactionPhase {
    Prepare,
    Summarize,
    Restore,
    Commit,
}

/// A question's exclusive choice; highlighting it does not submit an answer.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<QuestionOption>>,
}

/// What the user is asked; at most one request is open at a time.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalRequest {
    Plugin {
        plugin: String,
        prompt: PluginPrompt,
    },
    Information {
        title: String,
        text: String,
    },
    Editor {
        title: String,
        text: String,
    },
    Rewind {
        panel: RewindPanel,
    },
    /// A tool call the permission policy asks about.
    Permission {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        actor: Option<Box<str>>,
        call_id: String,
        tool: String,
        arguments: Value,
        surface: String,
        /// What the rule was matched against, such as the command or the paths.
        values: Vec<String>,
        matched_pattern: Option<String>,
        origin: String,
        /// What allowing for the session grants; empty when it cannot be offered.
        session_patterns: Vec<String>,
        /// For `write` and `edit`, the file before and after the call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<Box<FilePreview>>,
    },
    /// An MCP server's elicitation parameters, as sent.
    Elicitation {
        server: String,
        request: Value,
    },
    Questions {
        questions: Vec<Question>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
pub struct FilePreview {
    pub before: String,
    pub after: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    /// Widens a read or write path scope to both directions when set.
    AllowSession {
        both_directions: bool,
    },
    Deny {
        reason: Option<String>,
    },
    /// Elicitation answers.
    Accept {
        content: Option<Value>,
    },
    Decline,
    Cancel,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Op {
    Compact {
        instructions: Option<String>,
    },
    /// Whether the main composer or a blocking picker currently owns user input.
    InputState {
        blocked: bool,
    },
    /// Interrupt idle module work as well as an active turn.
    InterruptIdle,
    /// An action from a module's interactive surface.
    CommandInput {
        name: String,
        data: Value,
    },
    /// Remove persisted refill attachments; ordinary composer edits are not saved.
    ClearDraft,
    /// Continue the most recent failed request, without adding input.
    Retry,
    UserInput {
        content: Vec<InputContent>,
        delivery: InputDelivery,
    },
    Interrupt {
        turn_id: TurnId,
    },
    SetModel {
        provider: String,
        model: String,
    },
    SetThinking {
        level: ThinkingLevel,
    },
    Branch {
        entry_id: String,
    },
    Reload,
    ApprovalResponse {
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    },
    /// Removes inputs that are still queued; consumed ones are left alone.
    Retract {
        submission_ids: Vec<SubmissionId>,
    },
    /// A module command from `CommandsUpdated`; answered by `CommandOutput`.
    RunCommand {
        name: String,
        arguments: String,
    },
    /// The user's own shell command, run between turns and kept in the model context.
    RunShell {
        command: String,
    },
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelInfo {
    pub provider: String,
    /// The model's own id, without the provider prefix.
    pub id: String,
    /// Zero when the provider does not report it.
    pub context_window: u64,
    pub reasoning: bool,
    /// Levels the model accepts, `off` first.
    pub thinking_levels: Vec<ThinkingLevel>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
}

/// A user input waiting in the steering or follow-up queue.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueuedInput {
    pub submission_id: SubmissionId,
    pub content: Vec<InputContent>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Submission {
    pub id: SubmissionId,
    pub thread_id: ThreadId,
    pub epoch: SessionEpoch,
    pub op: Op,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Thread {
    pub id: ThreadId,
    pub epoch: SessionEpoch,
    pub turns: Vec<Turn>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Turn {
    pub id: TurnId,
    pub status: TurnStatus,
    pub items: Vec<Item>,
    /// Unix milliseconds; zero in older clients' data.
    #[serde(default)]
    pub started_at: u64,
    /// Unix milliseconds of the turn's last entry, once it has ended.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<u64>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    InProgress,
    Completed,
    Interrupted,
    Failed,
}

/// Known shapes are decoded first; future kinds or fields retain their complete payload.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Item {
    Known(KnownItem),
    Custom {
        kind: String,
        /// All object fields except `kind`, including any item id.
        #[serde(flatten)]
        payload: Map<String, Value>,
    },
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum KnownItem {
    Compaction {
        id: ItemId,
        summary: String,
        tokens_before: u64,
        /// The summary omits earlier history that did not fit its request.
        #[serde(default, skip_serializing_if = "is_false")]
        degraded: bool,
    },
    UserMessage {
        id: ItemId,
        content: Vec<InputContent>,
    },
    AssistantMessage {
        id: ItemId,
        text: String,
    },
    Thinking {
        id: ItemId,
        text: String,
    },
    ToolCall {
        id: ItemId,
        name: String,
        arguments: Value,
    },
    ToolResult {
        id: ItemId,
        call_id: ItemId,
        output: Value,
        is_error: bool,
    },
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Event {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub submission_id: Option<SubmissionId>,
    pub thread_id: ThreadId,
    pub epoch: SessionEpoch,
    pub event: EventKind,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TokenUsage {
    /// Uncached input tokens; cache reads and writes are counted separately.
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Reasoning is a subset of output tokens.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write_1h: u64,
    /// Estimated USD in billionths; absent when no prices were configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_nano_usd: Option<u64>,
}

fn is_false(value: &bool) -> bool {
    !value
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventKind {
    CompactionStarted {
        reason: CompactionReason,
    },
    /// Retries count re-sent summary requests; shrinks count reductions of the summary input.
    CompactionProgress {
        reason: CompactionReason,
        phase: CompactionPhase,
        retries: u32,
        shrinks: u32,
    },
    CompactionEnded {
        reason: CompactionReason,
        aborted: bool,
        error: Option<String>,
        will_retry: bool,
        /// Why automatic compaction now waits for new input, a model response or `/compact`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        paused: Option<String>,
    },
    /// Recover saved attachments only if the editor is empty.
    ComposerRestored {
        content: Vec<InputContent>,
    },
    ComposerRefilled {
        content: Vec<InputContent>,
    },
    DraftImagesDetached,
    Extension {
        name: String,
        data: Value,
    },
    /// Whether the current branch has an unresolved model request failure.
    RetryAvailable {
        available: bool,
    },
    ThreadStarted {
        thread: Thread,
        /// Replay a changed branch even when the client already shows this session.
        #[serde(default, skip_serializing_if = "is_false")]
        replay: bool,
    },
    TurnStarted {
        turn: Turn,
    },
    /// A retry is waiting when delay_ms is present, and starting when absent.
    RequestRetry {
        turn_id: TurnId,
        attempt: u32,
        max_retries: Option<u32>,
        delay_ms: Option<u64>,
        message: String,
    },
    ItemStarted {
        turn_id: TurnId,
        item: Item,
    },
    ItemDelta {
        turn_id: TurnId,
        item_id: ItemId,
        delta: String,
    },
    ItemCompleted {
        turn_id: TurnId,
        item: Item,
    },
    TokenUsageUpdated {
        turn_id: TurnId,
        /// Token usage for the latest model request.
        last: TokenUsage,
        /// Cumulative token usage for the thread.
        total: TokenUsage,
    },
    TurnCompleted {
        turn: Turn,
    },
    /// The next queued request, once the previous one is resolved.
    ApprovalRequested {
        turn_id: Option<TurnId>,
        approval_id: ApprovalId,
        request: ApprovalRequest,
    },
    /// Answered, withdrawn, or cancelled with its thread.
    ApprovalResolved {
        approval_id: ApprovalId,
    },
    Error {
        message: String,
    },
    /// After the thread starts and whenever the model or thinking level changes.
    SessionUpdated {
        model: ModelInfo,
        thinking: ThinkingLevel,
        /// The latest request's usage on the current branch, if any.
        context: Option<TokenUsage>,
        /// Cumulative usage for the thread, including inactive branches.
        total: TokenUsage,
    },
    /// The complete queue state after any change, in delivery order.
    QueueUpdated {
        steering: Vec<QueuedInput>,
        follow_up: Vec<QueuedInput>,
    },
    /// Inputs actually removed by `Op::Retract`, in queue order.
    InputRetracted {
        inputs: Vec<QueuedInput>,
    },
    /// Every model the session can switch to, after the thread starts.
    ModelsUpdated {
        models: Vec<ModelInfo>,
    },
    /// Commands contributed by modules, after the thread starts.
    ResourcesUpdated {
        resources: Vec<Resource>,
    },
    CommandsUpdated {
        commands: Vec<CommandInfo>,
    },
    CommandOutput {
        name: String,
        text: String,
    },
    /// A frozen, read-only panel contributed by a command.
    CommandPanel {
        name: String,
        panel: Box<ContextSnapshot>,
        view: ContextView,
        text: String,
    },
}

/// The schema entry point for queued operations, events, and JSON-RPC envelopes.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ProtocolMessage {
    Submission(Submission),
    Event(Box<Event>),
    JsonRpc(rpc::JsonRpcMessage),
}
