//! Session-owned MCP clients and live tool contributions.

mod client;
mod http;
pub mod oauth;
mod output;
mod resources;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use rmcp::{Peer, RoleClient};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tiechui_plugin_api::model::ToolSpec;
use tiechui_plugin_api::protocol::CommandInfo;
use tiechui_plugin_api::tools::ToolSource;
use tiechui_plugin_api::{
    CancellationToken, Command, McpOrigin, McpServerDefinition, McpSource, Module, ModuleConfig,
    NetworkConfig, Registry, Tool, ToolError, async_trait, serde_json,
};

pub use client::{DeclineElicitation, ElicitationHandler};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServerState {
    Starting,
    Ready,
    Disabled,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServerStatus {
    pub name: String,
    pub origin: String,
    pub state: ServerState,
    pub tools: Vec<String>,
    pub error: Option<String>,
    pub diagnostics: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<String>,
}

struct Server {
    oauth: Option<oauth::OAuthStore>,
    status: ServerStatus,
    peer: Option<Peer<RoleClient>>,
    tools: Vec<rmcp::model::Tool>,
    resources: bool,
    timeout: Duration,
}

struct Inner {
    servers: RwLock<BTreeMap<String, Server>>,
    cancel: CancellationToken,
    network: NetworkConfig,
}

struct Lifetime(CancellationToken);
impl Drop for Lifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Clone)]
pub struct McpModule {
    inner: Arc<Inner>,
    _lifetime: Arc<Lifetime>,
}

/// A file is one way to produce the same parsed contributions plugins provide.
pub struct FileMcpSource(pub Vec<McpServerDefinition>);
impl FileMcpSource {
    pub fn load(path: &Path, project: bool) -> Result<Self, ToolError> {
        #[derive(serde::Deserialize)]
        struct File {
            #[serde(default)]
            mcp_servers: BTreeMap<String, tiechui_plugin_api::McpServerConfig>,
        }
        let file: File = toml::from_str(&std::fs::read_to_string(path)?)
            .map_err(|_| ToolError(format!("{}: invalid MCP configuration", path.display())))?;
        Ok(Self(
            file.mcp_servers
                .into_iter()
                .map(|(name, config)| McpServerDefinition {
                    name,
                    config,
                    origin: if project {
                        McpOrigin::Project(path.into())
                    } else {
                        McpOrigin::User(path.into())
                    },
                })
                .collect(),
        ))
    }
}
impl McpSource for FileMcpSource {
    fn servers(&self) -> Vec<McpServerDefinition> {
        self.0.clone()
    }
}

impl McpModule {
    /// Returns immediately; each server initializes independently on the runtime.
    pub fn start(
        definitions: Vec<McpServerDefinition>,
        cwd: &Path,
        project_trusted: bool,
        cancel: CancellationToken,
        elicitation: Arc<dyn ElicitationHandler>,
    ) -> Self {
        Self::start_with_profile(
            definitions,
            cwd,
            project_trusted,
            cancel,
            elicitation,
            None,
            NetworkConfig::default(),
        )
    }

