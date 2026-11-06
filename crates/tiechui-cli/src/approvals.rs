//! Permission prompts and MCP elicitations for the TUI, through the session's queue.

use tiechui_core::Interactions;
use tiechui_module_mcp::ElicitationHandler;
use tiechui_module_permissions::{
    ApprovalDecision as Decision, ApprovalRequest as Request, ApprovalRequester,
};
use tiechui_plugin_api::serde_json::{Value, json};
use tiechui_plugin_api::{CancellationToken, ToolError, async_trait};
use tiechui_protocol::{ApprovalDecision, ApprovalRequest};

pub(crate) struct Prompts(pub Interactions);

#[async_trait]
impl tiechui_module_plugins::PluginHost for Prompts {
    async fn ask(
        &self,
        plugin: &str,
        prompt: tiechui_protocol::PluginPrompt,
        cancel: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError> {
        Ok(self
            .0
            .ask(
                ApprovalRequest::Plugin {
                    plugin: plugin.into(),
                    prompt,
                },
                cancel,
            )
            .await
            .unwrap_or(ApprovalDecision::Cancel))
    }
}

#[async_trait]
impl tiechui_module_ask::QuestionRequester for Prompts {
    async fn request(
        &self,
        questions: Vec<tiechui_protocol::Question>,
        cancel: CancellationToken,
    ) -> Result<Option<Value>, ToolError> {
        Ok(
            match self
                .0
                .ask(ApprovalRequest::Questions { questions }, cancel)
                .await
            {
                Some(ApprovalDecision::Accept { content }) => content,
                _ => None,
            },
        )
    }
}

#[async_trait]
impl ApprovalRequester for Prompts {
    async fn request(
        &self,
        request: Request,
        cancel: CancellationToken,
    ) -> Result<Decision, ToolError> {
        let request = ApprovalRequest::Permission {
            actor: request.actor.map(String::into_boxed_str),
            call_id: request.call.id,
            tool: request.call.name,
            arguments: request.call.arguments,
            surface: request.surface,
            values: request.values,
            matched_pattern: request.matched_pattern,
            origin: request.origin,
            session_patterns: request.session_patterns,
        };
        Ok(match self.0.ask(request, cancel).await {
            Some(ApprovalDecision::AllowOnce) => Decision::AllowOnce,
            Some(ApprovalDecision::AllowSession { both_directions }) => {
                Decision::AllowSession { both_directions }
            }
            Some(ApprovalDecision::Deny { reason }) => Decision::Deny { reason },
            _ => Decision::Deny {
                reason: Some("Approval cancelled.".into()),
            },
        })
    }
}

#[async_trait]
impl ElicitationHandler for Prompts {
    async fn elicit(
        &self,
        server: &str,
        request: Value,
        cancel: CancellationToken,
    ) -> Result<Value, ToolError> {
        // URL elicitations are declined rather than opened.
        if request["mode"] == "url" {
            return Ok(json!({"action": "decline"}));
        }
        let request = ApprovalRequest::Elicitation {
            server: server.into(),
            request,
        };
        Ok(match self.0.ask(request, cancel).await {
            Some(ApprovalDecision::Accept {
                content: Some(content),
            }) => json!({"action": "accept", "content": content}),
            Some(ApprovalDecision::Accept { content: None }) => json!({"action": "accept"}),
            Some(ApprovalDecision::Decline) => json!({"action": "decline"}),
            _ => json!({"action": "cancel"}),
        })
    }
}
