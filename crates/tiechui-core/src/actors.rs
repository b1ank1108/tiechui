use super::*;
use tiechui_plugin_api::{ChildRequest, ChildRunner, StaticContext, ToolError, async_trait};

struct UsageEvents;
impl tiechui_plugin_api::RuntimeExtension for UsageEvents {
    fn event(
        &self,
        event: &tiechui_plugin_api::RuntimeEvent,
        _: &tiechui_plugin_api::BranchView,
    ) -> Result<tiechui_plugin_api::RuntimeOutput, ToolError> {
        let mut output = tiechui_plugin_api::RuntimeOutput::default();
        if let tiechui_plugin_api::RuntimeEvent::Assistant { usage, .. } = event {
            output.events.push((
                tiechui_plugin_api::ACTOR_USAGE_EVENT.into(),
                serde_json::json!(usage),
            ));
        }
        Ok(output)
    }
}

pub(super) struct Runner {
    pub providers: BTreeMap<String, Arc<dyn ModelProvider>>,
    pub model: ModelRef,
    pub thinking: Option<ReasoningLevel>,
    pub retry: Arc<dyn RetryPolicy>,
}

fn deliver(
    events: &Arc<dyn Fn(Event) + Send + Sync>,
    event: Event,
    shutdown: &CancellationToken,
    diagnostic: &mut Option<String>,
) {
    if diagnostic.is_none()
        && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| events(event))).is_err()
    {
        *diagnostic = Some("Child event consumer panicked".into());
        shutdown.cancel();
    }
}

#[async_trait]
impl ChildRunner for Runner {
    async fn run(
        &self,
        request: ChildRequest,
        mut registry: Registry,
        cancel: CancellationToken,
        events: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<(), ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError("Child cancelled".into()));
        }
        let model = match request.model {
            Some(model) => {
                let (provider, _) = model
                    .split_once('/')
                    .ok_or_else(|| ToolError("Expected provider/model".into()))?;
                ModelRef {
                    provider: provider.into(),
                    model: tiechui_model::ModelId(model),
                }
            }
            None => self.model.clone(),
        };
        registry.register_context_provider(Arc::new(StaticContext(vec![request.system])));
        registry.register_runtime(Arc::new(UsageEvents));
        if let Some(names) = request.tools {
            registry.restrict_tools(names);
        }
        let directory = tempfile::tempdir()?;
        let home = directory.path().to_owned();
        let store_cancel = cancel.clone();
        let store = tokio::task::spawn_blocking(move || {
            SessionStore::create(&home, &request.cwd, &store_cancel)
        })
        .await
        .map_err(|e| ToolError(e.to_string()))?
        .map_err(|e| ToolError(e.to_string()))?;
        let thread_id = ThreadId(store.header().id.clone());
        let shutdown = cancel.child_token();
        let mut handle = SessionActor::spawn(
            store,
            SessionConfig {
                compaction: Default::default(),
                model,
                thinking: self.thinking,
                providers: self.providers.clone(),
                registry,
                retry: self.retry.clone(),
                interactions: None,
                epoch: SessionEpoch(0),
            },
            shutdown.clone(),
        );
        let sent = handle
            .submissions
            .send(Submission {
                id: SubmissionId("child-input".into()),
                thread_id,
                epoch: SessionEpoch(0),
                op: Op::UserInput {
                    content: vec![InputContent::Text { text: request.task }],
                    delivery: InputDelivery::FollowUp,
                },
            })
            .await;
        if sent.is_err() {
            shutdown.cancel();
        }
        let mut terminal = false;
        let mut diagnostic = None;
        loop {
            tokio::select! {
                biased;
                result = handle.events.recv() => match result {
                    Ok(event) => {
                        if let EventKind::TurnCompleted { .. } = &event.event {
                            terminal = true;
                            shutdown.cancel();
                        }
                        deliver(&events, event, &shutdown, &mut diagnostic);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        diagnostic = Some("Child event stream overflowed".into());
                        shutdown.cancel();
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                result = &mut handle.task => {
                    match result {
                        Ok(Ok(())) => {},
                        Ok(Err(error)) => diagnostic = Some(error),
                        Err(error) => diagnostic = Some(format!("Child actor panicked: {error}")),
                    }
                    while let Ok(event) = handle.events.try_recv() {
                        terminal |= matches!(event.event, EventKind::TurnCompleted { .. });
                        deliver(&events, event, &shutdown, &mut diagnostic);
                    }
                    return match diagnostic {
                        Some(error) => Err(ToolError(error)),
                        None if terminal => Ok(()),
                        None => Err(ToolError("Child stopped before completing its turn".into())),
                    };
                }
            }
        }
        shutdown.cancel();
        handle
            .task
            .await
            .map_err(|e| ToolError(e.to_string()))?
            .map_err(ToolError)?;
        match diagnostic {
            Some(error) => Err(ToolError(error)),
            None if terminal => Ok(()),
            None => Err(ToolError("Child event stream closed".into())),
        }
    }
}
