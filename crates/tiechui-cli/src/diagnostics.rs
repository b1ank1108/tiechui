use tiechui_plugin_api::{
    CancellationToken, Command, ToolError, async_trait, protocol::CommandInfo,
};

/// Registered by the host even when every optional module is disabled.
pub(crate) struct Status {
    pub text: String,
    pub sources: Vec<std::sync::Arc<dyn Command>>,
}

#[async_trait]
impl Command for Status {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "status".into(),
            description: "Show module states and configuration paths".into(),
            argument_hint: None,
        }
    }
    fn allows_busy(&self, _: &str) -> bool {
        true
    }
    async fn run(&self, _: &str, cancel: CancellationToken) -> Result<String, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let mut text = self.text.clone();
        for source in &self.sources {
            text.push_str(&format!(
                "\n{}:\n{}",
                source.info().name,
                source.run("", cancel.child_token()).await?
            ));
        }
        Ok(text)
    }
}
