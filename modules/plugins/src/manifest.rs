use std::fs;
use std::path::{Component, Path, PathBuf};

use tiechui_plugin_api::{
    CancellationToken, HookFile, McpOrigin, McpServerDefinition, ToolError,
    serde_json::{self, Value},
};

use crate::{Plugin, check};

const AGENT_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
const RESERVED: &[&str] = &[
    "exec",
    "editing",
    "permissions",
    "context-files",
    "skills",
    "mcp",
    "hooks",
    "plugins",
];

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains("..")
        && !name.starts_with('.')
        && !name.ends_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        && !RESERVED.contains(&name)
}

fn valid_agent_name(name: &str) -> bool {
    valid_name(name)
        && name.len() <= 64
        && !name.contains("--")
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-".contains(&b))
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

pub(crate) fn load(
    root: &Path,
    project: bool,
    home: Option<&Path>,
    cancel: &CancellationToken,
) -> Result<Plugin, ToolError> {
    check(cancel)?;
    let root = root.canonicalize()?;
    let native = root.join(".tiechui-plugin/plugin.json");
    let agent = root.join("plugin.json");
    if agent.exists() && !fs::symlink_metadata(&agent)?.file_type().is_file() {
        return Err(ToolError("root plugin.json must be a regular file".into()));
    }
    let agent_value = if agent.is_file() && agent.canonicalize()?.starts_with(&root) {
        read_json(&agent).ok()
    } else {
        None
    };
    let is_agent = agent_value
        .as_ref()
        .and_then(|v| v["$schema"].as_str())
        .is_some_and(|s| s.starts_with("https://agent-plugins.org/schemas/"));
    let manifest = if native.exists() {
        native
    } else if is_agent {
        agent
    } else {
        root.join(".codex-plugin/plugin.json")
    };
    if !fs::symlink_metadata(
        manifest
            .parent()
            .ok_or_else(|| ToolError("missing manifest directory".into()))?,
    )?
    .file_type()
    .is_dir()
    {
        return Err(ToolError("manifest directory must not be a symlink".into()));
    }
    if !manifest.canonicalize()?.starts_with(&root) {
        return Err(ToolError("manifest escapes the plugin root".into()));
    }
    let value = read_json(&manifest)?;
    if !value.is_object() {
        return Err(ToolError("plugin manifest must contain an object".into()));
    }
    let is_agent = manifest == root.join("plugin.json");
    if is_agent && value["$schema"] != AGENT_SCHEMA {
        return Err(ToolError("unsupported Agent Plugins schema".into()));
    }
    let declared_name = optional_string(&value, "name")?;
    let name = declared_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .or_else(|| {
            (!is_agent)
                .then(|| root.file_name().and_then(|name| name.to_str()))
                .flatten()
        })
        .unwrap_or_default();
    if !valid_name(name) || (is_agent && !valid_agent_name(name)) {
        return Err(ToolError(format!(
            "invalid or reserved plugin name {name:?}"
        )));
    }
    let mut plugin = Plugin {
        name: name.into(),
        version: optional_string(&value, "version")?,
        description: optional_string(&value, "description")?,
        data_root: home.map_or_else(
            || root.join(".plugin-data"),
            |home| home.join("plugin-data").join(name),
        ),
        root,
        manifest,
        project,
        enabled: true,
        skills: vec![],
        prompts: vec![],
        agents: vec![],
        installation: None,
        diagnostics: vec![],
        servers: vec![],
        hooks: vec![],
        extension: None,
    };
    if let Some(extension) = value.get("extension") {
        match load_extension(&value, extension) {
            Ok(extension) => plugin.extension = Some(extension),
            Err(error) => plugin
                .diagnostics
                .push(format!("Ignoring extension: {error}")),
        }
    }
    if is_agent {
        plugin.skills = paths(&mut plugin, None, "skills", "skills");
        load_mcp(&mut plugin, None, "mcp.json", cancel);
        let extension = value
            .pointer("/extensions/com.openai")
            .filter(|extension| extension.is_object());
        let overlay_path = plugin.root.join(".codex-plugin/plugin.json");
        let overlay = if extension.is_none() && overlay_path.is_file() {
            Some(read_json(&overlay_path)?)
        } else {
            None
        };
        let hooks_value = extension.or(overlay.as_ref());
        let pointer = if extension.is_some() {
            "/extensions/com.openai/hooks"
        } else {
            "/hooks"
        };
        let source = if overlay.is_some() {
            overlay_path
        } else {
            plugin.manifest.clone()
        };
        load_hooks(
            &mut plugin,
            hooks_value.and_then(|v| v.get("hooks")),
            source,
            pointer,
        );
    } else {
        plugin.skills = paths(&mut plugin, value.get("skills"), "skills", "skills");
        load_mcp(&mut plugin, value.get("mcpServers"), ".mcp.json", cancel);
        let source = plugin.manifest.clone();
        load_hooks(&mut plugin, value.get("hooks"), source, "/hooks");
    }
    plugin.prompts = paths(
        &mut plugin,
        value.get("prompts").filter(|_| !is_agent),
        "prompts",
        "prompts",
    );
    plugin.agents = paths(
        &mut plugin,
        value.get("agents").filter(|_| !is_agent),
        "agents",
        "agents",
    );
    check(cancel)?;
    Ok(plugin)
}

fn load_extension(
    manifest: &Value,
    value: &Value,
) -> Result<tiechui_plugin_api::protocol::PluginExtension, ToolError> {
    use tiechui_plugin_api::protocol::{PluginCapability as C, PluginExtension};
    if manifest
        .pointer("/tiechui/apiVersion")
        .and_then(Value::as_u64)
        != Some(1)
    {
        return Err(ToolError("T2 requires tiechui.apiVersion = 1".into()));
    }
    let extension: PluginExtension =
        serde_json::from_value(value.clone()).map_err(|e| ToolError(e.to_string()))?;
    if extension.command.is_empty()
        || Path::new(&extension.command)
            .components()
            .any(|p| !matches!(p, Component::Normal(_) | Component::CurDir))
    {
        return Err(ToolError(
            "T2 command must be relative to the plugin directory".into(),
        ));
    }
    let contributions = &extension.contributions;
    if contributions
        .tools
        .iter()
        .any(|tool| !tool.input_schema.is_object())
    {
        return Err(ToolError("T2 tool inputSchema must be an object".into()));
    }
    for (populated, capability) in [
        (!contributions.tools.is_empty(), C::Tools),
        (!contributions.commands.is_empty(), C::Commands),
        (!contributions.interceptors.is_empty(), C::Intercept),
    ] {
        if populated && !extension.capabilities.contains(&capability) {
            return Err(ToolError(
                "Contribution requires its declared capability".into(),
            ));
        }
    }
    let mut names = std::collections::BTreeSet::new();
    for (kind, name) in contributions
        .tools
        .iter()
        .map(|v| ("tool", &v.name))
        .chain(contributions.commands.iter().map(|v| ("command", &v.name)))
    {
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || !names.insert((kind, name))
        {
            return Err(ToolError(
                "Invalid or duplicate T2 contribution name".into(),
            ));
        }
    }
    if extension
        .capabilities
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        != extension.capabilities.len()
    {
        return Err(ToolError("Duplicate capability".into()));
    }
    Ok(extension)
}

fn optional_string(value: &Value, field: &str) -> Result<Option<String>, ToolError> {
    value
        .get(field)
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or_else(|| ToolError(format!("{field} must be a string")))
        })
        .transpose()
}

