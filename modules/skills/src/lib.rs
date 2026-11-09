//! Skill metadata discovery and prompt catalogs.

mod discovery;
mod frontmatter;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tiechui_plugin_api::model::Content;
use tiechui_plugin_api::protocol::{
    CommandInfo, InputContent, RESOURCE_PICKER_EVENT, ResourceKind, ResourcePickerRequest,
    literal_dollar, reference_start,
};
use tiechui_plugin_api::{
    CancellationToken, Command, CommandSession, Compactor, ContextProvider, Module, ModuleConfig,
    Registry, RestoreContext, RestoredContext, SessionEffect, SkillRoot, ToolError, TurnInput,
    async_trait, resolve_tool_path, serde_json,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub path: PathBuf,
    pub source: &'static str,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub root: Root,
    pub disable_model_invocation: bool,
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub kind: &'static str,
    pub path: PathBuf,
    pub message: String,
}

impl Diagnostic {
    fn warning(path: &Path, message: impl Into<String>) -> Self {
        Self {
            kind: "warning",
            path: path.to_owned(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SkillsModule {
    pub skills: Vec<Skill>,
    pub diagnostics: Vec<Diagnostic>,
}

impl SkillsModule {
    /// Run on a blocking worker; `user_home` and `config_home` are supplied by the host.
    pub fn load(
        cwd: &Path,
        user_home: &Path,
        config_home: &Path,
        project_trusted: bool,
        contributed: &[Arc<dyn SkillRoot>],
        cancel: &CancellationToken,
    ) -> Result<Self, ToolError> {
        check(cancel)?;
        let shared = [".agents/skills", ".claude/skills"];
        let mut roots: Vec<_> = shared
            .iter()
            .map(|path| Root {
                path: user_home.join(path),
                source: "user",
            })
            .collect();
        roots.push(Root {
            path: config_home.join("skills"),
            source: "user",
        });
        let mut result = Self::default();
        for path in shared.iter().copied().map(|path| cwd.join(path)) {
            if project_trusted {
                roots.push(Root {
                    path,
                    source: "project",
                });
            } else if path.exists() {
                result.diagnostics.push(Diagnostic::warning(
                    &path,
                    "skipping untrusted project skills",
                ));
            }
        }
        for contributor in contributed {
            check(cancel)?;
            for path in contributor.paths() {
                let path = if path.is_absolute() {
                    path
                } else {
                    cwd.join(path)
                };
                roots.push(Root {
                    path,
                    source: "plugin",
                });
            }
        }
        discovery::load(&roots, &mut result, cancel)?;
        Ok(result)
    }

    pub fn catalog(&self) -> String {
        let mut catalog = String::new();
        for skill in self
            .skills
            .iter()
            .filter(|skill| !skill.disable_model_invocation)
        {
            if catalog.is_empty() {
                catalog.push_str("\n\nThe following skills provide specialized instructions for specific tasks.\nUse the read tool to load a skill's file when the task matches its description.\nWhen a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.\n\n<available_skills>");
            }
            catalog.push_str(&format!(
                "\n  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n  </skill>",
                escape_xml(&skill.name), escape_xml(&skill.description),
                escape_xml(&skill.path.to_string_lossy()),
            ));
        }
        if !catalog.is_empty() {
            catalog.push_str("\n</available_skills>");
        }
        catalog
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "skills": self.skills.iter().map(|skill| serde_json::json!({
                "name": skill.name, "description": skill.description, "path": skill.path,
                "root": skill.root.path, "source": skill.root.source,
                "disable_model_invocation": skill.disable_model_invocation,
            })).collect::<Vec<_>>(),
            "diagnostics": self.diagnostics.iter().map(|diagnostic| serde_json::json!({
                "type": diagnostic.kind, "path": diagnostic.path, "message": diagnostic.message,
            })).collect::<Vec<_>>(),
        })
    }
}

impl Module for SkillsModule {
    fn id(&self) -> &'static str {
        "skills"
    }

    fn register(&self, registry: &mut Registry, config: &ModuleConfig) -> Result<(), ToolError> {
        let mut module = self.clone();
        let disabled = &config.skills.disabled;
        module
            .skills
            .retain(|skill| !disabled.contains(&skill.name));
        let resources = module
            .skills
            .iter()
            .map(|skill| {
                let path = skill.path.to_string_lossy().into_owned();
                let owner = registry
                    .skill_roots()
                    .iter()
                    .find(|root| root.paths().iter().any(|path| skill.path.starts_with(path)))
                    .and_then(|root| root.owner());
                tiechui_plugin_api::protocol::Resource {
                    id: format!("skill:{path}"),
                    kind: tiechui_plugin_api::protocol::ResourceKind::Skill,
                    name: skill.name.clone(),
                    description: skill
                        .description
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .chars()
                        .take(300)
                        .collect(),
                    reference: reference(skill),
                    source: Some(skill.root.source.into()),
                    manual: skill.disable_model_invocation,
                    path: Some(path.clone()),
                    owner,
                    search_terms: vec![path],
                }
            })
            .collect();
        registry.register_resources(Arc::new(Resources(resources)));
        registry.register_context_provider(Arc::new(module.clone()));
        registry.register_compactor(Arc::new(module.clone()));
        registry.register_command(Arc::new(ListCommand(module)));
        Ok(())
    }
}

const SOURCE: &str = "skills";

/// Re-reads skills the model loaded in the summarized history, latest first, unless the
/// retained history still holds the same content.
#[async_trait]
impl Compactor for SkillsModule {
    async fn restore(
        &self,
        context: &RestoreContext,
        cancel: CancellationToken,
    ) -> Result<Vec<RestoredContext>, ToolError> {
        let loaded: HashSet<_> = results(&context.summarized)
            .filter(|(_, error, _)| !error)
            .map(|(id, _, _)| id)
            .collect();
        let activated = context
            .summarized
            .iter()
            .rev()
            .flat_map(|message| message.content.iter().rev())
            .filter_map(|part| match part {
                Content::ToolCall(call) if call.name == "read" && loaded.contains(&call.id) => {
                    resolve_tool_path(call.arguments["path"].as_str()?, &context.lifecycle.cwd).ok()
                }
                _ => None,
            });
        let previous = context
            .previous
            .iter()
            .filter(|item| item.source == SOURCE)
            .filter_map(|item| resolve_tool_path(&item.key, &context.lifecycle.cwd).ok());
        let skills: Vec<_> = self
            .skills
            .iter()
            .filter_map(|skill| {
                let path = resolve_tool_path(&skill.path.to_string_lossy(), &context.lifecycle.cwd);
                Some((path.ok()?, skill))
            })
            .collect();
        let retained: Vec<_> = results(&context.retained)
            .map(|(_, _, text)| text)
            .collect();
        let mut seen = HashSet::new();
        let mut items = Vec::new();
        for path in activated.chain(previous) {
            check(&cancel)?;
            let Some((_, skill)) = skills.iter().find(|(candidate, _)| *candidate == path) else {
                continue;
            };
            if !seen.insert(path) {
                continue;
            }
            let Ok(body) = std::fs::read_to_string(&skill.path) else {
                continue;
            };
            let body = body.trim();
            if body.is_empty() || retained.iter().any(|text| text.contains(body)) {
                continue;
            }
            if let Some(text) = fit(skill, body, context) {
                items.push(RestoredContext {
                    source: SOURCE.into(),
                    key: skill.path.to_string_lossy().into_owned(),
                    version: format!("sha256:{:x}", Sha256::digest(body)),
                    text,
                });
            }
        }
        Ok(items)
    }
}

/// Tool results as call ID, error flag and text.
fn results(
    messages: &[tiechui_plugin_api::model::Message],
) -> impl Iterator<Item = (&String, bool, String)> {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            Content::ToolResult(result) => Some((
                &result.call_id,
                result.is_error,
                result
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        Content::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect(),
            )),
            _ => None,
        })
}

