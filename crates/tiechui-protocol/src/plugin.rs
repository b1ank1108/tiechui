//! Versioned out-of-process plugin and declarative UI contracts.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PLUGIN_EVENT: &str = "tiechui.plugin.status";
pub const PLUGIN_NOTICE_EVENT: &str = "tiechui.plugin.notice";

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub enum PluginCapability {
    #[serde(rename = "tools")]
    Tools,
    #[serde(rename = "commands")]
    Commands,
    #[serde(rename = "tool.intercept")]
    Intercept,
    #[serde(rename = "ui.notify")]
    Notify,
    #[serde(rename = "ui.select")]
    Select,
    #[serde(rename = "ui.confirm")]
    Confirm,
    #[serde(rename = "ui.input")]
    Input,
    #[serde(rename = "status.set")]
    Status,
    #[serde(rename = "ui.panel")]
    Panel,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PluginInitialize {
    pub api_version: u32,
    pub capabilities: Vec<PluginCapability>,
    pub contributions: PluginContributions,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginContributions {
    #[serde(default)]
    pub tools: Vec<PluginTool>,
    #[serde(default)]
    pub commands: Vec<PluginCommand>,
    #[serde(default)]
    pub interceptors: Vec<PluginStage>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct PluginTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginCommand {
    pub name: String,
    pub description: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginStage {
    BeforePolicy,
    Policy,
    AfterPolicy,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginExtension {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub capabilities: Vec<PluginCapability>,
    #[serde(default)]
    pub contributions: PluginContributions,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginPanel {
    pub title: String,
    pub nodes: Vec<PanelNode>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PanelNode {
    Text {
        text: String,
    },
    Markdown {
        text: String,
    },
    List {
        children: Vec<PanelNode>,
    },
    Form {
        fields: Vec<PanelField>,
    },
    Progress {
        label: String,
        value: u64,
        total: u64,
    },
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PanelField {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub required: bool,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginPrompt {
    Select {
        title: String,
        text: String,
        options: Vec<String>,
    },
    Confirm {
        title: String,
        text: String,
    },
    Input {
        title: String,
        text: String,
    },
    Panel {
        panel: PluginPanel,
    },
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PluginStatus {
    pub plugin: String,
    pub text: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PluginOutput {
    pub text: String,
    #[serde(default)]
    pub is_error: bool,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginVerdict {
    Allow,
    Deny { reason: String },
    Rewrite { arguments: Value },
}

/// All typed payloads included in the generated protocol schema.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(JsonSchema)]
pub struct PluginProtocol {
    pub envelope: crate::rpc::JsonRpcMessage,
    pub extension: PluginExtension,
    pub initialize: PluginInitialize,
    pub prompt: PluginPrompt,
    pub status: PluginStatus,
    pub output: PluginOutput,
    pub verdict: PluginVerdict,
    pub requests: PluginRequest,
    pub decision: crate::ApprovalDecision,
}

/// Method payloads; requests may additionally carry the transport's `_meta` object.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "method", content = "params")]
pub enum PluginRequest {
    #[serde(rename = "initialize")]
    Initialize(PluginInitialize),
    #[serde(rename = "tools/list")]
    ToolsList {},
    #[serde(rename = "tools/call")]
    ToolsCall { name: String, arguments: Value },
    #[serde(rename = "commands/list")]
    CommandsList {},
    #[serde(rename = "commands/run")]
    CommandsRun { name: String, arguments: String },
    #[serde(rename = "tool.intercept")]
    Intercept(PluginIntercept),
    #[serde(rename = "ui.notify")]
    Notify { title: String, text: String },
    #[serde(rename = "ui.confirm")]
    Confirm { title: String, text: String },
    #[serde(rename = "ui.input")]
    Input { title: String, text: String },
    #[serde(rename = "ui.select")]
    Select {
        title: String,
        text: String,
        options: Vec<String>,
    },
    #[serde(rename = "panel.show")]
    Panel(PluginPanel),
    #[serde(rename = "status.set")]
    Status { text: String },
    #[serde(rename = "notifications/cancelled")]
    Cancelled {
        #[serde(rename = "requestId")]
        request_id: u64,
    },
    #[serde(rename = "notifications/progress")]
    Progress {
        #[serde(rename = "progressToken")]
        progress_token: u64,
        progress: f64,
        message: String,
    },
    #[serde(rename = "shutdown")]
    Shutdown {},
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum PluginIntercept {
    Before {
        stage: PluginStage,
        call: PluginCall,
    },
    After {
        stage: PluginStage,
        call: PluginCall,
        output: Value,
    },
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct PluginCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}
