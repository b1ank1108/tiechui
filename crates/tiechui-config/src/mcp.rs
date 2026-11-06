use std::collections::BTreeMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Codex-compatible server settings. Unsupported fields remain diagnostic keys.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct McpServerConfig {
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub env_vars: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub url: Option<String>,
    pub http_headers: BTreeMap<String, String>,
    pub bearer_token_env_var: Option<String>,
    pub startup_timeout_sec: Option<f64>,
    pub tool_timeout_sec: Option<f64>,
    pub enabled: Option<bool>,
    pub enabled_tools: Option<Vec<String>>,
    pub disabled_tools: Vec<String>,
    #[serde(flatten)]
    pub unknown: BTreeMap<String, serde_json::Value>,
}
