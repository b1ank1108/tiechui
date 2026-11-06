use super::*;
use tiechui_plugin_api::{Command, CommandSession, protocol};

pub(super) struct Diagnostics(pub Arc<Permissions>);

impl Diagnostics {
    fn text(&self) -> String {
        let mode = self.0.mode.get();
        let mut lines = vec![format!(
            "Mode: {}{}",
            label(mode),
            match self.0.headless {
                None => "",
                Some(HeadlessPolicy::Deny) => " (headless, every call denied)",
                Some(HeadlessPolicy::Unavailable) => " (headless)",
            }
        )];
        if mode == PermissionMode::Auto {
            let denials = self.0.denials.lock().unwrap_or_else(|e| e.into_inner());
            lines.push(format!(
                "Classifier blocks: {} in a row, {} total",
                denials.consecutive, denials.total
            ));
        }
        let sessions = self.0.sessions.lock().unwrap_or_else(|e| e.into_inner());
        lines.extend([
            format!("Policy: {}", self.0.policy_source),
            format!("Project: {}", self.0.project_source),
            "No OS sandbox; session grants clear on reload.".into(),
            "Effective rules (later matches win per surface):".into(),
        ]);
        for rule in &self.0.rules {
            lines.push(format!(
                "{} · {} · {:?} · {}",
                rule.surface, rule.pattern, rule.action, rule.origin
            ));
        }
        lines.push("Temporary session grants:".into());
        for rule in &sessions.rules {
            lines.push(format!(
                "{} · {} · {:?}",
                rule.surface, rule.pattern, rule.action
            ));
        }
        if sessions.rules.is_empty() {
            lines.push("None".into());
        }
        lines.push("Recent decisions (this instance):".into());
        let recent = self.0.recent.lock().unwrap_or_else(|e| e.into_inner());
        lines.extend(recent.iter().skip(recent.len().saturating_sub(20)).cloned());
        if recent.is_empty() {
            lines.push("None".into());
        }
        lines.join("\n")
    }

    fn switch(&self, arguments: &str) -> Option<String> {
        let mode = match arguments.trim() {
            "rules" => PermissionMode::Rules,
            "auto" => PermissionMode::Auto,
            "yolo" => PermissionMode::Yolo,
            _ => return None,
        };
        self.0.mode.set(mode);
        Some(format!("Permission mode: {}.", label(mode)))
    }
}

fn label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::Rules => "rules",
        PermissionMode::Auto => "auto",
        PermissionMode::Yolo => "yolo",
    }
}

const USAGE: &str = "Usage: /permissions [rules|auto|yolo]";

#[async_trait]
impl Command for Diagnostics {
    fn info(&self) -> protocol::CommandInfo {
        protocol::CommandInfo {
            name: "permissions".into(),
            description: "Show effective rules and recent decisions, or switch the mode".into(),
            argument_hint: Some("[rules|auto|yolo]".into()),
        }
    }
    fn interactive(&self) -> bool {
        true
    }
    fn allows_busy(&self, _: &str) -> bool {
        true
    }
    async fn run(&self, arguments: &str, cancel: CancellationToken) -> Result<String, ToolError> {
        check(&cancel)?;
        if arguments.trim().is_empty() {
            return Ok(self.text());
        }
        Ok(self.switch(arguments).unwrap_or_else(|| USAGE.into()))
    }
    async fn session_run(
        &self,
        arguments: &str,
        session: CommandSession,
        cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        check(&cancel)?;
        if !arguments.trim().is_empty() {
            return Ok(self.switch(arguments).unwrap_or_else(|| USAGE.into()));
        }
        session
            .host
            .ask(
                protocol::ApprovalRequest::Information {
                    title: "Permissions".into(),
                    text: self.text(),
                },
                cancel,
            )
            .await?;
        Ok(String::new())
    }
}