    pub fn start_with_profile(
        definitions: Vec<McpServerDefinition>,
        cwd: &Path,
        project_trusted: bool,
        cancel: CancellationToken,
        elicitation: Arc<dyn ElicitationHandler>,
        home: Option<&Path>,
        network: NetworkConfig,
    ) -> Self {
        let cancel = cancel.child_token();
        let inner = Arc::new(Inner {
            servers: RwLock::default(),
            cancel: cancel.clone(),
            network,
        });
        let module = Self {
            inner: inner.clone(),
            _lifetime: Arc::new(Lifetime(cancel)),
        };
        for definition in definitions {
            let name = definition.name.clone();
            let trusted = project_trusted
                || !matches!(
                    definition.origin,
                    McpOrigin::Project(_) | McpOrigin::Plugin { project: true, .. }
                );
            let disabled = definition.config.enabled == Some(false) || !trusted;
            let oauth = home
                .zip(definition.config.url.as_deref())
                .filter(|_| {
                    definition.config.bearer_token_env_var.is_none()
                        && !definition
                            .config
                            .http_headers
                            .keys()
                            .any(|key| key.eq_ignore_ascii_case("authorization"))
                })
                .map(|(home, url)| oauth::OAuthStore::new(home, &name, url, inner.network.clone()));
            let origin = match &definition.origin {
                McpOrigin::User(path) | McpOrigin::Project(path) => path.display().to_string(),
                McpOrigin::Extension(id) => id.clone(),
                McpOrigin::Plugin { name, root, .. } => format!("plugin:{name}:{}", root.display()),
            };
            let mut servers = inner.servers.write().unwrap_or_else(|e| e.into_inner());
            if let Some(existing) = servers.get_mut(&name) {
                existing
                    .status
                    .diagnostics
                    .push(format!("Duplicate server from {origin} ignored"));
                continue;
            }
            servers.insert(
                name.clone(),
                Server {
                    oauth: oauth.clone(),
                    status: ServerStatus {
                        name: name.clone(),
                        origin,
                        state: if disabled {
                            ServerState::Disabled
                        } else {
                            ServerState::Starting
                        },
                        tools: vec![],
                        error: None,
                        oauth: None,
                        diagnostics: if !trusted {
                            vec!["Untrusted project server ignored".into()]
                        } else {
                            definition
                                .config
                                .unknown
                                .keys()
                                .map(|key| format!("Unknown MCP key: {key}"))
                                .collect()
                        },
                    },
                    peer: None,
                    tools: vec![],
                    resources: false,
                    timeout: Duration::from_secs(60),
                },
            );
            drop(servers);
            if !disabled {
                tokio::spawn(client::run_server(
                    inner.clone(),
                    definition,
                    cwd.to_owned(),
                    elicitation.clone(),
                    oauth,
                ));
            }
        }
        module
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_config(
        config: &ModuleConfig,
        sources: &[Arc<dyn McpSource>],
        cwd: &Path,
        project: Option<&Path>,
        trusted: bool,
        cancel: CancellationToken,
        elicitation: Arc<dyn ElicitationHandler>,
        home: Option<&Path>,
    ) -> Self {
        let mut definitions: Vec<_> = config
            .mcp_servers
            .iter()
            .map(|(name, server)| {
                let path = config.mcp_origins.get(name).cloned().unwrap_or_default();
                McpServerDefinition {
                    name: name.clone(),
                    config: server.clone(),
                    origin: if project == Some(path.as_path()) {
                        McpOrigin::Project(path)
                    } else {
                        McpOrigin::User(path)
                    },
                }
            })
            .collect();
        definitions.extend(sources.iter().flat_map(|source| source.servers()));
        if config.modules.get("mcp") == Some(&false) {
            for definition in &mut definitions {
                definition.config.enabled = Some(false);
            }
        }
        Self::start_with_profile(
            definitions,
            cwd,
            trusted,
            cancel,
            elicitation,
            home,
            config.network.clone(),
        )
    }

    pub fn statuses(&self) -> Vec<ServerStatus> {
        let tools = self.tools();
        let servers = self.inner.servers.read().unwrap_or_else(|e| e.into_inner());
        servers
            .values()
            .map(|server| {
                let mut status = server.status.clone();
                status.oauth = server.oauth.as_ref().map(|store| store.status().into());
                let names: BTreeSet<_> = server
                    .tools
                    .iter()
                    .flat_map(|raw| {
                        [
                            tool_name(&status.name, &raw.name, false),
                            tool_name(&status.name, &raw.name, true),
                        ]
                    })
                    .collect();
                status.tools = tools
                    .iter()
                    .filter_map(|tool| {
                        let name = tool.spec().name.clone();
                        names.contains(&name).then_some(name)
                    })
                    .collect();
                status
            })
            .collect()
    }

    pub async fn wait_for_startup(&self, cancel: CancellationToken) {
        loop {
            if !self
                .statuses()
                .iter()
                .any(|s| s.state == ServerState::Starting)
            {
                return;
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = self.inner.cancel.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    }
}

impl Module for McpModule {
    fn id(&self) -> &'static str {
        "mcp"
    }
    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        registry.register_tool_source(Arc::new(self.clone()));
        registry.register_resources(Arc::new(self.clone()));
        registry.register_command(Arc::new(StatusCommand(self.clone())));
        Ok(())
    }
}

/// `/mcp`: each server's state, origin, tools and last error.
struct StatusCommand(McpModule);

#[async_trait]
impl Command for StatusCommand {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "mcp".into(),
            description: "Show MCP servers and their tools".into(),
            argument_hint: None,
        }
    }

    async fn run(&self, _: &str, _: CancellationToken) -> Result<String, ToolError> {
        let statuses = self.0.statuses();
        if statuses.is_empty() {
            return Ok("No MCP servers configured.".into());
        }
        let mut lines = Vec::new();
        for status in statuses {
            let state = serde_json::to_value(&status.state)
                .ok()
                .and_then(|state| state.as_str().map(str::to_owned))
                .unwrap_or_default();
            lines.push(format!("{} · {state} · {}", status.name, status.origin));
            if let Some(oauth) = &status.oauth {
                lines.push(format!("  OAuth: {oauth}"));
            }
            if !status.tools.is_empty() {
                lines.push(format!("  tools: {}", status.tools.join(", ")));
            }
            if let Some(error) = &status.error {
                lines.push(format!("  error: {error}"));
            }
        }
        Ok(lines.join("\n"))
    }
}

