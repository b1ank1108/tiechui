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
impl Children {
    fn base_registry(&self) -> Result<Registry, ToolError> {
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
        Ok(registry)
    }

    fn finish_registry(
        &self,
        mut registry: Registry,
        parent: &ToolContext,
    ) -> Result<Registry, ToolError> {
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

impl ChildRegistry for Children {
    fn build(&self, parent: &ToolContext, identity: &str) -> Result<Registry, ToolError> {
        let mut registry = self.base_registry()?;
        if self.config.modules.get("permissions") != Some(&false) {
            self.permissions.register_child(
                &mut registry,
                &self.config,
                &self.home,
                self.trusted,
                identity,
            )?;
        }
        self.finish_registry(registry, parent)
    }

    fn build_default(&self, parent: &ToolContext) -> Result<Registry, ToolError> {
        let mut registry = self.base_registry()?;
        if self.config.modules.get("permissions") != Some(&false) {
            self.permissions
                .register_default_child(&mut registry, &self.config)?;
        }
        self.finish_registry(registry, parent)
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
            matches!(self.interactions.ask(request, cancel).await, Some(ApprovalDecision::Accept { content: Some(v) }) if v["answers"]["project-agents"]["answers"].as_array().is_some_and(|a| a.iter().any(|v| v == "Run"))),
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

#[cfg(test)]
mod tests {
    use super::*;
    use tiechui_module_permissions::HeadlessPolicy;
    use tiechui_protocol::{SessionEpoch, ThreadId, TurnId};
    use tiechui_tools::ExecutionScope;

    #[tokio::test]
    async fn build_default_mounts_tools_without_loading_default_agent_policy_file() {
        let sandbox = tempfile::tempdir().unwrap();
        let cwd = sandbox.path().canonicalize().unwrap();
        let home = cwd.join("user-home");
        std::fs::create_dir_all(home.join("agents")).unwrap();
        std::fs::write(
            home.join("agents/default.md"),
            "---\nname: default\ndescription: disk\npermission:\n  read: deny\n---\n",
        )
        .unwrap();

        let mut config = ModuleConfig::default();
        config.permissions.rules = Some(vec![PermissionRule {
            surface: "*".into(),
            pattern: "*".into(),
            action: PermissionAction::Allow,
            reason: None,
        }]);
        let children = Children {
            home: home.clone(),
            trusted: true,
            config,
            permissions: PermissionsModule::headless(
                cwd.clone(),
                home,
                HeadlessPolicy::Unavailable,
            ),
            rewind: cwd.join("rewind"),
            mcp: None,
        };
        let parent = ToolContext {
            cwd,
            scope: ExecutionScope {
                thread_id: ThreadId("cli-parent".into()),
                turn_id: TurnId("cli-turn".into()),
            },
            epoch: SessionEpoch(1),
            branch: Arc::default(),
            cancel: CancellationToken::new(),
            updates: Arc::new(|_| {}),
            append_entry: Arc::new(|_| Ok(())),
            invoker: None,
            parent_session: None,
            origin: Default::default(),
        };

        let unnamed = children.build_default(&parent).unwrap();
        assert!(unnamed.tools().get("read").is_some());
        assert!(unnamed.tools().get("bash").is_some());
        assert!(unnamed.tools().get("subagent").is_none());

        let call = model::ToolCall {
            id: "read".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": "note.txt"}),
            opaque: None,
        };
        assert_eq!(
            unnamed.interceptors()[0]
                .interceptor
                .before(&call, parent.clone())
                .await
                .unwrap(),
            BeforeToolCall::Allow
        );

        let named = children.build(&parent, "default").unwrap();
        assert!(matches!(
            named.interceptors()[0]
                .interceptor
                .before(&call, parent)
                .await
                .unwrap(),
            BeforeToolCall::Deny(_)
        ));
    }
}
