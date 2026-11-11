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
    /// `anthropic-messages`, `openai-completions` or `openai-responses`.
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

/// A server of `[mcp_servers]` in the user's `config.toml`, as far as clients edit it; saving
/// keeps its other keys.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct McpServerEntry {
    pub name: String,
    /// Starts a stdio server; otherwise `url` is a Streamable HTTP endpoint.
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub url: Option<String>,
    pub http_headers: BTreeMap<String, String>,
    pub tool_timeout_sec: Option<f64>,
    pub enabled: bool,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct McpSaveParams {
    /// The server to update, which takes the new name; `None` adds the server.
    pub previous: Option<String>,
    pub server: McpServerEntry,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct McpParams {
    pub name: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpState {
    Starting,
    Ready,
    Disabled,
    Failed,
    Stopped,
}

/// A server of `mcp/list` once it has started, from `mcp/status`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct McpStatus {
    pub name: String,
    pub state: McpState,
    pub tools: Vec<String>,
    pub error: Option<String>,
    /// For HTTP servers that may sign in: `authenticated`, `expired`, `logged_out` or
    /// `invalid_credentials`.
    pub oauth: Option<String>,
}

/// The `mcp/authorize` notification: the page to open for the `mcp/login` waiting on it.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct McpAuthorize {
    pub name: String,
    pub url: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillsListParams {
    /// The project whose skills a session there adds; without one, the user's alone.
    pub cwd: Option<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillList {
    /// `$TIECHUI_HOME/skills`.
    pub dir: String,
    pub skills: Vec<SkillEntry>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    /// The skill's Markdown file.
    pub path: String,
    /// The skills directory it was found in.
    pub root: String,
    /// `user`, `project` or `plugin`.
    pub source: String,
    /// Loaded only through `$name`, never offered to the model.
    pub manual: bool,
    /// Not named in the user's `skills.disabled`.
    pub enabled: bool,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SkillEnableParams {
    pub name: String,
    pub enabled: bool,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromptsListParams {
    /// The project whose prompts a session there adds; without one, the user's alone.
    pub cwd: Option<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromptList {
    /// `$TIECHUI_HOME/prompts`.
    pub dir: String,
    pub prompts: Vec<PromptEntry>,
    /// Unreadable files, names defined twice and untrusted project prompts.
    pub warnings: Vec<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromptEntry {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    /// The template; `$ARGUMENTS` stands for the text after the command.
    pub body: String,
    pub path: String,
    /// `user` or `project`.
    pub source: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromptSaveParams {
    /// Required for the `project` scope.
    pub cwd: Option<String>,
    /// `user` saves to `$TIECHUI_HOME/prompts`, `project` to `<cwd>/.agents/prompts`.
    pub scope: String,
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub body: String,
    /// The file being edited: rewritten in place, or removed when the name or scope changes.
    pub previous: Option<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PromptDeleteParams {
    pub cwd: Option<String>,
    pub path: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentsListParams {
    /// The project whose agent definitions a dispatch there finds; without one, the user's alone.
    pub cwd: Option<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentList {
    /// `$TIECHUI_HOME/agents`.
    pub dir: String,
    /// Built-in agents, then definitions by name; a user definition hides a project one.
    pub agents: Vec<AgentEntry>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentEntry {
    pub name: String,
    pub description: String,
    /// The only tools the agent may use; all of them when absent.
    pub tools: Option<Vec<String>>,
    /// `provider/id`; the parent's model when absent.
    pub model: Option<String>,
    /// `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` or `inherit`; the parent's
    /// requested thinking level when absent.
    pub thinking: Option<String>,
    pub prompt: String,
    /// Absent for built-in agents.
    pub path: Option<String>,
    /// `builtin`, `user` or `project`.
    pub source: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentSaveParams {
    /// Required for the `project` scope.
    pub cwd: Option<String>,
    /// `user` saves to `$TIECHUI_HOME/agents`, `project` to `<cwd>/.agents/agents`.
    pub scope: String,
    pub name: String,
    pub description: String,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    /// `off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` or `inherit`.
    pub thinking: Option<String>,
    pub prompt: String,
    /// The file being edited: rewritten in place keeping its other frontmatter, or removed
    /// when the name or scope changes.
    pub previous: Option<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AgentDeleteParams {
    pub cwd: Option<String>,
    pub path: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HooksListParams {
    /// The project whose hooks a session there loads; without one, the user's and plugins' alone.
    pub cwd: Option<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HookList {
    /// `$TIECHUI_HOME/hooks.json`.
    pub path: String,
    pub hooks: Vec<HookEntry>,
    /// Files, handlers and plugins that could not be loaded.
    pub diagnostics: Vec<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HookEntry {
    /// Names the handler for `hooks/save`, `hooks/delete` and `hooks/approve`.
    pub key: String,
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
    /// Seconds.
    pub timeout: u64,
    pub path: String,
    /// `user`, `project` or `plugin`.
    pub source: String,
    /// `user` and `trusted` handlers run; `unapproved` and `changed` ones wait for
    /// `hooks/approve`, `untrusted_project` ones for project trust.
    pub trust: String,
    /// Handlers in `hooks.json` files can be saved and deleted.
    pub editable: bool,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HookSaveParams {
    /// Required for the `project` scope.
    pub cwd: Option<String>,
    /// `user` saves to `$TIECHUI_HOME/hooks.json`, `project` to `<cwd>/.agents/hooks.json`.
    pub scope: String,
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
    /// Seconds.
    pub timeout: u64,
    /// The key of the handler being edited: changed in place, or moved when the scope, event
    /// or matcher changes.
    pub previous: Option<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HookDeleteParams {
    pub cwd: Option<String>,
    pub key: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HookApproveParams {
    pub cwd: Option<String>,
    /// Handlers to approve as they are now; others and ones not awaiting approval are left alone.
    pub keys: Vec<String>,
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
