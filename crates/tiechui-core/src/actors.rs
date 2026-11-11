use super::*;
use tiechui_plugin_api::{
    CatalogModel, ChildRequest, ChildRunner, ChildThinking, ModelCatalog, StaticContext, ToolError,
    async_trait, split_child_model, unknown_child_model_error,
};

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
    pub is_child: bool,
    pub providers: BTreeMap<String, Arc<dyn ModelProvider>>,
    pub model: ModelRef,
    pub thinking: Option<ReasoningLevel>,
    pub retry: Arc<dyn RetryPolicy>,
}

fn strip_provider<'a>(provider: &str, model: &'a tiechui_model::ModelId) -> &'a str {
    model
        .0
        .strip_prefix(&format!("{provider}/"))
        .unwrap_or(&model.0)
}

fn catalog_entry(
    provider_name: &str,
    provider: &dyn ModelProvider,
    model_id: &tiechui_model::ModelId,
) -> Option<CatalogModel> {
    let capabilities = provider.capabilities(model_id);
    if capabilities.context_window == 0 {
        return None;
    }
    let id = strip_provider(provider_name, model_id);
    let thinking_levels = provider
        .thinking_levels(model_id)
        .into_iter()
        .map(protocol_thinking)
        .collect();
    Some(CatalogModel {
        model: format!("{provider_name}/{id}"),
        reasoning: capabilities.reasoning,
        thinking_levels,
    })
}

impl Runner {
    fn resolve_model(&self, requested: Option<String>) -> Result<ModelRef, ToolError> {
        let Some(raw) = requested.filter(|value| value != "inherit") else {
            return Ok(self.model.clone());
        };
        let catalog = self.model_catalog();
        let (provider, _) = split_child_model(&raw, &catalog)?;
        let target = tiechui_model::ModelId(raw.clone());
        let ready = self
            .providers
            .get(provider)
            .is_some_and(|instance| instance.capabilities(&target).context_window > 0);
        if !ready {
            return Err(unknown_child_model_error(&raw, &catalog));
        }
        Ok(ModelRef {
            provider: provider.into(),
            model: target,
        })
    }

    fn resolve_thinking(
        &self,
        model: &ModelRef,
        requested: Option<ChildThinking>,
    ) -> Option<ReasoningLevel> {
        let desired = match requested {
            Some(ChildThinking::Level(explicit)) => thinking_level(explicit),
            None | Some(ChildThinking::Inherit) => self.thinking,
        };
        let supported = self.providers.get(&model.provider).map_or_else(
            || vec![None],
            |provider| provider.thinking_levels(&model.model),
        );
        clamp(desired, &supported)
    }
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
    fn model_catalog(&self) -> ModelCatalog {
        let active_id = strip_provider(&self.model.provider, &self.model.model);
        let active = format!("{}/{active_id}", self.model.provider);
        let mut models = Vec::new();
        for (name, provider) in &self.providers {
            for model_id in provider.models() {
                if let Some(entry) = catalog_entry(name, provider.as_ref(), &model_id) {
                    models.push(entry);
                }
            }
        }
        if !models.iter().any(|entry| entry.model == active)
            && let Some(provider) = self.providers.get(&self.model.provider)
            && let Some(entry) =
                catalog_entry(&self.model.provider, provider.as_ref(), &self.model.model)
        {
            models.push(entry);
        }
        ModelCatalog {
            is_child: self.is_child,
            active,
            models,
        }
    }

    fn validate_model(&self, model: &str) -> Result<(), ToolError> {
        self.resolve_model(Some(model.to_owned())).map(|_| ())
    }

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
        let model = self.resolve_model(request.model)?;
        let thinking = self.resolve_thinking(&model, request.thinking);
        if !request.system.is_empty() {
            registry.register_context_provider(Arc::new(StaticContext(vec![request.system])));
        }
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
        let mut handle = SessionActor::spawn_child(
            store,
            SessionConfig {
                compaction: Default::default(),
                model,
                thinking,
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
