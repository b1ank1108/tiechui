//! Context file discovery and rendering, with content-free session audit data.

mod discovery;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tiechui_plugin_api::{
    CancellationToken, ContextProvider, Module, ModuleConfig, Registry, ToolError, TurnInput,
    async_trait, serde_json,
};

pub const ENTRY_KIND: &str = "context_files";
pub const MAX_BYTES: usize = 32 * 1024;
pub const FILENAMES: &[&str] = &[
    "AGENTS.override.md",
    "AGENTS.md",
    "AGENTS.MD",
    "CLAUDE.md",
    "CLAUDE.MD",
];
const TRUNCATION_NOTICE: &str = "[Context files truncated: 32768 byte limit reached.]";

#[derive(Clone)]
struct ContextFile {
    path: PathBuf,
    content: String,
}

#[derive(Clone)]
struct LoadedContext {
    files: Vec<ContextFile>,
    truncated: bool,
}

#[derive(Debug)]
pub struct Diagnostic {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Clone)]
pub struct ContextFilesModule {
    context: LoadedContext,
}

impl ContextFilesModule {
    /// Run on a blocking worker on every process start, including resume.
    pub fn load(
        cwd: &Path,
        home: &Path,
        cancel: &CancellationToken,
    ) -> Result<(Self, Vec<Diagnostic>), ToolError> {
        check(cancel)?;
        let (context, diagnostics) = discovery::load(cwd, home, cancel)?;
        Ok((Self { context }, diagnostics))
    }

    pub fn entry_data(&self) -> serde_json::Value {
        let files: Vec<_> = self.context.files.iter().map(|file| {
            serde_json::json!({ "path": file.path.display().to_string(), "bytes": file.content.len() })
        }).collect();
        serde_json::json!({ "version": 2, "files": files, "truncated": self.context.truncated })
    }
}

impl Module for ContextFilesModule {
    fn id(&self) -> &'static str {
        "context-files"
    }

    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        registry.register_context_provider(Arc::new(self.clone()));
        Ok(())
    }
}

#[async_trait]
impl ContextProvider for ContextFilesModule {
    fn describe(&self, fragments: &[String]) -> Vec<tiechui_plugin_api::PromptFragment> {
        fragments
            .iter()
            .map(|text| {
                let instructions = text.starts_with("<project_context>");
                tiechui_plugin_api::PromptFragment {
                    label: if instructions {
                        "project_context"
                    } else {
                        "cwd"
                    }
                    .into(),
                    category: if instructions {
                        "Instructions"
                    } else {
                        "System prompt"
                    }
                    .into(),
                    source: "context-files".into(),
                    text: text.clone(),
                    references: if instructions {
                        self.context
                            .files
                            .iter()
                            .filter(|file| text.contains(&file.content))
                            .map(|file| (file.path.display().to_string(), file.content.clone()))
                            .collect()
                    } else {
                        vec![]
                    },
                }
            })
            .collect()
    }

    async fn fragments(
        &self,
        input: &TurnInput,
        cancel: CancellationToken,
    ) -> Result<Vec<String>, ToolError> {
        check(&cancel)?;
        let mut fragments = Vec::new();
        if !self.context.files.is_empty() {
            let mut context =
                "<project_context>\nProject-specific instructions and guidelines:".to_owned();
            for file in &self.context.files {
                check(&cancel)?;
                context.push_str(&format!(
                    "\n\n<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                    file.path.display(),
                    file.content
                ));
            }
            if self.context.truncated {
                context.push_str("\n\n");
                context.push_str(TRUNCATION_NOTICE);
            }
            context.push_str("\n</project_context>");
            fragments.push(context);
        }
        fragments.push(format!(
            "<cwd>\n{}\n</cwd>",
            input.cwd.to_string_lossy().replace('\\', "/")
        ));
        Ok(fragments)
    }
}

fn check(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError("Operation aborted".into()))
    } else {
        Ok(())
    }
}
