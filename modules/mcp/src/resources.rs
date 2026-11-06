use std::collections::BTreeSet;
use std::sync::{Arc, Weak};
use std::time::Duration;

use rmcp::{Peer, RoleClient};
use tiechui_plugin_api::model::{ToolCall, ToolSpec};
use tiechui_plugin_api::serde_json::{Value, json};
use tiechui_plugin_api::{Concurrency, Tool, ToolContext, ToolError, ToolOutput, async_trait};

use crate::Inner;

pub(super) fn tools(inner: Weak<Inner>) -> Vec<Arc<dyn Tool>> {
    ["list_mcp_resources", "list_mcp_resource_templates", "read_mcp_resource"].into_iter().map(|name| {
        let read = name == "read_mcp_resource";
        Arc::new(ResourceTool {
            spec: ToolSpec { name: name.into(), description: if read { "Read a specific MCP resource." } else { "List MCP resources. Omit server to list all ready servers." }.into(),
                input_schema: if read { json!({"type":"object","properties":{"server":{"type":"string"},"uri":{"type":"string"}},"required":["server","uri"],"additionalProperties":false}) }
                    else { json!({"type":"object","properties":{"server":{"type":"string"},"cursor":{"type":"string"}},"additionalProperties":false}) } },
            inner: inner.clone(),
        }) as Arc<dyn Tool>
    }).collect()
}

struct ResourceTool {
    spec: ToolSpec,
    inner: Weak<Inner>,
}
type ResourceServer = (String, Peer<RoleClient>, Duration);

fn visible(item: &Value) -> bool {
    let uri = item["uri"]
        .as_str()
        .or(item["uriTemplate"].as_str())
        .unwrap_or_default();
    let mime = item["mimeType"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .replace([' ', '"'], "");
    !uri.starts_with("ui://") && !mime.contains(";profile=mcp-app")
}

#[async_trait]
impl Tool for ResourceTool {
    fn source(&self) -> Option<String> {
        Some("MCP resources".into())
    }
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Shared
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let inner = self
            .inner
            .upgrade()
            .ok_or_else(|| ToolError("MCP session ended".into()))?;
        let filter = call.arguments["server"].as_str();
        let servers: Vec<ResourceServer> = {
            let servers = inner.servers.read().unwrap_or_else(|e| e.into_inner());
            servers
                .iter()
                .filter(|(name, server)| {
                    server.resources && filter.is_none_or(|filter| filter == name.as_str())
                })
                .filter_map(|(name, server)| {
                    server
                        .peer
                        .as_ref()
                        .map(|peer| (name.clone(), peer.clone(), server.timeout))
                })
                .collect()
        };
        if let Some(name) = filter
            && servers.is_empty()
        {
            return Err(ToolError(format!("MCP server {name} has no resources")));
        }
        if self.spec.name == "read_mcp_resource" {
            let (name, peer, timeout) = servers
                .first()
                .filter(|_| filter.is_some())
                .ok_or_else(|| ToolError("server must be provided".into()))?;
            let uri = call.arguments["uri"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| ToolError("uri must be provided".into()))?;
            let result = crate::client::request(
                peer,
                "resources/read",
                json!({"uri":uri}),
                *timeout,
                ctx.cancel,
                inner.cancel.clone(),
            )
            .await?;
            return crate::output::resource(
                name,
                uri,
                result["contents"].as_array().cloned().unwrap_or_default(),
            );
        }
        let (key, method) = if self.spec.name == "list_mcp_resources" {
            ("resources", "resources/list")
        } else {
            ("resourceTemplates", "resources/templates/list")
        };
        let cursor = call.arguments["cursor"].as_str().map(str::to_owned);
        if filter.is_none() && cursor.is_some() {
            return Err(ToolError(
                "cursor can only be used when a server is specified".into(),
            ));
        }
        let mut items = vec![];
        let mut errors = vec![];
        let mut next = None;
        for (name, peer, timeout) in servers {
            let mut cursor = cursor.clone();
            let mut seen = BTreeSet::new();
            loop {
                let params = cursor.as_ref().map_or(json!({}), |c| json!({"cursor":c}));
                match crate::client::request(
                    &peer,
                    method,
                    params,
                    timeout,
                    ctx.cancel.clone(),
                    inner.cancel.clone(),
                )
                .await
                {
                    Ok(result) => {
                        for item in result[key]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter(|item| visible(item))
                        {
                            let mut item = item.clone();
                            if let Some(object) = item.as_object_mut() {
                                object.remove("_meta");
                                object.remove("icons");
                                object.insert("server".into(), json!(name));
                            }
                            items.push(item);
                        }
                        cursor = result["nextCursor"].as_str().map(str::to_owned);
                        if filter.is_some() {
                            next = cursor;
                            break;
                        }
                        let Some(value) = &cursor else {
                            break;
                        };
                        if !seen.insert(value.clone()) {
                            return Err(ToolError("Repeated MCP resource cursor".into()));
                        }
                    }
                    Err(error)
                        if filter.is_none()
                            && !ctx.cancel.is_cancelled()
                            && !inner.cancel.is_cancelled() =>
                    {
                        errors.push(json!({"server":name,"error":error.0}));
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        let mut payload = json!({key:items});
        if let Some(server) = filter {
            payload["server"] = json!(server);
        }
        if let Some(cursor) = next {
            payload["nextCursor"] = json!(cursor);
        }
        if !errors.is_empty() {
            payload["errors"] = json!(errors);
        }
        let mut output = ToolOutput::text(payload.to_string());
        output.structured_content = Some(payload);
        output.details = Some(json!({"server":filter.unwrap_or_default(),"tool":self.spec.name}));
        crate::output::limit(&mut output);
        Ok(output)
    }
}
