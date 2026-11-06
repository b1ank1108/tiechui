//! Local declarative plugin packages, registered as individual module instances.

mod extension;
mod freshness;
mod manifest;
pub mod marketplace;
mod mcp;
mod rpc;
mod store;
pub use extension::{Extension, PluginHost, validate_panel};

pub use store::{Installation, add, content_hash, remove};

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use tiechui_plugin_api::{
    CancellationToken, HookFile, HookSource, McpServerDefinition, McpSource, Module, ModuleConfig,
    PluginDirectories, Registry, SkillRoot, ToolError,
};

#[derive(Debug, Clone, Serialize)]
pub struct Plugin {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub root: PathBuf,
    pub data_root: PathBuf,
    pub manifest: PathBuf,
    pub project: bool,
    pub enabled: bool,
    pub skills: Vec<PathBuf>,
    pub prompts: Vec<PathBuf>,
    pub agents: Vec<PathBuf>,
    pub installation: Option<Installation>,
    pub diagnostics: Vec<String>,
    pub extension: Option<tiechui_plugin_api::protocol::PluginExtension>,
    #[serde(skip)]
    servers: Vec<McpServerDefinition>,
    #[serde(skip)]
    hooks: Vec<HookFile>,
}

impl Module for Plugin {
    fn id(&self) -> &str {
        &self.name
    }
    fn default_enabled(&self) -> bool {
        self.enabled
    }
    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        if self.enabled {
            let source = Arc::new(self.clone());
            registry.register_resources(source.clone());
            registry.register_skill_root(source.clone());
            registry.register_mcp_source(source.clone());
            registry.register_hook_source(source);
            registry.register_plugin_directories(PluginDirectories {
                name: self.name.clone(),
                prompts: self.prompts.clone(),
                agents: self.agents.clone(),
            });
        }
        Ok(())
    }
}

impl SkillRoot for Plugin {
    fn owner(&self) -> Option<String> {
        Some(self.root.display().to_string())
    }
    fn paths(&self) -> Vec<PathBuf> {
        self.skills.clone()
    }
}
impl McpSource for Plugin {
    fn servers(&self) -> Vec<McpServerDefinition> {
        self.servers.clone()
    }
}
impl HookSource for Plugin {
    fn files(&self) -> Vec<HookFile> {
        self.hooks.clone()
    }
    fn root(&self) -> &Path {
        &self.root
    }
    fn data_root(&self) -> &Path {
        &self.data_root
    }
    fn content_hash(&self, cancel: &CancellationToken) -> Result<String, ToolError> {
        content_hash(&self.root, cancel)
    }
}

#[derive(Default, Serialize)]
pub struct Plugins {
    pub plugins: Vec<Plugin>,
    pub diagnostics: Vec<String>,
}

impl Plugins {
    pub fn registry(&self, config: &ModuleConfig) -> Result<Registry, ToolError> {
        let modules: Vec<&dyn Module> = self.plugins.iter().map(|p| p as &dyn Module).collect();
        Registry::from_modules(&modules, config)
    }
}

pub fn project_locations(cwd: &Path) -> Vec<PathBuf> {
    let root = cwd
        .ancestors()
        .find(|p| p.join(".git").exists())
        .unwrap_or(cwd);
    let mut dirs: Vec<_> = cwd
        .ancestors()
        .take_while(|p| p.starts_with(root))
        .collect();
    dirs.reverse();
    dirs.into_iter()
        .flat_map(|p| [p.join(".agents/plugins"), p.join(".claude/plugins")])
        .collect()
}

pub fn discover(
    cwd: &Path,
    home: &Path,
    trusted: bool,
    config: &ModuleConfig,
    cancel: &CancellationToken,
) -> Result<Plugins, ToolError> {
    let mut result = Plugins::default();
    let mut names = BTreeSet::new();
    let user = home.join("plugins");
    for (directory, project) in std::iter::once((user.clone(), false)).chain(
        project_locations(cwd)
            .into_iter()
            .filter(|p| p != &user)
            .map(|p| (p, true)),
    ) {
        check(cancel)?;
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                result
                    .diagnostics
                    .push(format!("{}: {e}", directory.display()));
                continue;
            }
        };
        let mut entries = entries.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            check(cancel)?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if !entry.file_type()?.is_dir() {
                result.diagnostics.push(format!(
                    "{}: plugin must be a directory, not a link",
                    entry.path().display()
                ));
                continue;
            }
            let mut plugin = match manifest::load(&entry.path(), project, Some(home), cancel) {
                Ok(plugin) => plugin,
                Err(e) => {
                    result
                        .diagnostics
                        .push(format!("{}: {e}", entry.path().display()));
                    continue;
                }
            };
            if !names.insert(plugin.name.clone()) {
                result.diagnostics.push(format!(
                    "{}: duplicate plugin {} ignored",
                    entry.path().display(),
                    plugin.name
                ));
                continue;
            }
            plugin.enabled =
                (!project || trusted) && config.modules.get(&plugin.name) != Some(&false);
            if project && !trusted {
                plugin
                    .diagnostics
                    .push("Untrusted project plugin ignored; use chui trust allow".into());
            }
            if !project {
                match store::read_installation(home, &plugin.name) {
                    Ok(installation) => plugin.installation = installation,
                    Err(e) => plugin
                        .diagnostics
                        .push(format!("Invalid installation record: {e}")),
                }
            }
            result.plugins.push(plugin);
        }
    }
    Ok(result)
}

fn check(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError("Operation aborted".into()))
    } else {
        Ok(())
    }
}

impl tiechui_plugin_api::ResourceProvider for Plugin {
    fn resources(
        &self,
        active: &[String],
        resolved: &[tiechui_plugin_api::protocol::Resource],
    ) -> Vec<tiechui_plugin_api::protocol::Resource> {
        use tiechui_plugin_api::protocol::{Resource, ResourceKind};
        let tools = active
            .iter()
            .filter(|name| {
                self.servers
                    .iter()
                    .any(|server| name.starts_with(&format!("mcp__{}__", server.name)))
            })
            .take(32)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        let tools: String = tools.chars().take(1000).collect();
        let skills = resolved
            .iter()
            .filter(|r| r.kind == ResourceKind::Skill && r.owner == self.owner())
            .take(16)
            .map(|r| r.name.clone())
            .collect::<Vec<_>>()
            .join(", ");
        if tools.is_empty() && skills.is_empty() {
            return vec![];
        }
        let skills: String = skills.chars().take(500).collect();
        let description = format!("Tools: {tools} · Skills: {skills}");
        vec![Resource {
            source: Some("plugin".into()),
            manual: false,
            id: format!("plugin:{}", self.root.display()),
            kind: ResourceKind::Plugin,
            name: self.name.clone(),
            description: description.clone(),
            path: Some(self.root.display().to_string()),
            owner: Some(self.root.display().to_string()),
            search_terms: vec![],
            reference: format!(
                "@{} (available plugin capabilities: {description})",
                self.name
            ),
        }]
    }
}
