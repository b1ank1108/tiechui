//! The parts of the `codex app-server` JSON-RPC protocol (schema of 0.171.0, recordings of
//! 0.156.1) the driver speaks. Only what it reads or sends is declared; everything else in a
//! frame is ignored, so a newer Codex adding fields does not break it.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tiechui_protocol::InputContent;

/// One line from `codex app-server`: a response (`id` with `result` or `error`), a server
/// request (`id` with `method`) or a notification (`method` only).
#[derive(Debug, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Value,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcError {
    #[serde(default)]
    pub message: String,
}

// ----- requests the driver sends -----

pub fn initialize_params() -> Value {
    json!({
        "clientInfo": {
            "name": "tiechui",
            "title": "Tiechui",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": {
            "experimentalApi": true,
            // The turn's aggregate diff is a large payload the driver never uses.
            "optOutNotificationMethods": ["turn/diff/updated"],
        },
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadParams<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<&'a str>,
    pub cwd: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<&'a str>,
    pub approval_policy: &'static str,
    pub approvals_reviewer: &'static str,
    pub sandbox: &'static str,
    /// `thread/resume` only: the history stays in Codex, the session file has its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_turns: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStartParams<'a> {
    pub thread_id: &'a str,
    pub input: Vec<Value>,
    pub cwd: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<&'a str>,
    pub approval_policy: &'static str,
    pub approvals_reviewer: &'static str,
    pub sandbox_policy: Value,
    pub summary: &'static str,
}

pub fn user_input(content: &[InputContent]) -> Vec<Value> {
    content
        .iter()
        .map(|part| match part {
            InputContent::Text { text } => {
                json!({"type": "text", "text": text, "text_elements": []})
            }
            InputContent::Image { mime_type, data } => {
                json!({"type": "image", "url": format!("data:{mime_type};base64,{data}")})
            }
        })
        .collect()
}

// ----- responses -----

/// What `thread/start` and `thread/resume` answer.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadResponse {
    pub thread: ThreadId,
    #[serde(default)]
    pub model: String,
}

#[derive(Debug, Deserialize)]
pub struct ThreadId {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct TurnResponse {
    pub turn: TurnBody,
}

// ----- notifications -----

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnNotification {
    pub thread_id: String,
    pub turn: TurnBody,
}

#[derive(Debug, Deserialize)]
pub struct TurnBody {
    pub id: String,
    /// The items Codex lists for a finished turn (summary view).
    #[serde(default)]
    pub items: Vec<Value>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub error: Option<TurnError>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub additional_details: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemNotification {
    pub thread_id: String,
    pub item: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeltaNotification {
    pub thread_id: String,
    pub item_id: String,
    #[serde(default)]
    pub delta: String,
    #[serde(default)]
    pub summary_index: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub token_usage: ThreadTokenUsage,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsage {
    pub last: Breakdown,
    pub total: Breakdown,
    #[serde(default)]
    pub model_context_window: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Breakdown {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: u64,
    pub total_tokens: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedNotification {
    pub request_id: Value,
    #[serde(default)]
    pub thread_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusNotification {
    pub thread_id: String,
    pub status: Status,
}

#[derive(Debug, Deserialize)]
pub struct Status {
    #[serde(rename = "type", default)]
    pub kind: String,
}

// ----- items -----

#[derive(Debug, Deserialize)]
pub struct AgentMessageItem {
    pub id: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct ReasoningItem {
    pub id: String,
    #[serde(default)]
    pub summary: Vec<String>,
    #[serde(default)]
    pub content: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandItem {
    pub id: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub command_actions: Vec<CommandAction>,
    #[serde(default)]
    pub aggregated_output: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct CommandAction {
    #[serde(default)]
    pub command: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FileChangeItem {
    pub id: String,
    #[serde(default)]
    pub changes: Vec<FileChange>,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FileChange {
    pub path: String,
    pub kind: ChangeKind,
    #[serde(default)]
    pub diff: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ChangeKind {
    Add,
    Delete,
    Update {
        #[serde(default)]
        move_path: Option<String>,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct McpItem {
    pub id: String,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub result: Option<McpResult>,
    #[serde(default)]
    pub error: Option<McpError>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpResult {
    #[serde(default)]
    pub content: Vec<Value>,
    #[serde(default)]
    pub structured_content: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct McpError {
    #[serde(default)]
    pub message: String,
}

/// An item of a kind without a dedicated mapping: web search, dynamic tool call, image view.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OtherItem {
    pub id: String,
    #[serde(default)]
    pub query: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub arguments: Option<Value>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub success: Option<bool>,
    #[serde(default)]
    pub content_items: Option<Vec<Value>>,
}

// ----- server requests -----

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandApproval {
    pub thread_id: String,
    pub item_id: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub command_actions: Vec<CommandAction>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileApproval {
    pub thread_id: String,
    pub item_id: String,
    #[serde(default)]
    pub grant_root: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionsApproval {
    pub thread_id: String,
    pub item_id: String,
    #[serde(default)]
    pub permissions: Value,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Elicitation {
    pub thread_id: String,
    #[serde(default)]
    pub server_name: String,
}
