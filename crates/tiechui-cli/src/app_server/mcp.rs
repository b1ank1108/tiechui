//! `mcp/*`: the MCP servers of the user's `config.toml`, edited in place, started to report
//! their state, and signed in to.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use tiechui_config::{Config, McpServerConfig};
use tiechui_module_mcp::oauth::{Login, OAuthStore};
use tiechui_module_mcp::{DeclineElicitation, McpModule};
use tiechui_protocol::app::*;
use tokio_util::sync::CancellationToken;

use super::providers::{convert, write};
use super::{Reply, blocking, internal, invalid, parse, value};

pub(super) fn handles(method: &str) -> bool {
    matches!(method, "mcp/list" | "mcp/status") || edits(method)
}

/// Whether the method changes what sessions connect to or with, so open ones reload.
pub(super) fn edits(method: &str) -> bool {
    matches!(
        method,
        "mcp/save" | "mcp/delete" | "mcp/login" | "mcp/logout"
    )
}

/// `authorize` receives the page a login waits on.
pub(super) async fn handle(
    home: &Path,
    method: &str,
    params: Value,
    authorize: impl FnOnce(McpAuthorize),
) -> Reply {
    let file = home.join("config.toml");
    match method {
        "mcp/list" => {
            let servers = user(home).await?.mcp_servers;
            value(servers.into_iter().map(entry).collect::<Vec<_>>())
        }
        "mcp/status" => {
            let config = user(home).await?;
            let cancel = CancellationToken::new();
            let module = McpModule::from_config(
                &config,
                &[],
                home,
                None,
                false,
                cancel.clone(),
                Arc::new(DeclineElicitation),
                Some(home),
            );
            module.wait_for_startup(cancel.clone()).await;
            let statuses: Vec<_> = module
                .statuses()
                .into_iter()
                .map(|status| McpStatus {
                    name: status.name,
                    state: convert(status.state),
                    tools: status.tools,
                    error: status.error,
                    oauth: status.oauth,
                })
                .collect();
            cancel.cancel();
            value(statuses)
        }
        "mcp/save" => {
            let p: McpSaveParams = parse(params)?;
            let (name, server) = config(p.server)?;
            write(move || {
                tiechui_config::save_mcp_server(&file, p.previous.as_deref(), &name, &server)
            })
            .await
        }
        "mcp/delete" => {
            let p: McpParams = parse(params)?;
            write(move || tiechui_config::remove_mcp_server(&file, &p.name)).await
        }
        _ => {
            let p: McpParams = parse(params)?;
            let config = user(home).await?;
            let server = config
                .mcp_servers
                .get(&p.name)
                .ok_or_else(|| invalid(format!("No MCP server named {}", p.name)))?;
            let url = server
                .url
                .as_deref()
                .filter(|_| server.command.is_none())
                .ok_or_else(|| invalid("OAuth requires a Streamable HTTP server"))?;
            let store = OAuthStore::new(home, &p.name, url, config.network.clone());
            let cancel = CancellationToken::new();
            if method == "mcp/logout" {
                store.logout(&cancel).await.map_err(internal)?;
            } else {
                let login = Login::begin(store, &[], &cancel).await.map_err(internal)?;
                authorize(McpAuthorize {
                    name: p.name,
                    url: login.authorization_url.clone(),
                });
                login.finish(&cancel).await.map_err(internal)?;
            }
            Ok(Value::Null)
        }
    }
}

async fn user(home: &Path) -> Result<Config, tiechui_protocol::rpc::Error> {
    let user = home.join("config.toml");
    blocking(move |_| crate::setup::user_config(&user)).await
}

fn entry((name, server): (String, McpServerConfig)) -> McpServerEntry {
    McpServerEntry {
        name,
        command: server.command,
        args: server.args,
        env: server.env,
        url: server.url,
        http_headers: server.http_headers,
        tool_timeout_sec: server.tool_timeout_sec,
        enabled: server.enabled != Some(false),
    }
}

/// The keys `entry` reads, for a server with a command or an address and a positive timeout.
fn config(
    server: McpServerEntry,
) -> Result<(String, McpServerConfig), tiechui_protocol::rpc::Error> {
    let command = server.command.filter(|command| !command.trim().is_empty());
    let url = server.url.filter(|url| !url.trim().is_empty());
    if command.is_some() == url.is_some() {
        return Err(invalid("an MCP server needs exactly one of command or url"));
    }
    if server
        .tool_timeout_sec
        .is_some_and(|seconds| seconds <= 0.0)
    {
        return Err(invalid("tool_timeout_sec must be positive"));
    }
    Ok((
        server.name,
        McpServerConfig {
            command,
            args: server.args,
            env: server.env,
            url,
            http_headers: server.http_headers,
            tool_timeout_sec: server.tool_timeout_sec,
            enabled: (!server.enabled).then_some(false),
            ..Default::default()
        },
    ))
}
