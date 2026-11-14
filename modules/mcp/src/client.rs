use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{
    ClientConfig, ClientRequest, ElicitRequestParams, ElicitResult, ElicitationAction,
};
use rmcp::service::{RequestContext, RoleClient, RunningService, serve_client_with_ct};
use rmcp::transport::{
    StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
};
use rmcp::{ClientHandler, Peer};
use tiechui_plugin_api::model::{ToolCall, ToolSpec};
use tiechui_plugin_api::process::{ProcessGroup, deadline, seconds};
use tiechui_plugin_api::serde_json::{self, Value, json};
use tiechui_plugin_api::{
    CancellationToken, Concurrency, McpServerDefinition, NetworkConfig, Tool, ToolContext,
    ToolError, ToolOutput, async_trait,
};

use crate::{Inner, ServerState};

/// Answers server elicitations; hosts supply interactive or declining implementations.
#[async_trait]
pub trait ElicitationHandler: Send + Sync {
    async fn elicit(
        &self,
        server: &str,
        request: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolError>;
}

pub struct DeclineElicitation;
#[async_trait]
impl ElicitationHandler for DeclineElicitation {
    async fn elicit(&self, _: &str, _: Value, _: CancellationToken) -> Result<Value, ToolError> {
        Ok(json!({"action":"decline"}))
    }
}

struct Handler {
    server: String,
    elicitation: Arc<dyn ElicitationHandler>,
}
impl ClientHandler for Handler {
    fn get_info(&self) -> ClientConfig {
        let mut info = ClientConfig::default();
        info.client_info.name = "tiechui".into();
        info.client_info.version = env!("CARGO_PKG_VERSION").into();
        info.capabilities =
            serde_json::from_value(json!({"elicitation":{"form":{},"url":{}}})).unwrap_or_default();
        info
    }

    async fn create_elicitation(
        &self,
        request: ElicitRequestParams,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        let request = serde_json::to_value(request).unwrap_or(Value::Null);
        let response = tokio::select! {
            _ = context.ct.cancelled() => None,
            result = self.elicitation.elicit(&self.server, request, context.ct.clone()) => result.ok(),
        };
        Ok(response
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_else(|| ElicitResult::new(ElicitationAction::Decline)))
    }
}

async fn connect(
    definition: &McpServerDefinition,
    cwd: PathBuf,
    handler: Handler,
    cancel: CancellationToken,
    oauth: Option<crate::oauth::OAuthStore>,
    network: &NetworkConfig,
) -> Result<(RunningService<RoleClient, Handler>, Option<ProcessGroup>), ToolError> {
    let config = &definition.config;
    if definition.name.is_empty()
        || !definition
            .name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(ToolError(
            "Invalid MCP server name; use letters, digits, '_' and '-'".into(),
        ));
    }
    match (&config.command, &config.url) {
        (Some(command), None) => {
            let mut env = network.subprocess_env();
            for (key, value) in &config.env {
                env.insert(key.clone(), value.clone());
            }
            let mut group = ProcessGroup::spawn(
                std::path::Path::new(command),
                &config.args,
                &config.cwd.as_ref().map_or(cwd.clone(), |p| cwd.join(p)),
                &config.env_vars,
                &env,
            )?;
            let stdout = group
                .child
                .stdout
                .take()
                .ok_or_else(|| ToolError("MCP stdout unavailable".into()))?;
            let stdin = group
                .child
                .stdin
                .take()
                .ok_or_else(|| ToolError("MCP stdin unavailable".into()))?;
            let service = serve_client_with_ct(handler, (stdout, stdin), cancel)
                .await
                .map_err(error)?;
            Ok((service, Some(group)))
        }
        (None, Some(url)) => {
            let parsed =
                reqwest::Url::parse(url).map_err(|_| ToolError("Invalid MCP URL".into()))?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(ToolError("MCP URL must use HTTP or HTTPS".into()));
            }
            let mut transport = StreamableHttpClientTransportConfig::with_uri(url.clone());
            for (name, value) in &config.http_headers {
                if matches!(
                    name.to_ascii_lowercase().as_str(),
                    "accept" | "content-type" | "mcp-session-id" | "last-event-id"
                ) {
                    return Err(ToolError(format!("Reserved MCP HTTP header: {name}")));
                }
                transport.custom_headers.insert(
                    name.parse()
                        .map_err(|_| ToolError(format!("Invalid HTTP header name: {name}")))?,
                    value
                        .parse()
                        .map_err(|_| ToolError(format!("Invalid HTTP header value for {name}")))?,
                );
            }
            if let Some(key) = &config.bearer_token_env_var {
                let token = std::env::var(key)
                    .ok()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| {
                        ToolError(format!(
                            "MCP bearer token environment variable {key} is unset or empty"
                        ))
                    })?;
                transport.auth_header = Some(token);
            }
            let http = network
                .apply(reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()))
                .map_err(ToolError)?
                .build()
                .map_err(error)?;
            let transport = StreamableHttpClientTransport::with_client(
                crate::http::HttpClient(http, oauth, cancel.clone()),
                transport,
            );
            let service = serve_client_with_ct(handler, transport, cancel)
                .await
                .map_err(error)?;
            Ok((service, None))
        }
        _ => Err(ToolError(
            "MCP server needs exactly one of command or url".into(),
        )),
    }
}

