use std::{path::PathBuf, sync::Arc};
use tiechui_module_permissions::PermissionsModule;
use tiechui_plugin_api::*;

pub(crate) struct Children {
    pub home: PathBuf,
    pub trusted: bool,
    pub config: ModuleConfig,
    pub permissions: PermissionsModule,
    pub rewind: PathBuf,
    pub mcp: Option<tiechui_module_mcp::McpModule>,
}
impl ChildRegistry for Children {
    fn build(&self, parent: &ToolContext, identity: &str) -> Result<Registry, ToolError> {
        let mut registry = Registry::from_modules(
            &[
                &tiechui_module_exec::ExecModule { background: false },
                &tiechui_module_tool_search::ToolSearchModule,
                &tiechui_module_ptc::PtcModule,
            ],
            &self.config,
        )?;
        if self.config.modules.get("editing") != Some(&false) {
            tiechui_module_editing::EditingModule.register_child(&mut registry, &self.config)?;
        }
        if self.config.modules.get("permissions") != Some(&false) {
            self.permissions.register_child(
                &mut registry,
                &self.config,
                &self.home,
                self.trusted,
                identity,
            )?;
        }
        if self.config.modules.get("rewind") != Some(&false) {
            tiechui_module_rewind::RewindModule {
                directory: self.rewind.clone(),
            }
            .register_child(&mut registry, parent.clone());
        }
        if let Some(mcp) = &self.mcp {
            mcp.register(&mut registry, &self.config)?;
        }
        Ok(registry)
    }
}

#[async_trait]
impl tiechui_module_subagents::ProjectConfirmation for super::approvals::Prompts {
    async fn confirm(
        &self,
        paths: Vec<PathBuf>,
        cancel: CancellationToken,
    ) -> Result<bool, ToolError> {
        use protocol::*;
        let request = ApprovalRequest::Questions {
            questions: vec![Question {
                id: "project-agents".into(),
                header: "Project agents".into(),
                question: format!(
                    "Run these untrusted project agent definitions?\n{}",
                    paths
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
                options: Some(vec![
                    QuestionOption {
                        label: "Run".into(),
                        description: "Use these definitions for this dispatch.".into(),
                    },
                    QuestionOption {
                        label: "Cancel".into(),
                        description: "Do not run project agents.".into(),
                    },
                ]),
            }],
        };
        Ok(
            matches!(self.0.ask(request, cancel).await, Some(ApprovalDecision::Accept { content: Some(v) }) if v["answers"]["project-agents"]["answers"].as_array().is_some_and(|a| a.iter().any(|v| v == "Run"))),
        )
    }
}

pub(crate) fn register(
    registry: &mut Registry,
    children: Children,
    home: PathBuf,
    trusted: bool,
    prompts: Option<Arc<dyn tiechui_module_subagents::ProjectConfirmation>>,
) -> Result<(), ToolError> {
    if children.config.modules.get("subagents") == Some(&false) {
        return Ok(());
    }
    let config = children.config.clone();
    tiechui_module_subagents::SubagentsModule {
        home,
        trusted,
        confirmation: prompts,
        children: Arc::new(children),
    }
    .register(registry, &config)
}
