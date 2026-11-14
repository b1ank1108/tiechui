//! Permission prompts and MCP elicitations for the TUI, through the session's queue.

use tiechui_core::Interactions;
use tiechui_module_mcp::ElicitationHandler;
use tiechui_module_permissions::{
    ApprovalDecision as Decision, ApprovalRequest as Request, ApprovalRequester,
};
use tiechui_plugin_api::serde_json::{Value, json};
use tiechui_plugin_api::{
    CancellationToken, PermissionMode, SharedPermissionMode, ToolError, async_trait,
};
use tiechui_protocol::{ApprovalDecision, ApprovalRequest, FilePreview};

pub(crate) struct Requester(pub Interactions);

pub(crate) struct Prompts {
    pub(crate) interactions: Interactions,
    mode: SharedPermissionMode,
}

impl Prompts {
    pub(crate) fn new(interactions: Interactions, mode: SharedPermissionMode) -> Self {
        Self { interactions, mode }
    }
}

#[async_trait]
impl tiechui_module_plugins::PluginHost for Prompts {
    async fn ask(
        &self,
        plugin: &str,
        prompt: tiechui_protocol::PluginPrompt,
        cancel: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError> {
        Ok(self
            .interactions
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
                .interactions
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
impl ApprovalRequester for Requester {
    async fn request(
        &self,
        request: Request,
        cancel: CancellationToken,
    ) -> Result<Decision, ToolError> {
        let preview = tiechui_module_editing::preview(&request.call, &request.cwd)
            .map(|(before, after)| Box::new(FilePreview { before, after }));
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
            preview,
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
        if request["mode"] == "url" || self.mode.get() == PermissionMode::Yolo {
            return Ok(json!({"action": "decline"}));
        }
        let request = ApprovalRequest::Elicitation {
            server: server.into(),
            request,
        };
        Ok(match self.interactions.ask(request, cancel).await {
            Some(ApprovalDecision::Accept {
                content: Some(content),
            }) => json!({"action": "accept", "content": content}),
            Some(ApprovalDecision::Accept { content: None }) => json!({"action": "accept"}),
            Some(ApprovalDecision::Decline) => json!({"action": "decline"}),
            _ => json!({"action": "cancel"}),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use tiechui_core::{BoundedRetry, SessionActor, SessionConfig, SessionHandle};
    use tiechui_model::ModelId;
    use tiechui_module_ask::QuestionRequester;
    use tiechui_module_plugins::PluginHost;
    use tiechui_module_subagents::ProjectConfirmation;
    use tiechui_plugin_api::Registry;
    use tiechui_protocol::{
        EventKind, Op, PluginPrompt, Question, SessionEpoch, Submission, SubmissionId, ThreadId,
    };
    use tiechui_session::{ModelRef, SessionStore};

    struct Harness {
        _dir: tempfile::TempDir,
        shutdown: CancellationToken,
        handle: SessionHandle,
        thread: ThreadId,
        prompts: Arc<Prompts>,
        requester: Arc<Requester>,
        mode: SharedPermissionMode,
    }

    impl Harness {
        async fn new(initial: PermissionMode) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let shutdown = CancellationToken::new();
            let store =
                SessionStore::create(dir.path(), dir.path(), &shutdown).expect("session store");
            let (interactions, inbox) = Interactions::new();
            let mode = SharedPermissionMode::default();
            mode.set(initial);
            let requester = Arc::new(Requester(interactions.clone()));
            let prompts = Arc::new(Prompts::new(interactions, mode.clone()));
            let handle = SessionActor::spawn(
                store,
                SessionConfig {
                    compaction: Default::default(),
                    model: ModelRef {
                        provider: "fixture".into(),
                        model: ModelId("fixture/model".into()),
                    },
                    thinking: None,
                    providers: BTreeMap::new(),
                    registry: Registry::default(),
                    retry: Arc::new(BoundedRetry {
                        max_retries: 0,
                        initial_delay: Duration::from_millis(1),
                        max_delay: Duration::from_millis(1),
                    }),
                    interactions: Some(inbox),
                    epoch: SessionEpoch(0),
                    short_cache: false,
                },
                shutdown.clone(),
            );
            let mut harness = Self {
                _dir: dir,
                shutdown,
                handle,
                thread: ThreadId(String::new()),
                prompts,
                requester,
                mode,
            };
            loop {
                let event = harness.handle.events.recv().await.expect("event");
                if matches!(event.event, EventKind::ThreadStarted { .. }) {
                    harness.thread = event.thread_id;
                    break;
                }
            }
            harness
        }

        async fn answer(&mut self, decision: ApprovalDecision) -> ApprovalRequest {
            loop {
                let event = tokio::time::timeout(Duration::from_secs(2), self.handle.events.recv())
                    .await
                    .expect("approval event timeout")
                    .expect("event stream open");
                if let EventKind::ApprovalRequested {
                    approval_id,
                    request,
                    ..
                } = event.event
                {
                    self.handle
                        .submissions
                        .send(Submission {
                            id: SubmissionId("test".into()),
                            thread_id: self.thread.clone(),
                            epoch: SessionEpoch(0),
                            op: Op::ApprovalResponse {
                                approval_id,
                                decision,
                            },
                        })
                        .await
                        .expect("send approval response");
                    return request;
                }
            }
        }

        fn assert_no_pending_approval(&mut self) {
            while let Ok(event) = self.handle.events.try_recv() {
                assert!(
                    !matches!(event.event, EventKind::ApprovalRequested { .. }),
                    "unexpected approval prompt: {event:?}"
                );
            }
        }

        async fn stop(self) {
            self.shutdown.cancel();
            drop(self.handle.submissions);
            let _ = self.handle.task.await;
        }
    }

    #[tokio::test]
    async fn elicit_declines_in_yolo_and_tracks_live_mode_changes() {
        let mut h = Harness::new(PermissionMode::Yolo).await;
        let cancel = CancellationToken::new();

        let declined = tokio::time::timeout(
            Duration::from_millis(200),
            h.prompts
                .elicit("srv", json!({"message": "API token?"}), cancel.clone()),
        )
        .await
        .expect("yolo elicitation should not block on interactions")
        .unwrap();
        assert_eq!(declined, json!({"action": "decline"}));
        h.assert_no_pending_approval();

        h.mode.set(PermissionMode::Rules);
        let prompts = h.prompts.clone();
        let ask_cancel = cancel.clone();
        let asked = tokio::spawn(async move {
            prompts
                .elicit("srv", json!({"message": "Name?"}), ask_cancel)
                .await
                .expect("elicit result")
        });
        let seen = h
            .answer(ApprovalDecision::Accept {
                content: Some(json!({"name": "ada"})),
            })
            .await;
        assert!(matches!(
            seen,
            ApprovalRequest::Elicitation { ref server, .. } if server == "srv"
        ));
        assert_eq!(
            asked.await.unwrap(),
            json!({"action": "accept", "content": {"name": "ada"}})
        );

        let url_declined = h
            .prompts
            .elicit(
                "srv",
                json!({"mode": "url", "url": "https://example.com"}),
                cancel.clone(),
            )
            .await
            .unwrap();
        assert_eq!(url_declined, json!({"action": "decline"}));
        h.assert_no_pending_approval();

        h.mode.set(PermissionMode::Auto);
        let prompts = h.prompts.clone();
        let auto_cancel = cancel.clone();
        let auto_asked = tokio::spawn(async move {
            prompts
                .elicit("srv", json!({"message": "Confirm?"}), auto_cancel)
                .await
                .expect("auto elicit result")
        });
        let seen = h.answer(ApprovalDecision::Decline).await;
        assert!(matches!(seen, ApprovalRequest::Elicitation { .. }));
        assert_eq!(auto_asked.await.unwrap(), json!({"action": "decline"}));

        h.mode.set(PermissionMode::Yolo);
        let switched_back = tokio::time::timeout(
            Duration::from_millis(200),
            h.prompts
                .elicit("srv", json!({"message": "Again?"}), cancel),
        )
        .await
        .expect("elicitation after switching back to yolo should not block")
        .unwrap();
        assert_eq!(switched_back, json!({"action": "decline"}));
        h.assert_no_pending_approval();

        h.stop().await;
    }

    #[tokio::test]
    async fn non_elicitation_prompts_still_ask_in_yolo_mode() {
        let mut h = Harness::new(PermissionMode::Yolo).await;
        let cancel = CancellationToken::new();

        let prompts = h.prompts.clone();
        let q_cancel = cancel.clone();
        let question = tokio::spawn(async move {
            QuestionRequester::request(
                prompts.as_ref(),
                vec![Question {
                    id: "q1".into(),
                    header: "Target".into(),
                    question: "Which env?".into(),
                    options: None,
                }],
                q_cancel,
            )
            .await
            .expect("question result")
        });
        let seen = h
            .answer(ApprovalDecision::Accept {
                content: Some(json!({"answers": {"q1": {"answers": ["staging"]}}})),
            })
            .await;
        assert!(matches!(seen, ApprovalRequest::Questions { .. }));
        assert_eq!(
            question.await.unwrap(),
            Some(json!({"answers": {"q1": {"answers": ["staging"]}}}))
        );

        let prompts = h.prompts.clone();
        let p_cancel = cancel.clone();
        let plugin = tokio::spawn(async move {
            PluginHost::ask(
                prompts.as_ref(),
                "demo-plugin",
                PluginPrompt::Confirm {
                    title: "Capability".into(),
                    text: "Allow network?".into(),
                },
                p_cancel,
            )
            .await
            .expect("plugin prompt result")
        });
        let seen = h.answer(ApprovalDecision::AllowOnce).await;
        assert!(matches!(seen, ApprovalRequest::Plugin { .. }));
        assert_eq!(plugin.await.unwrap(), ApprovalDecision::AllowOnce);

        let prompts = h.prompts.clone();
        let c_cancel = cancel.clone();
        let confirm = tokio::spawn(async move {
            ProjectConfirmation::confirm(
                prompts.as_ref(),
                vec![PathBuf::from(".agents/reviewer.md")],
                c_cancel,
            )
            .await
            .expect("project confirmation result")
        });
        let seen = h
            .answer(ApprovalDecision::Accept {
                content: Some(json!({"answers": {"project-agents": {"answers": ["Run"]}}})),
            })
            .await;
        assert!(matches!(seen, ApprovalRequest::Questions { .. }));
        assert!(confirm.await.unwrap());

        let requester = h.requester.clone();
        let dir = h._dir.path().to_path_buf();
        let a_cancel = cancel.clone();
        let approval = tokio::spawn(async move {
            ApprovalRequester::request(
                requester.as_ref(),
                Request {
                    actor: None,
                    call: tiechui_model::ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        arguments: json!({"command": "echo hi"}),
                        opaque: None,
                    },
                    surface: "bash".into(),
                    values: vec!["echo hi".into()],
                    matched_pattern: None,
                    origin: "user".into(),
                    session_patterns: vec![],
                    cwd: dir,
                },
                a_cancel,
            )
            .await
            .expect("approval result")
        });
        let seen = h.answer(ApprovalDecision::AllowOnce).await;
        assert!(matches!(seen, ApprovalRequest::Permission { .. }));
        assert!(matches!(approval.await.unwrap(), Decision::AllowOnce));

        h.stop().await;
    }
}
