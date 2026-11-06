use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use tiechui_plugin_api::{
    McpServerConfig, ToolError,
    serde_json::{self, Value},
};

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum AgentServer {
    #[serde(rename = "stdio")]
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        cwd: Option<String>,
    },
    #[serde(rename = "streamable-http")]
    Http {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

fn portable(path: &str) -> bool {
    !path.contains('\\') && !path.contains(':') && !Path::new(path).is_absolute()
}

fn contained(root: &Path, relative: &str) -> Result<PathBuf, ToolError> {
    if !portable(relative) {
        return Err(ToolError("path must remain within its plugin root".into()));
    }
    let mut path = root.to_owned();
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir if path != root => {
                path.pop();
            }
            _ => return Err(ToolError("path escapes its plugin root".into())),
        }
        if path.exists() {
            path = path.canonicalize()?;
        }
        if !path.starts_with(root) {
            return Err(ToolError("path escapes its plugin root".into()));
        }
    }
    Ok(path)
}

fn expand(value: &str, root: &Path, data: &Path) -> String {
    // Split once per original placeholder so replacement text is never expanded again.
    value
        .split("${PLUGIN_ROOT}")
        .map(|part| part.replace("${PLUGIN_DATA}", &data.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(&root.to_string_lossy())
}

pub(crate) fn normalize(
    mut value: Value,
    agent: bool,
    root: &Path,
    data: &Path,
) -> Result<McpServerConfig, ToolError> {
    if !agent {
        let object = value
            .as_object_mut()
            .ok_or_else(|| ToolError("server must be an object".into()))?;
        if let Some(transport) = object.remove("type")
            && !matches!(
                transport.as_str(),
                Some("stdio" | "http" | "streamable_http" | "streamable-http")
            )
        {
            return Err(ToolError("unsupported MCP transport".into()));
        }
        let mut config: McpServerConfig =
            serde_json::from_value(value).map_err(|e| ToolError(e.to_string()))?;
        if let Some(cwd) = &config.cwd {
            config.cwd = Some(root.join(cwd));
        }
        return Ok(config);
    }
    let server: AgentServer =
        serde_json::from_value(value).map_err(|e| ToolError(e.to_string()))?;
    match server {
        AgentServer::Stdio {
            command,
            args,
            mut env,
            cwd,
        } => {
            let command = if let Some(relative) = command.strip_prefix("./") {
                contained(root, relative)?.to_string_lossy().into_owned()
            } else if !command.is_empty() && !command.contains(['/', '\\', ':']) {
                command
            } else {
                return Err(ToolError(
                    "command must be a bare executable or a contained ./ path".into(),
                ));
            };
            if env.contains_key("PLUGIN_ROOT") || env.contains_key("PLUGIN_DATA") {
                return Err(ToolError(
                    "env cannot override PLUGIN_ROOT or PLUGIN_DATA".into(),
                ));
            }
            let cwd = cwd.as_deref().unwrap_or("${PLUGIN_ROOT}");
            let cwd = if let Some(relative) = cwd.strip_prefix("./") {
                contained(root, relative)?
            } else {
                let (base, suffix) = if let Some(suffix) = cwd.strip_prefix("${PLUGIN_ROOT}") {
                    (root, suffix)
                } else if let Some(suffix) = cwd.strip_prefix("${PLUGIN_DATA}") {
                    (data, suffix)
                } else {
                    return Err(ToolError(
                        "cwd must use ./, PLUGIN_ROOT, or PLUGIN_DATA".into(),
                    ));
                };
                if !suffix.is_empty() && !suffix.starts_with('/') {
                    return Err(ToolError("invalid cwd suffix".into()));
                }
                contained(base, suffix.strip_prefix('/').unwrap_or(suffix))?
            };
            for value in env.values_mut() {
                *value = expand(value, root, data);
            }
            env.insert("PLUGIN_ROOT".into(), root.to_string_lossy().into_owned());
            env.insert("PLUGIN_DATA".into(), data.to_string_lossy().into_owned());
            Ok(McpServerConfig {
                command: Some(command),
                args: args.iter().map(|arg| expand(arg, root, data)).collect(),
                env,
                cwd: Some(cwd),
                ..Default::default()
            })
        }
        AgentServer::Http { url, mut headers } => {
            let parsed = url::Url::parse(&url).map_err(|e| ToolError(e.to_string()))?;
            let loopback = match parsed.host() {
                Some(url::Host::Domain("localhost")) => true,
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                _ => false,
            };
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host().is_none()
                || (parsed.scheme() == "http" && !loopback)
                || !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.fragment().is_some()
            {
                return Err(ToolError("HTTP URL must use HTTPS (or loopback HTTP), without user information or fragment".into()));
            }
            let mut seen = BTreeSet::new();
            for (name, value) in &headers {
                if name.is_empty()
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                    || !seen.insert(name.to_ascii_lowercase())
                    || value.bytes().any(|b| (b < 32 && b != b'\t') || b == 127)
                {
                    return Err(ToolError("invalid or duplicate HTTP header".into()));
                }
            }
            headers.retain(|name, _| {
                ![
                    "accept",
                    "authorization",
                    "connection",
                    "content-encoding",
                    "content-length",
                    "content-type",
                    "host",
                    "last-event-id",
                    "mcp-protocol-version",
                    "mcp-session-id",
                    "proxy-authorization",
                    "te",
                    "trailer",
                    "transfer-encoding",
                    "upgrade",
                    "user-agent",
                ]
                .contains(&name.to_ascii_lowercase().as_str())
            });
            Ok(McpServerConfig {
                url: Some(url),
                http_headers: headers,
                ..Default::default()
            })
        }
    }
}
