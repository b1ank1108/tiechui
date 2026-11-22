use super::*;
use std::time::Duration;
use tiechui_plugin_api::{
    BeforeToolCall, CatalogModel, ChildMode, ChildRequest, ChildRunner, InterceptorStage,
    ModelCatalog, StaticContext, ToolError, ToolInterceptor, async_trait,
};

const FORK_CONFIG_ENTRY: &str = "subagent.fork_config";
const FORK_PLACEHOLDER: &str = "Fork started — processing in background";
const FORK_RECURSION_ERROR: &str = "Forked subagents cannot spawn or manage subagents; complete your task directly using your tools.";

#[derive(Clone, Default)]
pub(super) struct ActorOverrides {
    pub system: Option<Option<String>>,
    pub tools: Option<Vec<tiechui_model::ToolSpec>>,
    pub cache_key: Option<String>,
}

#[derive(Clone)]
pub(super) struct WireRequest {
    pub system: Option<String>,
    pub tools: Vec<tiechui_model::ToolSpec>,
    pub messages: Vec<Message>,
    pub cache_key: Option<String>,
}

#[derive(Clone)]
pub(super) struct ForkAssistant {
    pub message: Message,
    pub model: ModelRef,
    pub stop_reason: StopReason,
    pub reasoning: Option<ReasoningLevel>,
}

#[derive(Clone)]
pub(super) struct ForkSnapshot {
    pub system: Option<String>,
    pub tools: Vec<tiechui_model::ToolSpec>,
    pub messages: Vec<Message>,
    pub assistant: ForkAssistant,
    pub model: ModelRef,
    pub thinking: Option<ReasoningLevel>,
    pub cache_key: Option<String>,
}

#[derive(Clone)]
struct PersistedForkConfig {
    system: Option<String>,
    tools: Vec<tiechui_model::ToolSpec>,
    model: ModelRef,
    thinking: Option<ReasoningLevel>,
    cache_key: Option<String>,
}

struct PreparedChild {
    store: SessionStore,
    _temp_dir: Option<tempfile::TempDir>,
    model: ModelRef,
    thinking: Option<ReasoningLevel>,
    short_cache: bool,
    overrides: ActorOverrides,
}

struct ChildProgress {
    thread_id: ThreadId,
    submitted_users: usize,
    seen_users: usize,
    active_turn: bool,
    terminal: bool,
    failed_or_interrupted: bool,
    diagnostic: Option<String>,
}

impl ChildProgress {
    fn new(thread_id: ThreadId) -> Self {
        Self {
            thread_id,
            submitted_users: 0,
            seen_users: 0,
            active_turn: false,
            terminal: false,
            failed_or_interrupted: false,
            diagnostic: None,
        }
    }

    fn is_settled(&self) -> bool {
        self.terminal
            && !self.active_turn
            && (self.failed_or_interrupted || self.seen_users >= self.submitted_users)
    }
}

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

struct RejectSubagentTools;

#[async_trait]
impl ToolInterceptor for RejectSubagentTools {
    fn stage(&self) -> InterceptorStage {
        InterceptorStage::BeforePolicy
    }

    async fn before(
        &self,
        call: &ToolCall,
        _ctx: ToolContext,
    ) -> Result<BeforeToolCall, ToolError> {
        if call.name == "subagent" || call.name.starts_with("subagent_") {
            return Ok(BeforeToolCall::Deny(FORK_RECURSION_ERROR.into()));
        }
        Ok(BeforeToolCall::Allow)
    }
}

