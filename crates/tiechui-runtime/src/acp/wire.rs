use serde::Deserialize;
use serde_json::{Map, Value, json};
use tiechui_protocol::InputContent;

use super::dialect::InitializeInfo;

pub const ACP_PROTOCOL_VERSION: u64 = 1;

pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": ACP_PROTOCOL_VERSION,
        "clientCapabilities": {
            "fs": {
                "readTextFile": false,
                "writeTextFile": false,
            },
            "terminal": false,
            "elicitation": { "form": {} },
        },
        "clientInfo": {
            "name": "tiechui",
            "title": "Tiechui",
            "version": env!("CARGO_PKG_VERSION"),
        },
    })
}

pub fn session_params(cwd: &str, session_id: Option<&str>) -> Value {
    let mut body = Map::new();
    body.insert("cwd".into(), json!(cwd));
    body.insert("mcpServers".into(), json!([]));
    if let Some(id) = session_id {
        body.insert("sessionId".into(), json!(id));
    }
    Value::Object(body)
}

pub fn user_prompt_blocks(content: &[InputContent]) -> Vec<Value> {
    content
        .iter()
        .map(|part| match part {
            InputContent::Text { text } => json!({ "type": "text", "text": text }),
            InputContent::Image { mime_type, data } => {
                let raw = strip_data_url_prefix(data).unwrap_or(data.as_str());
                json!({
                    "type": "image",
                    "mimeType": mime_type,
                    "data": raw,
                })
            }
        })
        .collect()
}

fn strip_data_url_prefix(data: &str) -> Option<&str> {
    let rest = data.strip_prefix("data:")?;
    let (_, b64) = rest.split_once(";base64,")?;
    Some(b64)
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InitializeWire {
    pub protocol_version: Option<u64>,
    pub agent_capabilities: AgentCapabilitiesWire,
    pub agent_info: Option<ImplementationWire>,
}

impl InitializeWire {
    pub fn into_info(self) -> InitializeInfo {
        InitializeInfo {
            protocol_version: self.protocol_version.unwrap_or(ACP_PROTOCOL_VERSION),
            agent_version: self.agent_info.and_then(|i| i.version),
            load_session: self.agent_capabilities.load_session,
            resume_session: self
                .agent_capabilities
                .session_capabilities
                .resume
                .is_some(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AgentCapabilitiesWire {
    pub load_session: bool,
    pub session_capabilities: SessionCapabilitiesWire,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionCapabilitiesWire {
    pub resume: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ImplementationWire {
    pub version: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionSetupWire {
    pub session_id: Option<String>,
    pub modes: Option<SessionModeStateWire>,
    pub models: Option<SessionModelStateWire>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionModeStateWire {
    pub current_mode_id: String,
    pub available_modes: Vec<SessionModeWire>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionModeWire {
    pub id: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionModelStateWire {
    pub current_model_id: String,
    pub available_models: Vec<AcpModelInfoWire>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AcpModelInfoWire {
    pub model_id: String,
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PromptResponseWire {
    pub stop_reason: String,
    pub usage: Option<AcpUsageWire>,
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AcpUsageWire {
    #[serde(alias = "inputTokens", alias = "input_tokens")]
    pub input_tokens: u64,
    #[serde(alias = "outputTokens", alias = "output_tokens")]
    pub output_tokens: u64,
    #[serde(alias = "cachedReadTokens", alias = "cache_read_input_tokens")]
    pub cached_read_tokens: u64,
    #[serde(
        alias = "cachedWriteTokens",
        alias = "cacheCreationTokens",
        alias = "cache_creation_input_tokens"
    )]
    pub cached_write_tokens: u64,
    #[serde(alias = "thoughtTokens", alias = "reasoning_tokens")]
    pub thought_tokens: u64,
    #[serde(alias = "totalTokens", alias = "total_tokens")]
    pub total_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SessionNotificationWire {
    pub session_id: String,
    pub update: Value,
    #[serde(rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AvailableCommandWire {
    pub name: String,
    pub description: String,
    pub input: Option<CommandInputHintWire>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CommandInputHintWire {
    pub hint: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PermissionOptionWire {
    pub option_id: String,
    pub name: String,
    pub kind: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PermissionRequestWire {
    pub tool_call: Value,
    pub options: Vec<PermissionOptionWire>,
}