pub(super) async fn run_server(
    inner: Arc<Inner>,
    definition: McpServerDefinition,
    cwd: PathBuf,
    elicitation: Arc<dyn ElicitationHandler>,
    oauth: Option<crate::oauth::OAuthStore>,
) {
    let name = definition.name.clone();
    let startup = async {
        let startup_timeout = seconds(definition.config.startup_timeout_sec, 10.0)?;
        let tool_timeout = seconds(definition.config.tool_timeout_sec, 60.0)?;
        let startup = async {
            let handler = Handler {
                server: name.clone(),
                elicitation,
            };
            let (service, group) = connect(
                &definition,
                cwd,
                handler,
                inner.cancel.child_token(),
                oauth,
                &inner.network,
            )
            .await?;
            let info = service.peer_info();
            let resources = info
                .as_ref()
                .is_some_and(|info| info.capabilities.resources.is_some());
            let mut tools = if info
                .as_ref()
                .is_some_and(|info| info.capabilities.tools.is_some())
            {
                service.list_all_tools().await.map_err(error)?
            } else {
                vec![]
            };
            tools.retain(|tool| {
                let tool = tool.name.as_ref();
                !definition
                    .config
                    .disabled_tools
                    .iter()
                    .any(|name| name == tool)
                    && definition
                        .config
                        .enabled_tools
                        .as_ref()
                        .is_none_or(|names| names.iter().any(|name| name == tool))
            });
            Ok::<_, ToolError>((service, group, tools, resources, tool_timeout))
        };
        deadline(startup_timeout, inner.cancel.clone(), startup).await
    };
    let outcome = tokio::select! {
        biased;
        _ = inner.cancel.cancelled() => Err(ToolError("MCP session stopped".into())),
        result = startup => result,
    };
    let result = match outcome {
        Ok((service, _group, tools, resources, timeout)) => {
            {
                let mut servers = inner.servers.write().unwrap_or_else(|e| e.into_inner());
                if let Some(server) = servers.get_mut(&name) {
                    server.status.state = ServerState::Ready;
                    server.peer = Some(service.peer().clone());
                    server.tools = tools;
                    server.resources = resources;
                    server.timeout = timeout;
                }
            }
            let stopped = service.cancellation_token();
            let result = tokio::select! {
                _ = inner.cancel.cancelled() => { stopped.cancel(); Ok(()) },
                result = service.waiting() => Err(ToolError(format!("MCP connection closed: {result:?}"))),
            };
            #[cfg(windows)]
            if let Some(mut group) = _group {
                let _ = group.child.kill().await;
            }
            result
        }
        Err(error) => Err(error),
    };
    let mut servers = inner.servers.write().unwrap_or_else(|e| e.into_inner());
    if let Some(server) = servers.get_mut(&name) {
        server.peer = None;
        server.tools.clear();
        server.status.state = if inner.cancel.is_cancelled() {
            ServerState::Stopped
        } else {
            ServerState::Failed
        };
        server.status.error = result.err().map(|e| inner.network.redact(e.0));
    }
}

fn error(error: impl std::fmt::Display) -> ToolError {
    ToolError(tiechui_plugin_api::redact_proxy_secrets(&error.to_string()))
}

pub(super) async fn request(
    peer: &Peer<RoleClient>,
    method: &str,
    params: Value,
    timeout: Duration,
    cancel: CancellationToken,
    session: CancellationToken,
) -> Result<Value, ToolError> {
    let request: ClientRequest =
        serde_json::from_value(json!({"method":method,"params":params})).map_err(error)?;
    tiechui_plugin_api::process::request(peer, request, timeout, cancel, session, None).await
}

pub(super) struct McpTool {
    pub read_only: bool,
    pub spec: ToolSpec,
    pub server: String,
    pub tool: String,
    pub peer: Peer<RoleClient>,
    pub timeout: Duration,
    pub session: CancellationToken,
    pub readable_resources: bool,
}
#[async_trait]
impl Tool for McpTool {
    fn programmatic(&self) -> bool {
        true
    }
    fn deferrable(&self) -> bool {
        true
    }
    fn read_only(&self) -> bool {
        self.read_only
    }
    fn source(&self) -> Option<String> {
        Some(format!("MCP server: {}", self.server))
    }
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        if self.read_only {
            Concurrency::Shared
        } else {
            Concurrency::Exclusive
        }
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let result = request(
            &self.peer,
            "tools/call",
            json!({"name":self.tool,"arguments":call.arguments}),
            self.timeout,
            ctx.cancel,
            self.session.clone(),
        )
        .await?;
        crate::output::convert(&self.server, &self.tool, result, self.readable_resources)
    }
}