fn read_json(path: &Path) -> Result<Value, ToolError> {
    // Manifests and contribution files cannot redirect reads outside a bundle.
    if fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Err(ToolError(format!(
            "{}: symlink is not allowed",
            path.display()
        )));
    }
    serde_json::from_slice(&fs::read(path)?)
        .map_err(|e| ToolError(format!("{}: {e}", path.display())))
}

fn resolve(plugin: &mut Plugin, value: &str, field: &str) -> Option<PathBuf> {
    let Some(relative) = value.strip_prefix("./") else {
        plugin
            .diagnostics
            .push(format!("Ignoring {field}: path must start with ./"));
        return None;
    };
    if relative.is_empty()
        || Path::new(relative)
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        plugin.diagnostics.push(format!(
            "Ignoring {field}: path must stay within the plugin root"
        ));
        return None;
    }
    let path = plugin.root.join(relative);
    if path.exists()
        && !path
            .canonicalize()
            .is_ok_and(|p| p.starts_with(&plugin.root))
    {
        plugin
            .diagnostics
            .push(format!("Ignoring {field}: path escapes the plugin root"));
        return None;
    }
    Some(path)
}

fn paths(plugin: &mut Plugin, value: Option<&Value>, field: &str, default: &str) -> Vec<PathBuf> {
    let mut paths = match value {
        Some(Value::String(path)) => resolve(plugin, path, field).into_iter().collect(),
        Some(Value::Array(paths)) if paths.iter().all(Value::is_string) => paths
            .iter()
            .filter_map(|p| resolve(plugin, p.as_str().unwrap_or_default(), field))
            .collect(),
        Some(_) => {
            plugin
                .diagnostics
                .push(format!("Ignoring {field}: expected a path or paths"));
            vec![]
        }
        None => vec![],
    };
    if paths.is_empty() {
        paths.extend(resolve(plugin, &format!("./{default}"), field));
    }
    paths.retain(|p| p.is_dir());
    paths.sort();
    paths.dedup();
    paths
}