/// The longest prefix of `body` whose rendering fits the per-item budget.
fn fit(skill: &Skill, body: &str, context: &RestoreContext) -> Option<String> {
    let location = skill.path.to_string_lossy();
    let render = |end: usize| {
        let note = if end < body.len() {
            format!("\n[Truncated; read {location} for the full instructions.]")
        } else {
            String::new()
        };
        format!(
            "<skill name=\"{}\" location=\"{}\">\n{}{note}\n</skill>",
            escape_xml(&skill.name),
            escape_xml(&location),
            &body[..end]
        )
    };
    let ends: Vec<_> = body
        .char_indices()
        .map(|(index, c)| index + c.len_utf8())
        .collect();
    let fits = |count: usize| (context.estimate)(&render(ends[count - 1])) <= context.item_tokens;
    let (mut low, mut high) = (0, ends.len());
    while low < high {
        let middle = (low + high).div_ceil(2);
        if fits(middle) {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    (low > 0).then(|| render(ends[low - 1]))
}

/// `/skills`: the loaded skills with their sources, then discovery warnings.
struct ListCommand(SkillsModule);

#[async_trait]
impl Command for ListCommand {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "skills".into(),
            description: "Select a skill to insert into the draft".into(),
            argument_hint: None,
        }
    }

    fn interactive(&self) -> bool {
        true
    }
    fn allows_busy(&self, _: &str) -> bool {
        true
    }

    async fn session_run(
        &self,
        arguments: &str,
        session: CommandSession,
        cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        check(&cancel)?;
        if !arguments.trim().is_empty() {
            return self.run(arguments, cancel).await;
        }
        session
            .host
            .apply(SessionEffect::Emit {
                kind: RESOURCE_PICKER_EVENT.into(),
                data: serde_json::to_value(ResourcePickerRequest {
                    kind: ResourceKind::Skill,
                })
                .map_err(|e| ToolError(e.to_string()))?,
            })
            .await?;
        Ok(String::new())
    }

    async fn run(&self, _: &str, _: CancellationToken) -> Result<String, ToolError> {
        let mut lines: Vec<String> = self
            .0
            .skills
            .iter()
            .map(|skill| {
                let manual = if skill.disable_model_invocation {
                    " · manual only"
                } else {
                    ""
                };
                format!(
                    "{} · {}{manual}\n  {}",
                    skill.name,
                    skill.root.source,
                    skill.path.display()
                )
            })
            .collect();
        if lines.is_empty() {
            lines.push("No skills loaded.".into());
        }
        lines.extend(self.0.diagnostics.iter().map(|diagnostic| {
            format!(
                "{}: {}: {}",
                diagnostic.kind,
                diagnostic.path.display(),
                diagnostic.message
            )
        }));
        Ok(lines.join("\n"))
    }
}