pub(super) struct Runner {
    pub providers: BTreeMap<String, Arc<dyn ModelProvider>>,
    pub model: ModelRef,
    pub thinking: Option<ReasoningLevel>,
    pub retry: Arc<dyn RetryPolicy>,
    pub fork_snapshot: Arc<std::sync::Mutex<Option<ForkSnapshot>>>,
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

fn encode_fork_config(config: &PersistedForkConfig) -> serde_json::Value {
    serde_json::json!({
        "system": config.system,
        "tools": config.tools,
        "model": config.model,
        "thinking": config.thinking,
        "cache_key": config.cache_key,
    })
}

fn decode_fork_config(value: &serde_json::Value) -> Result<PersistedForkConfig, ToolError> {
    let err = |e: serde_json::Error| ToolError(format!("Invalid persisted fork config: {e}"));
    let field = |k: &str| value.get(k).cloned().unwrap_or_default();
    Ok(PersistedForkConfig {
        system: serde_json::from_value(field("system")).map_err(err)?,
        tools: serde_json::from_value(field("tools")).map_err(err)?,
        model: serde_json::from_value(field("model")).map_err(err)?,
        thinking: serde_json::from_value(field("thinking")).map_err(err)?,
        cache_key: serde_json::from_value(field("cache_key")).map_err(err)?,
    })
}

fn find_fork_config(store: &SessionStore) -> Result<Option<PersistedForkConfig>, ToolError> {
    for entry in store.entries() {
        if let EntryKind::Custom { kind, data } = &entry.kind
            && kind == FORK_CONFIG_ENTRY
        {
            return decode_fork_config(data).map(Some);
        }
    }
    Ok(None)
}

fn append_store_entry(
    store: &mut SessionStore,
    kind: EntryKind,
    cancel: &CancellationToken,
) -> Result<(), ToolError> {
    store
        .append(kind, cancel)
        .map(|_| ())
        .map_err(|e| ToolError(e.to_string()))
}

fn seed_message_entry(msg: Message, model: &ModelRef) -> EntryKind {
    let (origin, stop_reason) = if msg.role == Role::Assistant {
        let has_calls = !tool_calls(&msg).is_empty();
        let reason = if has_calls {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        };
        (Some(model.clone()), Some(reason))
    } else {
        (None, None)
    };
    EntryKind::Message {
        message: msg,
        usage: None,
        model: origin,
        stop_reason,
        reasoning: None,
        details: None,
    }
}

fn placeholder_tool_entry(call_id: String) -> EntryKind {
    EntryKind::Message {
        message: Message {
            role: Role::Tool,
            content: vec![Content::ToolResult(ToolResult {
                call_id,
                content: vec![text_content(FORK_PLACEHOLDER.into())],
                is_error: false,
            })],
            opaque: None,
        },
        usage: None,
        model: None,
        stop_reason: None,
        reasoning: None,
        details: None,
    }
}

fn seed_fork_store(
    store: &mut SessionStore,
    snapshot: ForkSnapshot,
    config: &PersistedForkConfig,
    cancel: &CancellationToken,
) -> Result<(), ToolError> {
    append_store_entry(
        store,
        EntryKind::Custom {
            kind: FORK_CONFIG_ENTRY.into(),
            data: encode_fork_config(config),
        },
        cancel,
    )?;
    append_store_entry(
        store,
        EntryKind::ModelChange {
            model: snapshot.model.clone(),
        },
        cancel,
    )?;
    append_store_entry(
        store,
        EntryKind::ThinkingChange {
            level: snapshot.thinking,
        },
        cancel,
    )?;
    seed_fork_messages(
        store,
        &snapshot.model,
        snapshot.messages,
        snapshot.assistant,
        cancel,
    )
}

fn seed_fork_messages(
    store: &mut SessionStore,
    model: &ModelRef,
    messages: Vec<Message>,
    assistant: ForkAssistant,
    cancel: &CancellationToken,
) -> Result<(), ToolError> {
    for msg in messages {
        append_store_entry(store, seed_message_entry(msg, model), cancel)?;
    }
    let calls = tool_calls(&assistant.message);
    append_store_entry(
        store,
        EntryKind::Message {
            message: assistant.message,
            usage: None,
            model: Some(assistant.model),
            stop_reason: Some(assistant.stop_reason),
            reasoning: assistant.reasoning,
            details: None,
        },
        cancel,
    )?;
    for call in calls {
        append_store_entry(store, placeholder_tool_entry(call.id), cancel)?;
    }
    Ok(())
}

fn sync_standard_settings(
    store: &mut SessionStore,
    model: &ModelRef,
    thinking: Option<ReasoningLevel>,
    cancel: &CancellationToken,
) -> Result<(), ToolError> {
    if store.entries().is_empty() {
        return Ok(());
    }
    append_store_entry(
        store,
        EntryKind::ModelChange {
            model: model.clone(),
        },
        cancel,
    )?;
    append_store_entry(store, EntryKind::ThinkingChange { level: thinking }, cancel)
}

fn open_or_temp_store(
    store_path: Option<&std::path::Path>,
    cwd: &std::path::Path,
    cancel: &CancellationToken,
) -> Result<(SessionStore, Option<tempfile::TempDir>), ToolError> {
    if let Some(path) = store_path {
        let store = SessionStore::open_or_create(path, cwd, cancel)
            .map_err(|e| ToolError(e.to_string()))?;
        return Ok((store, None));
    }
    let temp = tempfile::tempdir()?;
    let store =
        SessionStore::create(temp.path(), cwd, cancel).map_err(|e| ToolError(e.to_string()))?;
    Ok((store, Some(temp)))
}

fn load_or_seed_fork_store(
    store_path: Option<&std::path::Path>,
    cwd: &std::path::Path,
    snapshot: Option<ForkSnapshot>,
    cancel: &CancellationToken,
) -> Result<(SessionStore, Option<tempfile::TempDir>, PersistedForkConfig), ToolError> {
    let (mut store, temp_dir) = open_or_temp_store(store_path, cwd, cancel)?;
    if let Some(persisted) = find_fork_config(&store)? {
        return Ok((store, temp_dir, persisted));
    }
    let snap = snapshot.ok_or_else(|| {
        ToolError("Fork snapshot is unavailable outside an active parent tool turn".into())
    })?;
    let config = PersistedForkConfig {
        system: snap.system.clone(),
        tools: snap.tools.clone(),
        model: snap.model.clone(),
        thinking: snap.thinking,
        cache_key: snap.cache_key.clone(),
    };
    seed_fork_store(&mut store, snap, &config, cancel)?;
    Ok((store, temp_dir, config))
}

impl Runner {
    fn available_models_label(&self) -> String {
        let catalog = self.model_catalog();
        if catalog.models.is_empty() {
            "none".into()
        } else {
            catalog
                .models
                .iter()
                .map(|entry| entry.model.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    fn is_model_ready(&self, provider: &str, model: &tiechui_model::ModelId) -> bool {
        self.providers
            .get(provider)
            .is_some_and(|instance| instance.capabilities(model).context_window > 0)
    }

    fn resolve_model(&self, requested: Option<String>) -> Result<ModelRef, ToolError> {
        let Some(raw) = requested else {
            return Ok(self.model.clone());
        };
        let Some((provider, _)) = raw
            .split_once('/')
            .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())
        else {
            return Err(ToolError(format!(
                "Invalid model '{raw}': expected <provider>/<id>. Available models: {}",
                self.available_models_label()
            )));
        };
        let target = tiechui_model::ModelId(raw.clone());
        if !self.is_model_ready(provider, &target) {
            return Err(ToolError(format!(
                "Unknown or unavailable model '{raw}'. Available models: {}",
                self.available_models_label()
            )));
        }
        Ok(ModelRef {
            provider: provider.into(),
            model: target,
        })
    }

    fn validate_model_ref(&self, model: &ModelRef) -> Result<(), ToolError> {
        if self.is_model_ready(&model.provider, &model.model) {
            return Ok(());
        }
        Err(ToolError(format!(
            "Unknown or unavailable model '{}'. Available models: {}",
            model.model.0,
            self.available_models_label()
        )))
    }

    fn resolve_thinking(
        &self,
        model: &ModelRef,
        requested: Option<ThinkingLevel>,
    ) -> Option<ReasoningLevel> {
        let desired = match requested {
            Some(explicit) => thinking_level(explicit),
            None => self.thinking,
        };
        let supported = self.providers.get(&model.provider).map_or_else(
            || vec![None],
            |provider| provider.thinking_levels(&model.model),
        );
        clamp(desired, &supported)
    }

    fn active_fork_snapshot(&self) -> Result<ForkSnapshot, ToolError> {
        self.fork_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| {
                ToolError("Fork snapshot is unavailable outside an active parent tool turn".into())
            })
    }

    async fn prepare_standard(
        &self,
        request: &ChildRequest,
        registry: &mut Registry,
        cancel: &CancellationToken,
    ) -> Result<PreparedChild, ToolError> {
        let model = self.resolve_model(request.model.clone())?;
        let thinking = self.resolve_thinking(&model, request.thinking);
        if !request.system.is_empty() {
            registry
                .register_context_provider(Arc::new(StaticContext(vec![request.system.clone()])));
        }
        registry.register_runtime(Arc::new(UsageEvents));
        if let Some(names) = &request.tools {
            registry.restrict_tools(names.clone());
        }
        let store_path = request.store_path.clone();
        let cwd = request.cwd.clone();
        let store_cancel = cancel.clone();
        let model_for_store = model.clone();
        let (store, temp_dir) = tokio::task::spawn_blocking(move || {
            let (mut store, temp_dir) =
                open_or_temp_store(store_path.as_deref(), &cwd, &store_cancel)?;
            sync_standard_settings(&mut store, &model_for_store, thinking, &store_cancel)?;
            Ok::<_, ToolError>((store, temp_dir))
        })
        .await
        .map_err(|e| ToolError(e.to_string()))??;
        Ok(PreparedChild {
            store,
            _temp_dir: temp_dir,
            model,
            thinking,
            short_cache: true,
            overrides: ActorOverrides::default(),
        })
    }

    fn snapshot_for_fork(
        &self,
        store_path: Option<&std::path::Path>,
    ) -> Result<Option<ForkSnapshot>, ToolError> {
        if store_path.is_some_and(|p| std::fs::symlink_metadata(p).is_ok()) {
            return Ok(self
                .fork_snapshot
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone());
        }
        let snap = self.active_fork_snapshot()?;
        self.validate_model_ref(&snap.model)?;
        Ok(Some(snap))
    }

    async fn prepare_fork(
        &self,
        request: &ChildRequest,
        registry: &mut Registry,
        cancel: &CancellationToken,
    ) -> Result<PreparedChild, ToolError> {
        let snapshot = self.snapshot_for_fork(request.store_path.as_deref())?;
        let store_path = request.store_path.clone();
        let cwd = request.cwd.clone();
        let store_cancel = cancel.clone();
        let (store, temp_dir, config) = tokio::task::spawn_blocking(move || {
            load_or_seed_fork_store(store_path.as_deref(), &cwd, snapshot, &store_cancel)
        })
        .await
        .map_err(|e| ToolError(e.to_string()))??;
        self.validate_model_ref(&config.model)?;
        registry.register_runtime(Arc::new(UsageEvents));
        registry.register_tool_interceptor(Arc::new(RejectSubagentTools), Duration::from_secs(5));
        Ok(PreparedChild {
            store,
            _temp_dir: temp_dir,
            model: config.model,
            thinking: config.thinking,
            short_cache: false,
            overrides: ActorOverrides {
                system: Some(config.system),
                tools: Some(config.tools),
                cache_key: config.cache_key,
            },
        })
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

async fn send_child_input(
    submissions: &mpsc::Sender<Submission>,
    progress: &mut ChildProgress,
    text: String,
    delivery: InputDelivery,
    shutdown: &CancellationToken,
) {
    let id = SubmissionId(format!("child-input-{}", progress.submitted_users));
    let sent = submissions
        .send(Submission {
            id,
            thread_id: progress.thread_id.clone(),
            epoch: SessionEpoch(0),
            op: Op::UserInput {
                content: vec![InputContent::Text { text }],
                delivery,
            },
        })
        .await;
    if sent.is_ok() {
        progress.submitted_users += 1;
    } else {
        shutdown.cancel();
    }
}

async fn drain_ready_steers(
    steer: &mut Option<mpsc::UnboundedReceiver<String>>,
    submissions: &mpsc::Sender<Submission>,
    progress: &mut ChildProgress,
    shutdown: &CancellationToken,
) {
    while let Some(text) = steer.as_mut().and_then(|rx| rx.try_recv().ok()) {
        send_child_input(submissions, progress, text, InputDelivery::Steer, shutdown).await;
    }
}

async fn next_steer(steer: &mut Option<mpsc::UnboundedReceiver<String>>) -> String {
    if let Some(rx) = steer.as_mut()
        && let Some(text) = rx.recv().await
    {
        return text;
    }
    *steer = None;
    std::future::pending().await
}

fn observe_child_event(event: &EventKind, progress: &mut ChildProgress) {
    match event {
        EventKind::TurnStarted { .. } => progress.active_turn = true,
        EventKind::ItemCompleted {
            item: Item::Known(KnownItem::UserMessage { .. }),
            ..
        } => progress.seen_users += 1,
        EventKind::TurnCompleted { turn } => {
            progress.active_turn = false;
            progress.terminal = true;
            progress.failed_or_interrupted |= turn.status != TurnStatus::Completed;
        }
        _ => {}
    }
}

async fn handle_broadcast_event(
    result: Result<Event, broadcast::error::RecvError>,
    steer: &mut Option<mpsc::UnboundedReceiver<String>>,
    submissions: &mpsc::Sender<Submission>,
    events: &Arc<dyn Fn(Event) + Send + Sync>,
    shutdown: &CancellationToken,
    progress: &mut ChildProgress,
) -> bool {
    match result {
        Ok(event) => {
            observe_child_event(&event.event, progress);
            if matches!(event.event, EventKind::TurnCompleted { .. }) {
                drain_ready_steers(steer, submissions, progress, shutdown).await;
                if progress.is_settled() {
                    shutdown.cancel();
                }
            }
            deliver(events, event, shutdown, &mut progress.diagnostic);
            true
        }
        Err(broadcast::error::RecvError::Lagged(_)) => {
            progress.diagnostic = Some("Child event stream overflowed".into());
            shutdown.cancel();
            true
        }
        Err(broadcast::error::RecvError::Closed) => false,
    }
}

fn finish_child_task(
    result: Result<Result<(), String>, tokio::task::JoinError>,
    handle_events: &mut broadcast::Receiver<Event>,
    events: &Arc<dyn Fn(Event) + Send + Sync>,
    shutdown: &CancellationToken,
    progress: &mut ChildProgress,
) -> Result<(), ToolError> {
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => progress.diagnostic = Some(error),
        Err(error) => progress.diagnostic = Some(format!("Child actor panicked: {error}")),
    }
    while let Ok(event) = handle_events.try_recv() {
        observe_child_event(&event.event, progress);
        deliver(events, event, shutdown, &mut progress.diagnostic);
    }
    match progress.diagnostic.take() {
        Some(error) => Err(ToolError(error)),
        None if progress.terminal => Ok(()),
        None => Err(ToolError("Child stopped before completing its turn".into())),
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
        ModelCatalog { active, models }
    }

    fn validate_model(&self, model: &str) -> Result<(), ToolError> {
        self.resolve_model(Some(model.to_owned())).map(|_| ())
    }

    async fn run(
        &self,
        mut request: ChildRequest,
        mut registry: Registry,
        cancel: CancellationToken,
        events: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<(), ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError("Child cancelled".into()));
        }
        let mut steer = request.steer.take();
        let prepared = match request.mode {
            ChildMode::Standard => {
                self.prepare_standard(&request, &mut registry, &cancel)
                    .await?
            }
            ChildMode::Fork => self.prepare_fork(&request, &mut registry, &cancel).await?,
        };
        let _temp_dir = prepared._temp_dir;
        let thread_id = ThreadId(prepared.store.header().id.clone());
        let shutdown = cancel.child_token();
        let mut handle = SessionActor::spawn_with_overrides(
            prepared.store,
            SessionConfig {
                compaction: Default::default(),
                model: prepared.model,
                thinking: prepared.thinking,
                providers: self.providers.clone(),
                registry,
                retry: self.retry.clone(),
                interactions: None,
                epoch: SessionEpoch(0),
                short_cache: prepared.short_cache,
            },
            prepared.overrides,
            shutdown.clone(),
        );
        let mut progress = ChildProgress::new(thread_id);
        send_child_input(
            &handle.submissions,
            &mut progress,
            request.task,
            InputDelivery::FollowUp,
            &shutdown,
        )
        .await;
        loop {
            tokio::select! {
                biased;
                text = next_steer(&mut steer), if steer.is_some() => {
                    send_child_input(&handle.submissions, &mut progress, text, InputDelivery::Steer, &shutdown).await;
                }
                result = handle.events.recv() => {
                    if !handle_broadcast_event(result, &mut steer, &handle.submissions, &events, &shutdown, &mut progress).await {
                        break;
                    }
                }
                result = &mut handle.task => {
                    return finish_child_task(result, &mut handle.events, &events, &shutdown, &mut progress);
                }
            }
        }
        shutdown.cancel();
        let joined = handle.task.await;
        finish_child_task(
            joined,
            &mut handle.events,
            &events,
            &shutdown,
            &mut progress,
        )
    }
}