impl ToolSource for McpModule {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let servers = self.inner.servers.read().unwrap_or_else(|e| e.into_inner());
        let mut counts = BTreeMap::<String, usize>::new();
        for (name, server) in servers.iter() {
            for tool in &server.tools {
                *counts
                    .entry(tool_name(name, &tool.name, false))
                    .or_default() += 1;
            }
        }
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        for (name, server) in servers.iter() {
            let Some(peer) = &server.peer else {
                continue;
            };
            for tool in &server.tools {
                let base = tool_name(name, &tool.name, false);
                let mut schema = Value::Object((*tool.input_schema).clone());
                if let Some(schema) = schema.as_object_mut() {
                    schema.entry("type").or_insert(json!("object"));
                    schema.entry("properties").or_insert(json!({}));
                }
                tools.push(Arc::new(client::McpTool {
                    read_only: tool.annotations.as_ref().and_then(|a| a.read_only_hint)
                        == Some(true),
                    spec: ToolSpec {
                        name: tool_name(name, &tool.name, counts[&base] > 1),
                        description: tool
                            .description
                            .as_deref()
                            .filter(|s| !s.trim().is_empty())
                            .or(tool.title.as_deref())
                            .map(str::to_owned)
                            .unwrap_or_else(|| {
                                format!("MCP tool {} from server {name}", tool.name)
                            }),
                        input_schema: schema,
                    },
                    server: name.clone(),
                    tool: tool.name.to_string(),
                    peer: peer.clone(),
                    timeout: server.timeout,
                    session: self.inner.cancel.clone(),
                    readable_resources: server.resources,
                }));
            }
        }
        if servers
            .values()
            .any(|server| server.peer.is_some() && server.resources)
        {
            tools.extend(resources::tools(Arc::downgrade(&self.inner)));
        }
        tools
    }
}

pub fn tool_name(server: &str, tool: &str, collision: bool) -> String {
    let name: String = format!("mcp__{server}__{tool}")
        .encode_utf16()
        .map(|c| {
            char::from_u32(u32::from(c))
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .unwrap_or('_')
        })
        .collect();
    if name.len() <= 64 && !collision {
        return name;
    }
    let digest = Sha256::digest(format!("{server}\0{tool}").as_bytes());
    let hash = format!("{digest:x}");
    format!("{}_{}", &name[..name.len().min(55)], &hash[..8])
}

impl tiechui_plugin_api::ResourceProvider for McpModule {
    fn resources(
        &self,
        active: &[String],
        _: &[tiechui_plugin_api::protocol::Resource],
    ) -> Vec<tiechui_plugin_api::protocol::Resource> {
        use tiechui_plugin_api::protocol::{Resource, ResourceKind};
        self.statuses()
            .into_iter()
            .filter_map(|server| {
                let tools = server
                    .tools
                    .iter()
                    .filter(|name| active.contains(name))
                    .take(32)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                if tools.is_empty() {
                    return None;
                }
                let description =
                    format!("Tools: {}", tools.chars().take(1000).collect::<String>());
                Some(Resource {
                    source: Some("mcp".into()),
                    manual: false,
                    id: format!("mcp:{}", server.name),
                    kind: ResourceKind::Plugin,
                    name: server.name.clone(),
                    description: description.clone(),
                    path: None,
                    owner: None,
                    search_terms: vec![format!("mcp__{}", server.name)],
                    reference: format!(
                        "@{} (available MCP capabilities: {description})",
                        server.name
                    ),
                })
            })
            .collect()
    }
}
