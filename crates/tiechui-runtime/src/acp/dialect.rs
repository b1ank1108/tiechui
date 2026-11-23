use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tiechui_protocol::{ApprovalDecision, ApprovalRequest, CommandInfo, ModelInfo, SubagentState};

use crate::backend::StartParams;
use crate::registry::Launch;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextDisposition {
    Keep,
    Ignore,
    FailTurn(String),
}

pub struct CustomApproval {
    pub request: ApprovalRequest,
    pub respond: Box<dyn FnOnce(&ApprovalDecision) -> Value + Send>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpSubagent {
    pub agent_id: String,
    pub name: Option<String>,
    pub state: SubagentState,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolLocation {
    pub path: String,
    pub line: Option<u64>,
}

#[derive(Debug, Clone, Default)]
pub struct ToolState {
    pub id: String,
    pub name: Option<String>,
    pub title: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub locations: Vec<ToolLocation>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<Value>,
    pub content: Vec<Value>,
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct InitializeInfo {
    pub protocol_version: u64,
    pub agent_version: Option<String>,
    pub load_session: bool,
    pub resume_session: bool,
}

#[derive(Debug, Clone)]
pub struct AcpLaunchSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env_set: Vec<(String, String)>,
    pub env_remove: Vec<String>,
}

impl AcpLaunchSpec {
    pub fn new(program: impl Into<PathBuf>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            env_set: Vec::new(),
            env_remove: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AcpTimeouts {
    pub handshake: Duration,
    pub session: Duration,
    pub request: Duration,
    pub wakeup_idle: Duration,
}

impl Default for AcpTimeouts {
    fn default() -> Self {
        Self {
            handshake: Duration::from_secs(60),
            session: Duration::from_secs(60),
            request: Duration::from_secs(30),
            wakeup_idle: Duration::from_secs(2),
        }
    }
}

#[async_trait]
pub trait AcpDialect: Send + Sync {
    fn provider_id(&self) -> &'static str {
        "acp"
    }

    fn launch(&self, _launch: &Launch, _params: &StartParams) -> Result<AcpLaunchSpec, String> {
        Err("no ACP launch command configured".into())
    }

    async fn fallback_models(&self, _launch: &Launch) -> Vec<ModelInfo> {
        Vec::new()
    }

    fn check_initialize(&self, _info: &InitializeInfo) -> Result<(), String> {
        Ok(())
    }

    fn session_mode(&self, mode: &str, available: &[String]) -> Option<String> {
        let candidates: &[&str] = match mode {
            "auto" => &["accept-edits", "auto_edit", "auto"],
            "yolo" => &["yolo", "bypassPermissions"],
            _ => &["default", "ask"],
        };
        if available.is_empty() {
            return candidates.first().map(|picked| (*picked).to_owned());
        }
        candidates
            .iter()
            .find(|candidate| available.iter().any(|item| item == **candidate))
            .map(|picked| (*picked).to_owned())
    }

    /// Whether switching to `next` mid-session requires restarting the ACP process and resuming.
    fn mode_requires_restart(&self, _current: Option<&str>, _next: &str) -> bool {
        false
    }

    fn filter_command(&self, _command: &CommandInfo) -> bool {
        true
    }

    fn on_text_chunk(&self, _thinking: bool, _text: &str) -> TextDisposition {
        TextDisposition::Keep
    }

    fn normalize_tool(&self, _tool: &mut ToolState) {}

    fn tool_subagent(&self, _tool: &ToolState) -> Option<AcpSubagent> {
        None
    }

    fn intercept_request(&self, _method: &str, _params: &Value) -> Option<CustomApproval> {
        None
    }

    fn format_error(&self, _code: i64, message: &str, _data: Option<&Value>) -> String {
        message.to_owned()
    }

    fn is_auth_error(&self, _code: i64, _message: &str, _data: Option<&Value>) -> bool {
        false
    }
}

pub struct GenericAcpDialect {
    pub spec: AcpLaunchSpec,
}

impl AcpDialect for GenericAcpDialect {
    fn launch(&self, _launch: &Launch, _params: &StartParams) -> Result<AcpLaunchSpec, String> {
        Ok(self.spec.clone())
    }
}

#[derive(Clone)]
pub struct AcpConfig {
    pub launch: Launch,
    pub dialect: Arc<dyn AcpDialect>,
    pub timeouts: AcpTimeouts,
}

impl AcpConfig {
    pub fn generic(launch: Launch, spec: AcpLaunchSpec) -> Self {
        Self {
            launch,
            dialect: Arc::new(GenericAcpDialect { spec }),
            timeouts: AcpTimeouts::default(),
        }
    }
}