#[async_trait]
impl ContextProvider for SkillsModule {
    fn describe(&self, fragments: &[String]) -> Vec<tiechui_plugin_api::PromptFragment> {
        fragments
            .iter()
            .map(|text| tiechui_plugin_api::PromptFragment {
                label: "skills".into(),
                category: "Skills".into(),
                source: if text.starts_with("<selected_skill ") {
                    "Explicit skill selection"
                } else {
                    "Loaded skill catalog"
                }
                .into(),
                text: text.clone(),
                references: vec![],
            })
            .collect()
    }

    async fn fragments(
        &self,
        input: &TurnInput,
        cancel: CancellationToken,
    ) -> Result<Vec<String>, ToolError> {
        check(&cancel)?;
        let catalog = self.catalog();
        check(&cancel)?;
        let mut fragments = if catalog.is_empty() {
            Vec::new()
        } else {
            vec![catalog]
        };
        let mut name_counts = HashMap::new();
        for skill in &self.skills {
            *name_counts.entry(skill.name.as_str()).or_insert(0) += 1;
        }
        for skill in &self.skills {
            let unambiguous = name_counts[skill.name.as_str()] == 1;
            if input
                .content
                .iter()
                .any(|part| matches!(part, InputContent::Text { text } if selected(skill, text, unambiguous)))
            {
                use tokio::io::AsyncReadExt;
                let mut bytes = Vec::new();
                tokio::select! {
                    _ = cancel.cancelled() => return Err(ToolError("Operation aborted".into())),
                    result = async {
                        let file = tokio::fs::File::open(&skill.path).await?;
                        file.take(131073).read_to_end(&mut bytes).await
                    } => { result.map_err(|e| ToolError(format!("Cannot load skill {}: {e}", skill.name)))?; }
                }
                if bytes.len() > 131072 {
                    return Err(ToolError(format!("Skill {} exceeds 128 KiB", skill.name)));
                }
                let body = String::from_utf8(bytes)
                    .map_err(|e| ToolError(format!("Invalid skill {}: {e}", skill.name)))?;
                fragments.push(format!(
                    "<selected_skill name=\"{}\" path=\"{}\">\n{}\n</selected_skill>",
                    escape_xml(&skill.name),
                    escape_xml(&skill.path.to_string_lossy()),
                    body
                ));
            }
        }
        check(&cancel)?;
        Ok(fragments)
    }
}

fn reference(skill: &Skill) -> String {
    format!(
        "[${}](<{}>)",
        skill.name.replace(['[', ']'], ""),
        skill
            .path
            .to_string_lossy()
            .replace('%', "%25")
            .replace('>', "%3E")
            .replace('<', "%3C")
    )
}

fn selected(skill: &Skill, text: &str, unambiguous: bool) -> bool {
    let bound = reference(skill);
    if text
        .match_indices(&bound)
        .any(|(at, _)| reference_start(text, at))
    {
        return true;
    }
    if !unambiguous || literal_dollar(&skill.name) {
        return false;
    }
    text.char_indices()
        .filter(|(_, c)| *c == '$')
        .any(|(at, _)| {
            if !reference_start(text, at) {
                return false;
            }
            let Some(rest) = text[at + 1..].strip_prefix(&skill.name) else {
                return false;
            };
            if rest
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
            {
                return false;
            }
            // A reference to a different source must never rebind by its visible name.
            !(text[..at].ends_with('[') && rest.starts_with("]("))
        })
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn check(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError("Operation aborted".into()))
    } else {
        Ok(())
    }
}

struct Resources(Vec<tiechui_plugin_api::protocol::Resource>);
impl tiechui_plugin_api::ResourceProvider for Resources {
    fn resources(
        &self,
        _: &[String],
        _: &[tiechui_plugin_api::protocol::Resource],
    ) -> Vec<tiechui_plugin_api::protocol::Resource> {
        self.0.clone()
    }
}
