//! `chui app-server`: JSON-RPC 2.0 methods for desktop clients, one JSON message per line.
//! Each open session's events arrive as `session/event` notifications numbered per session;
//! a snapshot's `seq` is the last number its events include.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{Event, Op, SubmissionId, ThinkingLevel, ThreadId};

pub const APP_PROTOCOL_VERSION: u32 = 1;

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InitializeParams {
    pub protocol_version: u32,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct InitializeResult {
    pub protocol_version: u32,
    pub server_version: String,
    /// The Tiechui home, `$TIECHUI_HOME` or `~/.tiechui`.
    pub home: String,
    /// Optional method groups: `workspace` for `fs/*`, `git/*` and `terminal/*`.
    pub capabilities: Vec<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionListParams {
    pub cwd: String,
    /// Include archived sessions.
    #[serde(default)]
    pub archived: bool,
}

/// What an open session is doing.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LiveState {
    Idle,
    Running,
    /// Waiting for an approval response.
    Waiting,
    /// The last model request failed and can be retried.
    Failed,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionSummary {
    pub thread_id: ThreadId,
    pub path: String,
    pub cwd: String,
    /// Unix milliseconds.
    pub created_at: u64,
    /// Unix milliseconds of the latest user or assistant message.
    pub modified: u64,
    pub first_message: String,
    pub title: Option<String>,
    pub archived_at: Option<u64>,
    /// The session file this one was forked from.
    pub parent: Option<String>,
    /// Present while the session is open in this server.
    pub live: Option<LiveState>,
}

/// Opens the session file at `path`, or starts a new session in `cwd`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SessionOpenParams {
    pub cwd: Option<String>,
    pub path: Option<String>,
    /// `<provider>/<model id>`.
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    /// `rules`, `auto` or `yolo`; otherwise the configured mode.
    pub mode: Option<String>,
}

/// Events that rebuild the session's current state, as if received from its start.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Snapshot {
    pub thread_id: ThreadId,
    pub path: String,
    pub cwd: String,
    pub seq: u64,
    pub events: Vec<Event>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ThreadParams {
    pub thread_id: ThreadId,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SubmitParams {
    pub thread_id: ThreadId,
    pub op: Op,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SubmitResult {
    pub submission_id: SubmissionId,
}

/// Names a turn by the id of its user message item, live or replayed.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TurnParams {
    pub thread_id: ThreadId,
    pub item_id: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RenameParams {
    pub path: String,
    /// None restores the first message as the title.
    pub title: Option<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArchiveParams {
    pub path: String,
    pub archived: bool,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PathParams {
    pub path: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TrustParams {
    pub cwd: String,
    /// None reads the current decision.
    pub allow: Option<bool>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TrustResult {
    /// None when the directory has no decision.
    pub allowed: Option<bool>,
}

/// A file a turn changed through tracked edits; `None` while the file is absent.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileChange {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// The `turn/changes` and `turn/undo` result.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TurnChanges {
    pub changes: Vec<FileChange>,
    /// Whether the turn has been undone.
    pub reverted: bool,
}

/// The `turn/check` result: files still as the turn left them (as before it, once undone),
/// files changed since, and the shell commands the turn ran, which undo does not track.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UndoCheck {
    pub safe: Vec<String>,
    pub r#unsafe: Vec<String>,
    pub ignored: Vec<String>,
}

/// The `turn/rewind` result. Files are restored to before the turn only when none of them
/// changed since last written by the agent; otherwise those are listed.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RewindResult {
    pub conflicts: Vec<String>,
}

/// A provider declared in `providers.toml`. Its key is never sent.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderEntry {
    pub name: String,
    /// `anthropic-messages` or `openai-completions`.
    pub wire: String,
    pub base_url: Option<String>,
    /// Whether a key is written in the file.
    pub api_key: bool,
    pub api_key_env: Option<String>,
    pub models: Vec<ModelEntry>,
}

/// The parts of a declared model a client edits; saving keeps the rest of its table.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelEntry {
    pub id: String,
    pub context_window: u64,
    pub max_output: u64,
    /// Levels offered, as in `ModelInfo`; only `off` without reasoning.
    pub levels: Vec<ThinkingLevel>,
    /// The value sent to the provider for a level, where one is configured.
    pub mapping: BTreeMap<ThinkingLevel, String>,
    pub images: bool,
}

/// Declares a provider or updates one.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderSaveParams {
    pub name: String,
    pub wire: String,
    pub base_url: Option<String>,
    /// A key to write in place of `api_key_env`; `None` keeps the current one.
    pub api_key: Option<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderParams {
    pub name: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelSaveParams {
    pub provider: String,
    /// The id of the model to update; `None` adds the model.
    pub previous: Option<String>,
    pub model: ModelEntry,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ModelParams {
    pub provider: String,
    pub id: String,
}

/// The `providers/test` result of one short request to the model.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ProviderTest {
    /// Why the request failed.
    pub error: Option<String>,
    pub millis: u64,
}

/// Sets one key of the user's `config.toml`: `model`, `thinking`, `network.proxy`,
/// `network.no_proxy`, `network.ca_cert`, `ask.auto_continue_after_sec` or
/// `sessions.archive_after_days`; null removes it.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigUpdateParams {
    pub key: String,
    pub value: serde_json::Value,
}

/// The `config/get` result: the user's values for the keys `config/update` sets.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConfigValues {
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    pub proxy: Option<String>,
    pub no_proxy: Option<String>,
    pub ca_cert: Option<String>,
    pub auto_continue_after_sec: Option<u64>,
    pub archive_after_days: Option<u64>,
}

/// The `session/event` notification.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionEvent {
    pub seq: u64,
    pub event: Event,
}

/// The `session/closed` notification, sent when a session ends without `session/close`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SessionClosed {
    pub thread_id: ThreadId,
    pub error: Option<String>,
}