fn load_hooks(plugin: &mut Plugin, value: Option<&Value>, source: PathBuf, pointer: &str) {
    let mut files = vec![];
    match value {
        Some(Value::String(path)) => {
            files.extend(resolve(plugin, path, "hooks").map(|path| HookFile {
                path,
                pointer: None,
            }))
        }
        Some(Value::Object(_)) => files.push(HookFile {
            path: source,
            pointer: Some(pointer.into()),
        }),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
            for item in items {
                files.extend(
                    resolve(plugin, item.as_str().unwrap_or_default(), "hooks").map(|path| {
                        HookFile {
                            path,
                            pointer: None,
                        }
                    }),
                );
            }
        }
        Some(Value::Array(items)) if items.iter().all(Value::is_object) => {
            for index in 0..items.len() {
                files.push(HookFile {
                    path: source.clone(),
                    pointer: Some(format!("{pointer}/{index}")),
                });
            }
        }
        Some(_) => plugin
            .diagnostics
            .push("Ignoring hooks: expected paths or hook objects".into()),
        None => {}
    }
    if files.is_empty()
        && let Some(path) = resolve(plugin, "./hooks/hooks.json", "hooks")
        && path.is_file()
    {
        files.push(HookFile {
            path,
            pointer: None,
        });
    }
    plugin.hooks = files;
}

fn load_mcp(plugin: &mut Plugin, value: Option<&Value>, default: &str, cancel: &CancellationToken) {
    let result = (|| -> Result<(), ToolError> {
        check(cancel)?;
        let value = match value {
            Some(value @ Value::Object(_)) => value.clone(),
            Some(Value::String(path)) => match resolve(plugin, path, "mcpServers") {
                Some(path) => read_json(&path)?,
                None => return load_default_mcp(plugin, default, cancel),
            },
            Some(_) => {
                plugin
                    .diagnostics
                    .push("Ignoring mcpServers: expected a path or object".into());
                return load_default_mcp(plugin, default, cancel);
            }
            None => {
                let Some(path) = resolve(plugin, &format!("./{default}"), "mcpServers") else {
                    return Ok(());
                };
                if !path.exists() {
                    return Ok(());
                }
                read_json(&path)?
            }
        };
        if default == "mcp.json"
            && value["$schema"] != "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json"
        {
            return Err(ToolError("unsupported Agent Plugins MCP schema".into()));
        }
        let servers = value
            .get("mcpServers")
            .unwrap_or(&value)
            .as_object()
            .ok_or_else(|| ToolError("MCP servers must be an object".into()))?;
        for (name, settings) in servers {
            check(cancel)?;
            let config = match crate::mcp::normalize(
                settings.clone(),
                default == "mcp.json",
                &plugin.root,
                &plugin.data_root,
            ) {
                Ok(config) => config,
                Err(e) => {
                    plugin.diagnostics.push(format!("MCP {name}: {e}"));
                    continue;
                }
            };
            plugin.servers.push(McpServerDefinition {
                name: name.clone(),
                config,
                origin: McpOrigin::Plugin {
                    name: plugin.name.clone(),
                    root: plugin.root.clone(),
                    project: plugin.project,
                },
            });
        }
        Ok(())
    })();
    if let Err(e) = result {
        plugin.diagnostics.push(e.to_string());
    }
}

fn load_default_mcp(
    plugin: &mut Plugin,
    default: &str,
    cancel: &CancellationToken,
) -> Result<(), ToolError> {
    load_mcp(plugin, None, default, cancel);
    Ok(())
}
