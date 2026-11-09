//! Prompt templates from Markdown files, registered as slash commands.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tiechui_plugin_api::protocol::{CommandInfo, InputContent};
use tiechui_plugin_api::{
    CancellationToken, Command, CommandSession, Module, ModuleConfig, Registry, SessionEffect,
    ToolError, async_trait, serde_json,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub body: String,
    pub path: PathBuf,
    /// `user` or `project`.
    pub source: &'static str,
}

impl Prompt {
    /// The message sent for `/name arguments`.
    pub fn expand(&self, arguments: &str) -> String {
        let arguments = arguments.trim();
        if self.body.contains("$ARGUMENTS") {
            self.body.replace("$ARGUMENTS", arguments)
        } else if arguments.is_empty() {
            self.body.clone()
        } else {
            format!("{}\n\n{arguments}", self.body)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PromptsModule {
    pub prompts: Vec<Prompt>,
    pub warnings: Vec<String>,
}

/// Prompt directories in precedence order, with their source.
pub fn roots(cwd: &Path, config_home: &Path) -> [(PathBuf, &'static str); 3] {
    [
        (config_home.join("prompts"), "user"),
        (cwd.join(".agents/prompts"), "project"),
        (cwd.join(".claude/commands"), "project"),
    ]
}

impl PromptsModule {
    /// Reads the `*.md` files directly in each root; project roots only when trusted.
    pub fn load(cwd: &Path, config_home: &Path, project_trusted: bool) -> Self {
        let mut module = Self::default();
        for (root, source) in roots(cwd, config_home) {
            if source == "project" && !project_trusted {
                if root.exists() {
                    module.warnings.push(format!(
                        "{}: skipping untrusted project prompts",
                        root.display()
                    ));
                }
                continue;
            }
            let Ok(entries) = fs::read_dir(&root) else {
                continue;
            };
            let mut paths: Vec<_> = entries
                .filter_map(|entry| Some(entry.ok()?.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "md") && path.is_file())
                .collect();
            paths.sort();
            for path in paths {
                match read(&path, source) {
                    Ok(prompt) if module.prompts.iter().any(|p| p.name == prompt.name) => {
                        module.warnings.push(format!(
                            "{}: /{} is already defined",
                            path.display(),
                            prompt.name
                        ));
                    }
                    Ok(prompt) => module.prompts.push(prompt),
                    Err(error) => module.warnings.push(format!("{}: {error}", path.display())),
                }
            }
        }
        module
    }
}

/// Commands registered earlier keep their names; register prompts last.
impl Module for PromptsModule {
    fn id(&self) -> &'static str {
        "prompts"
    }

    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        for prompt in &self.prompts {
            registry.register_command(Arc::new(PromptCommand(prompt.clone())));
        }
        Ok(())
    }
}

struct PromptCommand(Prompt);

#[async_trait]
impl Command for PromptCommand {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: self.0.name.clone(),
            description: self.0.description.clone(),
            argument_hint: self.0.argument_hint.clone(),
        }
    }
    fn allows_busy(&self, _: &str) -> bool {
        true
    }
    fn interactive(&self) -> bool {
        true
    }
    /// Without a session there is nothing to send to; show the message instead.
    async fn run(&self, arguments: &str, _: CancellationToken) -> Result<String, ToolError> {
        Ok(self.0.expand(arguments))
    }
    async fn session_run(
        &self,
        arguments: &str,
        session: CommandSession,
        _: CancellationToken,
    ) -> Result<String, ToolError> {
        let text = self.0.expand(arguments);
        session
            .host
            .apply(SessionEffect::Submit {
                content: vec![InputContent::Text { text }],
            })
            .await?;
        Ok(String::new())
    }
}

/// The file contents `read` parses back to these fields.
pub fn render(description: &str, argument_hint: Option<&str>, body: &str) -> String {
    let quote = |value: &str| serde_json::Value::from(value.replace('\n', " ")).to_string();
    let mut text = format!("---\ndescription: {}\n", quote(description));
    if let Some(hint) = argument_hint.filter(|hint| !hint.is_empty()) {
        text.push_str(&format!("argument-hint: {}\n", quote(hint)));
    }
    text.push_str("---\n");
    text.push_str(body.trim());
    text.push('\n');
    text
}

fn read(path: &Path, source: &'static str) -> Result<Prompt, String> {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|name| !name.starts_with('.') && !name.contains(char::is_whitespace))
        .ok_or("the file name is not a command name")?;
    let text = fs::read_to_string(path).map_err(|error| error.to_string())?;
    let text = text
        .strip_prefix('\u{feff}')
        .unwrap_or(&text)
        .replace("\r\n", "\n");
    let (fields, body) = split(&text);
    let field = |key: &str| {
        fields
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim() == key)
            .map(|(_, value)| unquote(value.trim()))
            .filter(|value| !value.is_empty())
    };
    let body = body.trim().to_owned();
    let description = field("description")
        .or_else(|| {
            body.lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .map(Into::into)
        })
        .unwrap_or_default();
    Ok(Prompt {
        name: name.into(),
        description,
        argument_hint: field("argument-hint"),
        body,
        path: path.to_owned(),
        source,
    })
}

/// Frontmatter lines and the body; frontmatter is read as `key: value` lines.
fn split(text: &str) -> (&str, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return ("", text);
    };
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return (&rest[..offset], &rest[offset + line.len()..]);
        }
        offset += line.len();
    }
    ("", text)
}

fn unquote(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        serde_json::from_str(value).unwrap_or_else(|_| value[1..value.len() - 1].into())
    } else if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        value[1..value.len() - 1].replace("''", "'")
    } else {
        value.into()
    }
}
