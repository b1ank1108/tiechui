//! Single-owner session execution over protocol submissions and events.

mod actors;
mod commands;
mod compaction;
mod estimate;
mod inspection;
mod interactions;
mod interception;
mod replay;
mod retry;
mod runtime;
mod stream;
mod validation;

pub use interactions::{Inbox, Interactions};
pub use retry::{BoundedRetry, RetryPolicy};

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use futures::StreamExt;
use tiechui_model::{
    Content, Message, ModelRequest, ReasoningLevel, Role, StopReason, StreamEvent, ToolCall,
    ToolResult, Usage,
};
use tiechui_plugin_api::{
    LifecycleContext, LifecycleEvent, LifecycleOutcome, Registry, SessionEvent, TurnInput,
};
use tiechui_protocol::*;
use tiechui_providers::{ErrorKind, ModelProvider, ProviderError};
use tiechui_session::{EntryKind, ModelRef, SessionError, SessionStore};
use tiechui_tools::{ExecutionScope, ToolContext, ToolOutput, ToolScheduler, ToolUpdate};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Fragment order and all fragment bytes are caller-owned and retained verbatim.
pub fn assemble_system_prompt(fragments: &[String]) -> String {
    fragments.join("\n\n")
}

pub struct SessionConfig {
    pub compaction: tiechui_plugin_api::CompactionConfig,
    pub model: ModelRef,
    pub thinking: Option<ReasoningLevel>,
    pub providers: BTreeMap<String, Arc<dyn ModelProvider>>,
    pub registry: Registry,
    pub retry: Arc<dyn RetryPolicy>,
    /// Prompts from interactive modules; headless sessions have none.
    pub interactions: Option<Inbox>,
    /// The first epoch. A host that replaces a client's session continues the count, so the
    /// client can tell the old session's events from the new one's.
    pub epoch: SessionEpoch,
    /// Caps prompt-cache writes at the short retention; set for short-lived child sessions.
    pub short_cache: bool,
}

pub struct SessionHandle {
    pub submissions: mpsc::Sender<Submission>,
    pub events: broadcast::Receiver<Event>,
    /// Dropping all submission senders or cancelling shutdown settles the active turn.
    pub task: JoinHandle<Result<(), String>>,
}

/// All mutable session and turn state lives in this task. Worker callbacks carry epochs.
pub struct SessionActor {
    published_resources: std::sync::Mutex<Option<Vec<tiechui_protocol::Resource>>>,
    store: Option<SessionStore>,
    config: SessionConfig,
    registry: Arc<Registry>,
    scheduler: Arc<ToolScheduler>,
    thread: ThreadId,
    epoch: SessionEpoch,
    model: ModelRef,
    /// The level last asked for; `thinking` is it clamped to the current model.
    requested: Option<ReasoningLevel>,
    thinking: Option<ReasoningLevel>,
    total: Usage,
    /// The latest request's usage on the current branch: what the context holds.
    context: Option<Usage>,
    excluded: HashSet<String>,
    system: String,
    system_fragments: Vec<tiechui_plugin_api::PromptFragment>,
    last_request: Option<tiechui_plugin_api::RequestObservation>,
    request_prefix: u64,
    usage_prefixes: VecDeque<(String, u64)>,
    /// The model and monotonic time of the latest response that read or wrote the prompt cache.
    cache_touch: Option<(ModelRef, std::time::Instant)>,
    session_context: Vec<String>,
    turn_context: Vec<String>,
    turn: Option<Turn>,
    submission: Option<SubmissionId>,
    sequence: u64,
    request_sequence: u64,
    stream_items: BTreeMap<u32, ItemId>,
    stream_order: Vec<u32>,
    tool_items: BTreeMap<String, ItemId>,
    steering: VecDeque<Submission>,
    follow_up: VecDeque<Submission>,
    /// Inputs queued by commands, numbering their submission ids.
    submitted: u64,
    navigation: Option<Submission>,
    cancel: CancellationToken,
    shutdown: CancellationToken,
    closing: bool,
    ops: mpsc::Receiver<Submission>,
    events: broadcast::Sender<Event>,
    updates_tx: mpsc::UnboundedSender<ToolUpdate>,
    updates: mpsc::UnboundedReceiver<ToolUpdate>,
    inbox: Option<Inbox>,
    prompts: interactions::Queue,
    commands_tx: mpsc::UnboundedSender<commands::Request>,
    commands_rx: mpsc::UnboundedReceiver<commands::Request>,
    command_cancel: CancellationToken,
    input_blocked: bool,
    automatic_input: Option<tiechui_plugin_api::Continuation>,
    /// Runtime extension asked first for idle work, so one cannot starve the others.
    next_extension: usize,
    settled: Option<TurnStatus>,
    retry_available: bool,
    retry_wake: Option<CancellationToken>,
    compacting: bool,
    pending_compaction: Option<Submission>,
    breaker: compaction::Breaker,
}

impl SessionActor {
    /// Open/create the store on a blocking thread before transferring ownership here.
    pub fn spawn(
        store: SessionStore,
        mut config: SessionConfig,
        shutdown: CancellationToken,
    ) -> SessionHandle {
        let (submissions, ops) = mpsc::channel(64);
        let (events, receiver) = broadcast::channel(1024);
        let (updates_tx, updates) = mpsc::unbounded_channel();
        let (commands_tx, commands_rx) = mpsc::unbounded_channel();
        let registry = Arc::new(std::mem::take(&mut config.registry));
        let inbox = config.interactions.take();
        let epoch = config.epoch;
        let sequence = store.entries().len() as u64;
        let actor = Self {
            published_resources: std::sync::Mutex::new(None),
            thread: ThreadId(store.header().id.clone()),
            model: config.model.clone(),
            requested: config.thinking,
            thinking: config.thinking,
            system: String::new(),
            system_fragments: vec![],
            last_request: None,
            request_prefix: 0,
            usage_prefixes: VecDeque::new(),
            cache_touch: None,
            session_context: vec![],
            turn_context: vec![],
            config,
            store: Some(store),
            registry,
            scheduler: Arc::default(),
            epoch,
            total: Usage::default(),
            context: None,
            excluded: HashSet::new(),
            turn: None,
            submission: None,
            sequence,
            request_sequence: 0,
            stream_items: BTreeMap::new(),
            stream_order: vec![],
            tool_items: BTreeMap::new(),
            steering: VecDeque::new(),
            follow_up: VecDeque::new(),
            submitted: 0,
            navigation: None,
            cancel: CancellationToken::new(),
            shutdown: shutdown.clone(),
            closing: false,
            ops,
            events,
            updates_tx,
            updates,
            inbox,
            prompts: interactions::Queue::default(),
            commands_tx,
            commands_rx,
            command_cancel: shutdown.child_token(),
            input_blocked: false,
            automatic_input: None,
            next_extension: 0,
            settled: None,
            retry_available: false,
            retry_wake: None,
            compacting: false,
            pending_compaction: None,
            breaker: compaction::Breaker::default(),
        };
        let task = tokio::spawn(actor.run());
        SessionHandle {
            submissions,
            events: receiver,
            task,
        }
    }

    fn store(&self) -> Result<&SessionStore, String> {
        self.store
            .as_ref()
            .ok_or_else(|| "session store unavailable".into())
    }

    async fn storage<T: Send + 'static>(
        &mut self,
        operation: impl FnOnce(&mut SessionStore) -> Result<T, SessionError> + Send + 'static,
    ) -> Result<T, String> {
        let mut store = self.store.take().ok_or("session store unavailable")?;
        let (store, result) = tokio::task::spawn_blocking(move || {
            let result = operation(&mut store);
            (store, result)
        })
        .await
        .map_err(|error| error.to_string())?;
        self.store = Some(store);
        result.map_err(|error| error.to_string())
    }

    async fn append(&mut self, kind: EntryKind) -> Result<String, String> {
        // Settlement writes must survive turn cancellation to close every tool call.
        self.storage(move |store| {
            store
                .append(kind, &CancellationToken::new())
                .map(|entry| entry.id.clone())
        })
        .await
    }

    fn emit(&self, event: EventKind) {
        let event = Event {
            submission_id: self.submission.clone(),
            thread_id: self.thread.clone(),
            epoch: self.epoch,
            event,
        };
        if matches!(
            event.event,
            EventKind::TurnStarted { .. }
                | EventKind::TurnCompleted { .. }
                | EventKind::ItemStarted { .. }
                | EventKind::ItemDelta { .. }
                | EventKind::ItemCompleted { .. }
                | EventKind::TokenUsageUpdated { .. }
                | EventKind::Extension { .. }
        ) {
            for observer in self.registry.turn_observers() {
                observer.observe(&event);
            }
        }
        let _ = self.events.send(event);
    }

    fn publish_thread(&self, replay: bool) {
        *self
            .published_resources
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.observe_session(SessionEvent::Started {
            thread_id: self.thread.clone(),
            epoch: self.epoch,
        });
        let turns = match self.history() {
            Ok(turns) => turns,
            Err(message) => {
                self.emit(EventKind::Error { message });
                vec![]
            }
        };
        self.emit(EventKind::ThreadStarted {
            replay,
            thread: Thread {
                id: self.thread.clone(),
                epoch: self.epoch,
                turns,
            },
        });
        if let Some(content) = self.saved_draft() {
            self.emit(EventKind::ComposerRestored { content });
        }
        self.publish_session();
        self.emit(EventKind::ModelsUpdated {
            models: self
                .config
                .providers
                .iter()
                .flat_map(|(provider, models)| {
                    models.models().into_iter().map(|model| ModelRef {
                        provider: provider.clone(),
                        model,
                    })
                })
                .map(|model| self.model_info(&model))
                .collect(),
        });
        self.publish_resources();
        self.emit(EventKind::CommandsUpdated {
            commands: self
                .registry
                .commands()
                .iter()
                .map(|command| command.info())
                .collect(),
        });
        self.emit(EventKind::RetryAvailable {
            available: self.retry_available,
        });
    }

    fn levels(&self, model: &ModelRef) -> Vec<Option<ReasoningLevel>> {
        self.config.providers.get(&model.provider).map_or_else(
            || vec![None],
            |provider| provider.thinking_levels(&model.model),
        )
    }

    fn model_info(&self, model: &ModelRef) -> ModelInfo {
        let capabilities = self
            .config
            .providers
            .get(&model.provider)
            .map(|provider| provider.capabilities(&model.model));
        // Model ids are qualified with their provider entry's name.
        let qualified = &model.model.0;
        let id = qualified
            .strip_prefix(&format!("{}/", model.provider))
            .unwrap_or(qualified);
        ModelInfo {
            provider: model.provider.clone(),
            id: id.to_owned(),
            context_window: capabilities.as_ref().map_or(0, |c| c.context_window),
            reasoning: capabilities.as_ref().is_some_and(|c| c.reasoning),
            thinking_levels: self
                .levels(model)
                .into_iter()
                .map(protocol_thinking)
                .collect(),
        }
    }

    /// Model, thinking level and usage, for status displays.
    fn publish_resources(&self) {
        let resources = self.registry.resources();
        let mut previous = self
            .published_resources
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if previous.as_ref() == Some(&resources) {
            return;
        }
        *previous = Some(resources.clone());
        drop(previous);
        self.emit(EventKind::ResourcesUpdated { resources });
    }

    fn bind_actors(&self) {
        if self.registry.actor_clients().is_empty() {
            return;
        }
        let runner: Arc<dyn tiechui_plugin_api::ChildRunner> = Arc::new(actors::Runner {
            providers: self.config.providers.clone(),
            model: self.model.clone(),
            thinking: self.requested,
            retry: self.config.retry.clone(),
        });
        for client in self.registry.actor_clients() {
            client.bind(runner.clone());
        }
    }

    fn publish_session(&self) {
        self.bind_actors();
        self.emit(EventKind::SessionUpdated {
            model: self.model_info(&self.model),
            thinking: protocol_thinking(self.thinking),
            context: self.context.map(tokens),
            total: tokens(self.total),
        });
    }

    /// Clients render resumed sessions from the current branch; each user message opens a turn.
    fn history(&self) -> Result<Vec<Turn>, String> {
        let store = self.store()?;
        let path = store
            .path_to_root(store.current_leaf().unwrap_or(&store.header().id))
            .map_err(|error| error.to_string())?;
        let mut turns: Vec<Turn> = Vec::new();
        for entry in path {
            if let EntryKind::Compaction {
                summary,
                tokens_before,
                details,
                ..
            } = &entry.kind
            {
                turns.push(Turn {
                    id: TurnId(entry.id.clone()),
                    status: TurnStatus::Completed,
                    items: vec![Item::Known(KnownItem::Compaction {
                        id: ItemId(entry.id.clone()),
                        summary: summary.clone(),
                        tokens_before: *tokens_before,
                        degraded: details["degraded"] == true,
                    })],
                    started_at: entry.timestamp,
                    ended_at: Some(entry.timestamp),
                });
                continue;
            }
            if matches!(entry.kind, EntryKind::CustomMessage { hidden: true, .. }) {
                continue;
            }
            let Some((message, _, _)) = entry.kind.context_message() else {
                continue;
            };
            let details = match &entry.kind {
                EntryKind::Message { details, .. } => details.as_ref(),
                _ => None,
            };
            let mut items = Vec::new();
            for (index, content) in message.content.iter().cloned().enumerate() {
                let id = ItemId(format!("{}-{index}", entry.id));
                match (message.role, content) {
                    (Role::User, Content::Text { text, .. }) => {
                        items.push(KnownItem::UserMessage {
                            id,
                            content: vec![InputContent::Text { text }],
                        })
                    }
                    (Role::User, Content::Image { mime_type, data }) => {
                        items.push(KnownItem::UserMessage {
                            id,
                            content: vec![InputContent::Image { mime_type, data }],
                        })
                    }
                    (Role::Tool, Content::ToolResult(result)) => {
                        let is_error = result.is_error;
                        items.push(KnownItem::ToolResult {
                            id,
                            call_id: ItemId(result.call_id),
                            output: serde_json::json!({ "content": result.content, "is_error": is_error, "details": details }),
                            is_error,
                        })
                    }
                    (Role::Assistant, content) => items.extend(content_item(content, id)),
                    _ => {}
                }
            }
            if (message.role == Role::User
                && !matches!(entry.kind, EntryKind::CustomMessage { .. }))
                || turns.is_empty()
            {
                turns.push(Turn {
                    id: TurnId(entry.id.clone()),
                    status: TurnStatus::Completed,
                    items: vec![],
                    started_at: entry.timestamp,
                    ended_at: None,
                });
            }
            if let Some(turn) = turns.last_mut() {
                turn.items.extend(items.into_iter().map(Item::Known));
                turn.ended_at = Some(entry.timestamp);
            }
        }
        Ok(turns)
    }

    fn restore(&mut self) -> Result<(), String> {
        let store = self.store()?;
        let path = store
            .path_to_root(store.current_leaf().unwrap_or(&store.header().id))
            .map_err(|error| error.to_string())?;
        let mut model = self.config.model.clone();
        let mut thinking = self.config.thinking;
        let mut excluded = HashSet::new();
        let mut explicit_model = false;
        let mut context = None;
        let mut retry_available = false;
        let mut retry_system = String::new();
        let mut retry_context = Vec::new();
        for entry in path {
            match &entry.kind {
                EntryKind::Custom { kind, data } if kind == "request_recovery" => {
                    retry_available = data["system"].is_string();
                    retry_system = data["system"].as_str().unwrap_or_default().into();
                    retry_context = data["context"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect();
                }
                EntryKind::Message { .. } | EntryKind::Compaction { .. } => retry_available = false,
                _ => {}
            }
            match &entry.kind {
                EntryKind::Usage { usage, .. } => context = Some(*usage),
                EntryKind::Compaction { .. } => context = None,
                EntryKind::ModelChange { model: next } => {
                    model = next.clone();
                    explicit_model = true;
                }
                EntryKind::Message {
                    model: Some(next), ..
                } if !explicit_model => model = next.clone(),
                EntryKind::ThinkingChange { level } => thinking = *level,
                EntryKind::Custom { kind, data } if kind == "request_omission" => {
                    if let Some(ids) = data.as_array() {
                        excluded.extend(ids.iter().filter_map(|id| id.as_str().map(str::to_owned)));
                    }
                }
                _ => {}
            }
        }
        let mut total = Usage::default();
        let mut requests = std::collections::HashMap::new();
        // Usage is thread-wide, including branches that are no longer active.
        for entry in store.entries() {
            if let EntryKind::Compaction {
                usage: Some(usage), ..
            } = &entry.kind
            {
                add_usage(&mut total, *usage);
            }
            if let EntryKind::Usage {
                usage, request_id, ..
            } = &entry.kind
            {
                let id = request_id.as_ref().unwrap_or(&entry.id);
                let previous = requests.insert(id, *usage).unwrap_or_default();
                replace_usage(&mut total, previous, *usage);
            }
        }
        self.thinking = clamp(thinking, &self.levels(&model));
        self.requested = thinking;
        self.model = model;
        self.excluded = excluded;
        self.retry_available = retry_available;
        if retry_available {
            self.system = retry_system;
            self.turn_context = retry_context;
        }
        self.total = total;
        self.context = context;
        Ok(())
    }

    async fn run(mut self) -> Result<(), String> {
        let result = self.run_inner().await;
        if let Err(error) = &result {
            self.emit(EventKind::Error {
                message: error.clone(),
            });
            self.complete(TurnStatus::Failed);
        }
        self.quiesce().await;
        self.runtime(tiechui_plugin_api::RuntimeEvent::Shutdown)
            .await?;
        self.command_cancel.cancel();
        self.observe_session(SessionEvent::Ended {
            thread_id: self.thread.clone(),
            epoch: self.epoch,
        });
        self.lifecycle(LifecycleEvent::SessionEnd, CancellationToken::new())
            .await?;
        result
    }

    async fn lifecycle(
        &mut self,
        event: LifecycleEvent,
        cancel: CancellationToken,
    ) -> Result<LifecycleOutcome, String> {
        let ctx = LifecycleContext {
            session_id: self.thread.0.clone(),
            turn_id: self
                .turn
                .as_ref()
                .map(|turn| turn.id.0.clone())
                .unwrap_or_default(),
            cwd: PathBuf::from(&self.store()?.header().cwd),
            transcript_path: Some(self.store()?.path().to_owned()),
            model: self.model.model.0.clone(),
        };
        let registry = self.registry.clone();
        let outcome = self
            .drive(async move {
                let mut outcome = LifecycleOutcome::default();
                for interceptor in registry.lifecycle_interceptors() {
                    let next = match interceptor.on_event(&event, &ctx, cancel.clone()).await {
                        Ok(next) => next,
                        Err(_) if cancel.is_cancelled() => return Ok(outcome),
                        Err(error) => return Err(error.to_string()),
                    };
                    outcome.context.extend(next.context);
                    outcome.diagnostics.extend(next.diagnostics);
                    if outcome.stop.is_none() {
                        outcome.stop = next.stop;
                    }
                    if outcome.block.is_none() {
                        outcome.block = next.block;
                    }
                }
                Ok::<_, String>(outcome)
            })
            .await??;
        if !outcome.diagnostics.is_empty() {
            self.append(EntryKind::Custom {
                kind: "extension.diagnostics".into(),
                data: serde_json::json!(outcome.diagnostics),
            })
            .await?;
        }
        Ok(outcome)
    }

    fn observe_session(&self, event: SessionEvent) {
        for observer in self.registry.session_observers() {
            observer.observe(&event);
        }
    }

    async fn run_inner(&mut self) -> Result<(), String> {
        self.restore()?;
        self.ensure_settings().await?;
        self.publish_thread(false);
        self.runtime(tiechui_plugin_api::RuntimeEvent::Restore { navigation: false })
            .await?;
        self.inherit_runtime().await?;
        let resumed = self.store()?.entries().iter().any(|entry| matches!(&entry.kind, EntryKind::Message { message, .. } if message.role == Role::User));
        let outcome = self
            .lifecycle(
                LifecycleEvent::SessionStart { resumed },
                self.shutdown.child_token(),
            )
            .await?;
        self.session_context.extend(outcome.context);
        if let Some(message) = outcome.stop {
            self.emit(EventKind::Error { message });
            return Ok(());
        }
        let mut resources_tick = tokio::time::interval(std::time::Duration::from_secs(1));
        while !self.closing && !self.shutdown.is_cancelled() {
            let submission = if let Some(compaction) = self.pending_compaction.take() {
                Some(compaction)
            } else if let Some(queued) = self
                .steering
                .pop_front()
                .or_else(|| self.follow_up.pop_front())
            {
                self.publish_queue();
                Some(queued)
            } else {
                // Prompts first: a withdrawal sent before an answer is applied before it.
                tokio::select! {
                    biased;
                    _ = self.shutdown.cancelled() => { self.closing = true; None }
                    message = interactions::next(&mut self.inbox) => { self.interaction(message); continue; }
                    Some(request) = self.commands_rx.recv() => { self.command_request(request).await; continue; }
                    op = self.ops.recv() => op,
                    _ = resources_tick.tick() => { self.publish_resources(); self.runtime(tiechui_plugin_api::RuntimeEvent::Tick).await?; continue; }
                    _ = tokio::time::sleep(std::time::Duration::from_millis(25)), if !self.registry.runtime_extensions().is_empty() => { self.continue_idle().await?; continue; }
                    update = self.updates.recv() => { if let Some(update) = update { self.tool_update(update); } continue; }
                }
            };
            let Some(submission) = submission else {
                break;
            };
            if !self.accepts(&submission) {
                continue;
            }
            match submission.op {
                Op::Compact { ref instructions } => {
                    let instructions = instructions.clone();
                    self.open_turn(&submission.id);
                    let succeeded = self
                        .compact(CompactionReason::Manual, instructions, false)
                        .await?;
                    self.complete(if self.cancel.is_cancelled() {
                        TurnStatus::Interrupted
                    } else if succeeded {
                        TurnStatus::Completed
                    } else {
                        TurnStatus::Failed
                    });
                    if let Some(navigation) = self.navigation.take() {
                        self.control(navigation).await?;
                    }
                }
                Op::Retry => {
                    if let Some(message) = self.retry_blocked() {
                        self.emit(EventKind::Error {
                            message: message.into(),
                        });
                    } else if self.retry_available {
                        self.run_turn(submission).await?;
                        if let Some(navigation) = self.navigation.take() {
                            self.control(navigation).await?;
                        }
                    } else {
                        self.emit(EventKind::Error {
                            message: "No failed request to retry.".into(),
                        });
                    }
                }
                Op::UserInput { .. } | Op::RunShell { .. } => {
                    if matches!(submission.op, Op::RunShell { .. }) {
                        self.shell(submission).await?;
                    } else {
                        self.run_turn(submission).await?;
                    }
                    if let Some(navigation) = self.navigation.take() {
                        self.control(navigation).await?;
                    }
                }
                _ => self.control(submission).await?,
            }
        }
        Ok(())
    }

    async fn ensure_settings(&mut self) -> Result<(), String> {
        let store = self.store()?;
        let path = store
            .path_to_root(store.current_leaf().unwrap_or(&store.header().id))
            .map_err(|error| error.to_string())?;
        let has_model = path
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::ModelChange { .. }));
        let has_thinking = path
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::ThinkingChange { .. }));
        if !has_model {
            self.append(EntryKind::ModelChange {
                model: self.model.clone(),
            })
            .await?;
        }
        if !has_thinking {
            self.append(EntryKind::ThinkingChange {
                level: self.requested,
            })
            .await?;
        }
        Ok(())
    }

    fn accepts(&self, submission: &Submission) -> bool {
        submission.thread_id == self.thread && submission.epoch == self.epoch
    }

    async fn control(&mut self, submission: Submission) -> Result<(), String> {
        let previous = self.submission.replace(submission.id);
        match submission.op {
            Op::InputState { blocked } => self.input_blocked = blocked,
            Op::InterruptIdle => {
                self.runtime(tiechui_plugin_api::RuntimeEvent::Interrupt)
                    .await?
            }
            Op::Retry => {
                if let Some(message) = self.retry_blocked() {
                    self.emit(EventKind::Error {
                        message: message.into(),
                    });
                } else if let Some(wake) = &self.retry_wake {
                    wake.cancel();
                } else {
                    self.emit(EventKind::Error {
                        message: "A request is already running.".into(),
                    });
                }
            }
            Op::SetModel { provider, model } => {
                let model = ModelRef {
                    provider,
                    model: tiechui_model::ModelId(model),
                };
                if self
                    .config
                    .providers
                    .get(&model.provider)
                    .is_some_and(|provider| provider.capabilities(&model.model).context_window > 0)
                {
                    self.append(EntryKind::ModelChange {
                        model: model.clone(),
                    })
                    .await?;
                    self.model = model;
                    self.last_request = None;
                    self.thinking = clamp(self.requested, &self.levels(&self.model));
                    self.publish_session();
                } else {
                    self.emit(EventKind::Error {
                        message: "unknown model or provider".into(),
                    });
                }
            }
            Op::SetThinking { level } => {
                let next = thinking_level(level);
                if next != self.requested {
                    self.append(EntryKind::ThinkingChange { level: next })
                        .await?;
                }
                self.requested = next;
                self.thinking = clamp(self.requested, &self.levels(&self.model));
                self.publish_session();
            }
            Op::Branch { entry_id } => {
                self.runtime(tiechui_plugin_api::RuntimeEvent::BeforeNavigate)
                    .await?;
                match self.storage(move |store| store.branch_to(&entry_id)).await {
                    Ok(()) => {
                        // Keep navigation durable even when no input or image refill follows.
                        self.append(EntryKind::Custom {
                            kind: "session.branch".into(),
                            data: serde_json::Value::Null,
                        })
                        .await?;
                        self.new_epoch(true).await?;
                    }
                    Err(message) => self.emit(EventKind::Error { message }),
                }
            }
            Op::Reload => {
                self.storage(|store| store.reload(&CancellationToken::new()))
                    .await?;
                self.new_epoch(false).await?;
            }
            Op::ClearDraft => self.save_draft(vec![]).await?,
            Op::Retract { submission_ids } => self.retract(&submission_ids),
            Op::RunCommand { name, arguments } => self.command(name, arguments).await,
            Op::CommandInput { name, data } => {
                if let Some(command) = self
                    .registry
                    .commands()
                    .iter()
                    .find(|c| c.info().name == name && c.background())
                    .cloned()
                    && let Err(error) = self
                        .background_command(command, String::new(), Some(data))
                        .await
                {
                    self.emit(EventKind::Error { message: error.0 });
                }
            }
            Op::RunShell { .. } => self.emit(EventKind::Error {
                message: "shell commands run between turns".into(),
            }),
            Op::ApprovalResponse {
                approval_id,
                decision,
            } => {
                let shown = self.shown();
                self.prompts.answer(&approval_id, decision);
                self.publish_prompts(shown);
            }
            Op::Interrupt { .. } | Op::UserInput { .. } | Op::Compact { .. } => {}
        }
        self.submission = previous;
        Ok(())
    }

    fn publish_queue(&self) {
        let queued = |queue: &VecDeque<Submission>| {
            queue
                .iter()
                .filter_map(|submission| match &submission.op {
                    Op::UserInput { content, .. } => Some(QueuedInput {
                        submission_id: submission.id.clone(),
                        content: content.clone(),
                    }),
                    _ => None,
                })
                .collect()
        };
        self.emit(EventKind::QueueUpdated {
            steering: queued(&self.steering),
            follow_up: queued(&self.follow_up),
        });
    }

    fn retract(&mut self, ids: &[SubmissionId]) {
        let mut inputs = Vec::new();
        for queue in [&mut self.steering, &mut self.follow_up] {
            queue.retain(|submission| {
                let Op::UserInput { content, .. } = &submission.op else {
                    return true;
                };
                if !ids.contains(&submission.id) {
                    return true;
                }
                inputs.push(QueuedInput {
                    submission_id: submission.id.clone(),
                    content: content.clone(),
                });
                false
            });
        }
        self.emit(EventKind::InputRetracted { inputs });
        self.publish_queue();
    }

    async fn new_epoch(&mut self, replay: bool) -> Result<(), String> {
        self.quiesce().await;
        self.command_cancel.cancel();
        self.command_cancel = self.shutdown.child_token();
        self.last_request = None;
        self.system_fragments.clear();
        let shown = self.shown();
        self.prompts.clear();
        self.publish_prompts(shown);
        self.epoch.0 = self
            .epoch
            .0
            .checked_add(1)
            .ok_or("session epoch exhausted")?;
        self.steering.clear();
        self.follow_up.clear();
        self.restore()?;
        self.ensure_settings().await?;
        self.publish_thread(replay);
        self.runtime(tiechui_plugin_api::RuntimeEvent::Restore { navigation: replay })
            .await?;
        Ok(())
    }

    async fn busy(&mut self, submission: Submission) -> Result<(), String> {
        if !self.accepts(&submission) {
            return Ok(());
        }
        if matches!(submission.op, Op::UserInput { .. }) && self.retry_wake.is_some() {
            self.cancel.cancel();
        }
        match &submission.op {
            Op::Compact { .. } => {
                if self.compacting {
                    self.emit(EventKind::Error {
                        message: "Compaction is already in progress".into(),
                    });
                } else {
                    self.pending_compaction = Some(submission);
                    self.cancel.cancel();
                }
            }
            Op::InterruptIdle if self.compacting => self.cancel.cancel(),
            Op::UserInput {
                delivery: InputDelivery::Steer,
                ..
            } => {
                self.steering.push_back(submission);
                self.publish_queue();
            }
            Op::UserInput {
                delivery: InputDelivery::FollowUp,
                ..
            } => {
                self.follow_up.push_back(submission);
                self.publish_queue();
            }
            Op::Interrupt { turn_id } => {
                if self.turn.as_ref().is_some_and(|turn| &turn.id == turn_id) {
                    self.cancel.cancel();
                    let shown = self.shown();
                    self.prompts.clear();
                    self.publish_prompts(shown);
                }
            }
            Op::Branch { entry_id } => {
                if let Err(error) = self.store()?.path_to_root(entry_id) {
                    self.emit(EventKind::Error {
                        message: error.to_string(),
                    });
                } else {
                    self.navigation = Some(submission);
                    self.cancel.cancel();
                }
            }
            Op::Reload => {
                self.navigation = Some(submission);
                self.cancel.cancel();
            }
            _ => self.control(submission).await?,
        }
        Ok(())
    }

    async fn drive<F: Future>(&mut self, work: F) -> Result<F::Output, String> {
        tokio::pin!(work);
        let mut failure = None;
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
            std::time::Duration::from_secs(1),
        );
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled(), if !self.closing => { self.closing = true; self.cancel.cancel(); }
                message = interactions::next(&mut self.inbox) => self.interaction(message),
                op = self.ops.recv(), if !self.closing => {
                    if let Some(op) = op {
                        if let Err(error) = self.busy(op).await { self.cancel.cancel(); failure.get_or_insert(error); }
                    }
                    else { self.closing = true; self.cancel.cancel(); }
                }
                update = self.updates.recv() => { if let Some(update) = update { self.tool_update(update); } }
                Some(request) = self.commands_rx.recv() => self.command_request(request).await,
                result = &mut work => return match failure { Some(error) => Err(error), None => Ok(result) },
                _ = ticker.tick() => {
                    self.publish_resources();
                    if let Err(error) = self.runtime(tiechui_plugin_api::RuntimeEvent::Tick).await { self.cancel.cancel(); failure.get_or_insert(error); }
                },
            }
        }
    }

    fn interaction(&mut self, message: Option<interactions::Message>) {
        match message {
            Some(message) => {
                let shown = self.shown();
                self.prompts.apply(message);
                self.publish_prompts(shown);
            }
            None => self.inbox = None,
        }
    }

    fn shown(&self) -> Option<ApprovalId> {
        self.prompts.head().map(|prompt| prompt.id.clone())
    }

    /// Tells the client when the head of the queue changed from `shown`.
    fn publish_prompts(&self, shown: Option<ApprovalId>) {
        let head = self.prompts.head();
        if head.map(|prompt| &prompt.id) == shown.as_ref() {
            return;
        }
        if let Some(approval_id) = shown {
            self.emit(EventKind::ApprovalResolved { approval_id });
        }
        if let Some(prompt) = head {
            self.emit(EventKind::ApprovalRequested {
                turn_id: self.turn.as_ref().map(|turn| turn.id.clone()),
                approval_id: prompt.id.clone(),
                request: prompt.request.clone(),
            });
        }
    }

    async fn drain_submissions(&mut self) -> Result<(), String> {
        while let Ok(submission) = self.ops.try_recv() {
            self.busy(submission).await?;
        }
        if self.shutdown.is_cancelled() {
            self.closing = true;
            self.cancel.cancel();
        }
        Ok(())
    }

    fn tool_update(&self, update: ToolUpdate) {
        if update.epoch == self.epoch
            && let Some(turn) = &self.turn
            && let Some(id) = self.tool_items.get(&update.call_id)
        {
            self.emit(EventKind::ItemDelta {
                turn_id: turn.id.clone(),
                item_id: id.clone(),
                delta: serde_json::to_string(&update.output).unwrap_or_default(),
            });
        }
    }

    fn item(&mut self, item: KnownItem) {
        if let Some(turn) = &self.turn {
            self.emit(EventKind::ItemStarted {
                turn_id: turn.id.clone(),
                item: Item::Known(item.clone()),
            });
        }
        self.completed_item(item);
    }

    fn completed_item(&mut self, item: KnownItem) {
        if let Some(turn) = &mut self.turn {
            let turn_id = turn.id.clone();
            let item = Item::Known(item);
            turn.items.push(item.clone());
            self.emit(EventKind::ItemCompleted { turn_id, item });
        }
    }

    async fn user(&mut self, submission: Submission) -> Result<bool, String> {
        if let Op::UserInput { content, .. } = submission.op {
            let outcome = if self.automatic_input.is_some() {
                LifecycleOutcome::default()
            } else {
                self.lifecycle(
                    LifecycleEvent::UserInput {
                        content: content.clone(),
                    },
                    self.cancel.clone(),
                )
                .await?
            };
            if let Some(message) = outcome.stop.or(outcome.block) {
                self.emit(EventKind::Error { message });
                return Ok(false);
            }
            self.turn_context.extend(outcome.context);
            let input = TurnInput {
                cwd: PathBuf::from(&self.store()?.header().cwd),
                content: content.clone(),
            };
            let registry = self.registry.clone();
            let cancel = self.cancel.clone();
            let system = self.drive(async move {
                let mut fragments = Vec::new();
                let mut descriptions = Vec::new();
                for provider in registry.context_providers() {
                    let contribution = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err("Operation aborted".to_owned()),
                        result = provider.fragments(&input, cancel.clone()) => result.map_err(|e| e.to_string())?,
                    };
                    descriptions.extend(provider.describe(&contribution));
                    fragments.extend(contribution);
                }
                Ok((assemble_system_prompt(&fragments), descriptions))
            }).await?;
            if self.cancel.is_cancelled() {
                return Ok(false);
            }
            (self.system, self.system_fragments) = system?;
            if let Some(input) = self.automatic_input.clone() {
                let id = self.append(EntryKind::CustomMessage { kind: input.kind, text: input.text, hidden: !input.visible,
                    details: serde_json::json!({"sessionId": self.thread.0, "identity": input.identity, "generation": input.generation}),
                }).await?;
                if input.visible {
                    self.item(KnownItem::UserMessage {
                        id: ItemId(id),
                        content,
                    });
                }
                return Ok(true);
            }
            let message = Message {
                role: Role::User,
                content: content
                    .iter()
                    .map(|part| match part {
                        InputContent::Text { text } => text_content(text.clone()),
                        InputContent::Image { mime_type, data } => Content::Image {
                            mime_type: mime_type.clone(),
                            data: data.clone(),
                        },
                    })
                    .collect(),
                opaque: None,
            };
            let id = self
                .append(EntryKind::Message {
                    reasoning: None,
                    usage: None,
                    stop_reason: None,
                    details: None,
                    message,
                    model: None,
                })
                .await?;
            if self.saved_draft().is_some_and(|draft| !draft.is_empty()) {
                self.save_draft(vec![]).await?;
            }
            self.item(KnownItem::UserMessage {
                id: ItemId(id),
                content,
            });
        }
        Ok(true)
    }

    async fn run_turn_inner(&mut self, submission: Submission) -> Result<(), String> {
        let mut manual_retry = matches!(submission.op, Op::Retry);
        let context = std::mem::take(&mut self.turn_context);
        self.open_turn(&submission.id);
        if manual_retry {
            self.turn_context = context;
        }
        if !manual_retry {
            self.retry_available = false;
            self.emit(EventKind::RetryAvailable { available: false });
        }
        if !manual_retry && !self.user(submission).await? {
            self.complete(if self.cancel.is_cancelled() {
                TurnStatus::Interrupted
            } else {
                TurnStatus::Completed
            });
            return Ok(());
        }
        self.auto_compact().await?;
        let mut continuation_active = false;
        let mut overflow_retried = false;
        loop {
            self.drain_submissions().await?;
            if self.cancel.is_cancelled() {
                self.complete(TurnStatus::Interrupted);
                return Ok(());
            }
            let mut retries = 0;
            let mut ambiguous_retried = false;
            let (message, reason, origin, usage, reasoning) = loop {
                let context_len = self.turn_context.len();
                let outcome = self
                    .lifecycle(LifecycleEvent::BeforeRequest, self.cancel.clone())
                    .await?;
                self.turn_context.extend(outcome.context);
                if outcome.stop.is_some() {
                    self.complete(TurnStatus::Completed);
                    return Ok(());
                }
                if self.cancel.is_cancelled() {
                    self.complete(TurnStatus::Interrupted);
                    return Ok(());
                }
                let (message, mut outcome, origin, usage, reasoning) = self.request().await?;
                if self.config.compaction.enabled
                    && origin == self.model
                    && matches!(outcome, Ok(StopReason::Length))
                    && self
                        .config
                        .providers
                        .get(&origin.provider)
                        .is_some_and(|provider| {
                            usage.output < provider.capabilities(&origin.model).max_output
                        })
                {
                    outcome = Err(provider_error(
                        ErrorKind::ContextOverflow,
                        "Truncated response",
                    ));
                }
                self.runtime(tiechui_plugin_api::RuntimeEvent::Assistant {
                    id: format!(
                        "{}-{}-request-{}",
                        self.thread.0, self.epoch.0, self.request_sequence
                    ),
                    usage: Some(usage),
                    tools: outcome.is_ok() && !tool_calls(&message).is_empty(),
                })
                .await?;
                // Request-time context is refreshed on recovery, not accumulated.
                self.turn_context.truncate(context_len);
                match outcome {
                    Ok(reason) => {
                        if usage.cache_read > 0 || usage.cache_write > 0 {
                            self.cache_touch = Some((origin.clone(), std::time::Instant::now()));
                        }
                        self.retry_available = false;
                        self.emit(EventKind::RetryAvailable { available: false });
                        manual_retry = false;
                        break (message, reason, origin, usage, reasoning);
                    }
                    Err(error) => {
                        let same_model = origin == self.model;
                        self.audit_partial(
                            message,
                            origin,
                            usage,
                            reasoning,
                            error.kind == ErrorKind::Cancelled || self.cancel.is_cancelled(),
                        )
                        .await?;
                        if error.kind == ErrorKind::ContextOverflow
                            && same_model
                            && overflow_retried
                        {
                            self.emit(EventKind::CompactionEnded {
                                reason: CompactionReason::Overflow,
                                aborted: false,
                                error: Some(if error.message == "Truncated response" {
                                    "Truncated response recovery failed after one compact-and-retry attempt."
                                } else {
                                    "Context overflow recovery failed after one compact-and-retry attempt. Try reducing context or switching to a larger-context model."
                                }.into()),
                                will_retry: false,
                                paused: None,
                            });
                        }
                        if error.kind == ErrorKind::ContextOverflow
                            && same_model
                            && !overflow_retried
                            && self.config.compaction.enabled
                            && !self.cancel.is_cancelled()
                        {
                            overflow_retried = true;
                            if self.compact(CompactionReason::Overflow, None, true).await? {
                                retries = 0;
                                ambiguous_retried = false;
                                continue;
                            }
                        }
                        if !self.cancel.is_cancelled() && error.kind != ErrorKind::Cancelled {
                            self.retry_available = true;
                        }
                        if self.retry_available {
                            self.append(EntryKind::Custom {
                                kind: "request_recovery".into(),
                                data: serde_json::json!({
                                    "system": self.system,
                                    "context": self.turn_context,
                                    "message": error.to_string(),
                                }),
                            })
                            .await?;
                            self.emit(EventKind::RetryAvailable { available: true });
                        }
                        if self.cancel.is_cancelled() || error.kind == ErrorKind::Cancelled {
                            self.complete(TurnStatus::Interrupted);
                            return Ok(());
                        }
                        // This guard remains here even if a replacement policy is over-permissive.
                        let delay = if !(manual_retry
                            || error.kind == ErrorKind::AmbiguousRequest && ambiguous_retried)
                            && matches!(
                                error.kind,
                                ErrorKind::RateLimit
                                    | ErrorKind::Overloaded
                                    | ErrorKind::TransientNetwork
                                    | ErrorKind::AmbiguousRequest
                            ) {
                            self.config.retry.delay(&error, retries)
                        } else {
                            None
                        };
                        if let Some(delay) = delay {
                            ambiguous_retried |= error.kind == ErrorKind::AmbiguousRequest;
                            retries += 1;
                            self.retry_event(retries, Some(delay), &error.message);
                            let cancel = self.cancel.clone();
                            let wake = CancellationToken::new();
                            self.retry_wake = Some(wake.clone());
                            self.drive(async move { tokio::select! { biased; _ = cancel.cancelled() => {}, _ = wake.cancelled() => {}, _ = tokio::time::sleep(delay) => {} } }).await?;
                            self.retry_wake = None;
                            if self.cancel.is_cancelled() {
                                self.complete(TurnStatus::Interrupted);
                                return Ok(());
                            }
                            self.retry_event(retries, None, &error.message);
                        } else {
                            if error.kind != ErrorKind::ContextOverflow {
                                self.auto_compact().await?;
                            }
                            self.runtime(tiechui_plugin_api::RuntimeEvent::Failure(
                                error.to_string(),
                            ))
                            .await?;
                            self.emit(EventKind::Error {
                                message: error.to_string(),
                            });
                            self.complete(TurnStatus::Failed);
                            return Ok(());
                        }
                    }
                }
            };
            let calls = tool_calls(&message);
            let has_tools = !calls.is_empty();
            let last_message = message
                .content
                .iter()
                .filter_map(|part| match part {
                    Content::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            self.persist_assistant(message, origin, reason, usage, reasoning)
                .await?;
            self.drain_submissions().await?;
            if !calls.is_empty() {
                self.execute(calls, reason).await?;
            }
            self.drain_submissions().await?;
            if self.cancel.is_cancelled() {
                self.complete(TurnStatus::Interrupted);
                return Ok(());
            }
            self.auto_compact().await?;
            if self.cancel.is_cancelled() {
                self.complete(TurnStatus::Interrupted);
                return Ok(());
            }
            let steering = self.steering.pop_front();
            let has_steering = steering.is_some();
            if let Some(steering) = steering {
                self.publish_queue();
                if !self.user(steering).await? {
                    self.complete(if self.cancel.is_cancelled() {
                        TurnStatus::Interrupted
                    } else {
                        TurnStatus::Completed
                    });
                    return Ok(());
                }
                continuation_active = false;
            }
            if !has_steering && !has_tools {
                let outcome = self
                    .lifecycle(
                        LifecycleEvent::BeforeStop {
                            active: continuation_active,
                            last_message: Some(last_message),
                        },
                        self.cancel.clone(),
                    )
                    .await?;
                self.turn_context.extend(outcome.context);
                if self.cancel.is_cancelled() {
                    self.complete(TurnStatus::Interrupted);
                    return Ok(());
                }
                if outcome.stop.is_none()
                    && let Some(reason) = outcome.block
                {
                    self.append(EntryKind::Message {
                        reasoning: None,
                        usage: None,
                        stop_reason: None,
                        details: None,
                        message: Message {
                            role: Role::User,
                            content: vec![text_content(reason)],
                            opaque: None,
                        },
                        model: None,
                    })
                    .await?;
                    continuation_active = true;
                    continue;
                }
                self.complete(TurnStatus::Completed);
                return Ok(());
            }
        }
    }

    fn retry_event(&self, attempt: u32, delay: Option<std::time::Duration>, message: &str) {
        if let Some(turn) = &self.turn {
            self.emit(EventKind::RequestRetry {
                turn_id: turn.id.clone(),
                attempt,
                max_retries: self.config.retry.max_retries(),
                delay_ms: delay.map(|delay| u64::try_from(delay.as_millis()).unwrap_or(u64::MAX)),
                message: message.into(),
            });
        }
    }

    fn retry_blocked(&self) -> Option<&'static str> {
        if self.prompts.head().is_some() {
            Some("Finish the current interaction before retrying.")
        } else if !self.steering.is_empty() || !self.follow_up.is_empty() {
            Some("Finish queued messages before retrying.")
        } else {
            None
        }
    }

    async fn request(
        &mut self,
    ) -> Result<
        (
            Message,
            Result<StopReason, ProviderError>,
            ModelRef,
            Usage,
            Option<ReasoningLevel>,
        ),
        String,
    > {
        let mut origin = self.model.clone();
        let reasoning = self.thinking;
        self.request_sequence += 1;
        self.stream_items.clear();
        self.stream_order.clear();
        let mut response = stream::Response::default();
        let Some(provider) = self.config.providers.get(&self.model.provider).cloned() else {
            return Ok((
                response.message(),
                Err(provider_error(
                    ErrorKind::InvalidRequest,
                    "unknown provider",
                )),
                origin,
                Usage {
                    partial: true,
                    ..Usage::default()
                },
                reasoning,
            ));
        };
        let tools = self.visible_tools();
        let mut history = self.store()?.build_context_with_origins(&self.excluded);
        let mut projected = false;
        for projection in self.registry.clone().projections() {
            let branch = self.branch_view()?;
            let raw = self
                .store()?
                .path_to_root(&branch.leaf)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter_map(|entry| match &entry.kind {
                    EntryKind::Message { message, .. } => Some(message.clone()),
                    _ => None,
                })
                .collect();
            let tool_details = self
                .store()?
                .path_to_root(&branch.leaf)
                .map_err(|e| e.to_string())?
                .into_iter()
                .flat_map(|entry| match &entry.kind {
                    EntryKind::Message {
                        message,
                        details: Some(details),
                        ..
                    } => message
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            Content::ToolResult(result) => {
                                Some((result.call_id.clone(), details.clone()))
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                    _ => vec![],
                })
                .collect();
            let context = tiechui_plugin_api::ProjectionContext {
                tool_details,
                estimate_message: |message| tiechui_providers::estimate::messages([message]),
                estimate_tools: |tools| tiechui_providers::estimate::input(&[], tools),
                branch,
                history: raw,
                messages: history
                    .iter()
                    .map(|(message, _, _)| message.clone())
                    .collect(),
                tools: tools.clone(),
                system: assemble_system_prompt(
                    &self
                        .session_context
                        .iter()
                        .chain(&self.turn_context)
                        .cloned()
                        .chain(std::iter::once(self.system.clone()))
                        .collect::<Vec<_>>(),
                ),
                window: provider.capabilities(&self.model.model).context_window,
                cache_cold: self.cache_touch.as_ref().is_some_and(|(model, at)| {
                    model == &self.model
                        && provider
                            .prompt_cache_ttl(&self.model.model)
                            .is_some_and(|ttl| at.elapsed() > ttl)
                }),
            };
            let projection = projection.clone();
            let cancel = self.cancel.clone();
            match self
                .drive(async move { projection.project(context, cancel).await })
                .await?
            {
                Ok(Some(view)) => {
                    let mut persisted = Ok(());
                    for (kind, data) in view.entries {
                        persisted = self
                            .append(EntryKind::Custom {
                                kind: kind.clone(),
                                data: data.clone(),
                            })
                            .await
                            .map(drop);
                        if persisted.is_err() {
                            break;
                        }
                        self.runtime(tiechui_plugin_api::RuntimeEvent::Extension {
                            name: kind,
                            data,
                        })
                        .await?;
                    }
                    if let Err(error) = persisted {
                        self.emit(EventKind::Extension {
                            name: "module.notice".into(),
                            data: serde_json::json!(format!(
                                "Request projection skipped: {error}. Original history remains visible."
                            )),
                        });
                        continue;
                    }
                    let next: Vec<_> = view
                        .messages
                        .into_iter()
                        .map(|item| {
                            let origin = item.source.and_then(|index| history.get(index));
                            (
                                item.message,
                                origin.and_then(|(_, model, _)| model.clone()),
                                origin.and_then(|(_, _, reason)| *reason),
                            )
                        })
                        .collect();
                    projected |= next != history;
                    history = next;
                }
                Ok(None) => {}
                Err(error) => self.emit(EventKind::Extension {
                    name: "module.notice".into(),
                    data: serde_json::json!(format!(
                        "Request projection skipped: {error}. Original history remains visible."
                    )),
                }),
            }
        }
        let mut messages = replay::prepare(
            history,
            &self.model,
            provider.capabilities(&self.model.model).images,
            provider.error_tool_images(),
            |id| provider.normalize_tool_call_id(id),
        );
        let system = self.system_message();
        let assembled_system = system.is_some();
        messages.splice(0..0, system);
        for extension in self.registry.runtime_extensions() {
            messages.extend(extension.messages());
        }
        let input_tokens = if projected {
            tiechui_providers::estimate::input(&messages, &tools)
        } else {
            estimate::input(self.store()?, &self.excluded, &messages, &tools)?
        };
        self.request_prefix = inspection::prefix(&messages, &tools);
        self.last_request = Some(self.observe_request(
            &messages[usize::from(assembled_system)..],
            assembled_system.then(|| &messages[0]),
            &tools,
        )?);
        let request = ModelRequest {
            disable_cache: false,
            short_cache: self.config.short_cache,
            cache_key: Some(self.thread.0.clone()),
            input_tokens: Some(input_tokens),
            model: self.model.model.clone(),
            messages,
            tools,
            max_output: None,
            reasoning,
        };
        let cancel = self.cancel.clone();
        let stream = self.drive(async move { tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(provider_error(ErrorKind::Cancelled, "request cancelled")),
            stream = provider.stream(request, cancel.clone()) => stream,
        } }).await?;
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                return Ok((
                    response.message(),
                    Err(error),
                    origin,
                    Usage {
                        partial: true,
                        ..Usage::default()
                    },
                    reasoning,
                ));
            }
        };
        let mut accounted = None;
        let mut reported = None;
        let result = loop {
            let cancel = self.cancel.clone();
            let next = self.drive(async { tokio::select! {
                biased;
                _ = cancel.cancelled() => Some(Err(provider_error(ErrorKind::Cancelled, "request cancelled"))),
                event = stream.next() => event,
            } }).await?;
            match next {
                Some(Ok(StreamEvent::Finish { reason, usage })) => {
                    reported = Some(usage);
                    self.record_usage(usage, &origin, &mut accounted).await?;
                    break match reason {
                        StopReason::Cancelled => {
                            Err(provider_error(ErrorKind::Cancelled, "request cancelled"))
                        }
                        StopReason::Error => Err(provider_error(
                            ErrorKind::InvalidRequest,
                            "provider ended with an error",
                        )),
                        _ => Ok(reason),
                    };
                }
                Some(Ok(StreamEvent::ResponseModel { model })) => {
                    origin.model = model;
                }
                Some(Ok(StreamEvent::Usage { usage })) => {
                    reported = Some(usage);
                    self.record_usage(usage, &origin, &mut accounted).await?;
                }
                Some(Ok(event)) => {
                    self.stream_delta(&event);
                    response.apply(event);
                    self.stream_order = response.indices();
                }
                Some(Err(error)) => break Err(error),
                None => {
                    break Err(provider_error(
                        ErrorKind::TransientNetwork,
                        "stream ended without finish",
                    ));
                }
            }
        };
        if let Some((_, usage)) = &accounted
            && let Some(turn) = &self.turn
        {
            self.emit(EventKind::TokenUsageUpdated {
                turn_id: turn.id.clone(),
                last: tokens(*usage),
                total: tokens(self.total),
            });
        }
        Ok((
            response.message(),
            result,
            origin,
            accounted
                .map(|(_, usage)| usage)
                .or(reported)
                .unwrap_or(Usage {
                    partial: true,
                    ..Usage::default()
                }),
            reasoning,
        ))
    }

    async fn record_usage(
        &mut self,
        mut usage: Usage,
        origin: &ModelRef,
        accounted: &mut Option<(String, Usage)>,
    ) -> Result<(), String> {
        if usage
            == (Usage {
                partial: usage.partial,
                ..Usage::default()
            })
        {
            return Ok(());
        }
        usage.cost_nano_usd = self
            .config
            .providers
            .get(&origin.provider)
            .and_then(|provider| provider.cost_nano_usd(&origin.model, usage));
        if accounted
            .as_ref()
            .is_some_and(|(_, previous)| *previous == usage)
        {
            return Ok(());
        }
        let id = self
            .append(EntryKind::Usage {
                usage,
                model: Some(origin.clone()),
                request_id: accounted.as_ref().map(|(id, _)| id.clone()),
            })
            .await?;
        if accounted.is_none() {
            self.usage_prefixes
                .push_back((id.clone(), self.request_prefix));
            if self.usage_prefixes.len() > 129 {
                self.usage_prefixes.pop_front();
            }
        }
        let (id, previous) = accounted.take().unwrap_or((id, Usage::default()));
        replace_usage(&mut self.total, previous, usage);
        *accounted = Some((id, usage));
        self.context = Some(usage);
        Ok(())
    }

    fn stream_delta(&mut self, event: &StreamEvent) {
        let Some(turn) = &self.turn else {
            return;
        };
        let (index, delta) = match event {
            StreamEvent::TextDelta { index, delta }
            | StreamEvent::ThinkingDelta { index, delta }
            | StreamEvent::ToolCallArgumentsDelta { index, delta } => (*index, Some(delta.clone())),
            StreamEvent::ToolCallStart { index, .. }
            | StreamEvent::ContentComplete { index, .. } => (*index, None),
            StreamEvent::ResponseModel { .. }
            | StreamEvent::Finish { .. }
            | StreamEvent::Usage { .. } => return,
        };
        let turn_id = turn.id.clone();
        if !self.stream_items.contains_key(&index) {
            let id = ItemId(format!("{}-{}-{index}", turn_id.0, self.request_sequence));
            let item = match event {
                StreamEvent::TextDelta { .. } => KnownItem::AssistantMessage {
                    id,
                    text: String::new(),
                },
                StreamEvent::ThinkingDelta { .. } => KnownItem::Thinking {
                    id,
                    text: String::new(),
                },
                StreamEvent::ToolCallStart { id, name, .. } => KnownItem::ToolCall {
                    id: ItemId(id.clone()),
                    name: name.clone(),
                    arguments: serde_json::json!({}),
                },
                StreamEvent::ContentComplete { content, .. } => {
                    match content_item(content.clone(), id) {
                        Some(item) => item,
                        None => return,
                    }
                }
                _ => return,
            };
            self.stream_items.insert(index, item_id(&item));
            self.emit(EventKind::ItemStarted {
                turn_id: turn_id.clone(),
                item: Item::Known(item),
            });
        }
        if let Some(delta) = delta {
            self.emit(EventKind::ItemDelta {
                turn_id,
                item_id: self.stream_items[&index].clone(),
                delta,
            });
        }
    }

    async fn persist_assistant(
        &mut self,
        message: Message,
        model: ModelRef,
        reason: StopReason,
        usage: Usage,
        reasoning: Option<ReasoningLevel>,
    ) -> Result<String, String> {
        let id = self
            .append(EntryKind::Message {
                usage: Some(usage),
                stop_reason: Some(reason),
                reasoning,
                details: None,
                message: message.clone(),
                model: Some(model),
            })
            .await?;
        for (index, content) in message.content.into_iter().enumerate() {
            let streamed = self
                .stream_order
                .get(index)
                .and_then(|index| self.stream_items.get(index));
            let block_id = streamed
                .cloned()
                .unwrap_or_else(|| ItemId(format!("{id}-{index}")));
            if let Some(item) = content_item(content, block_id) {
                if streamed.is_some() {
                    self.completed_item(item);
                } else {
                    self.item(item);
                }
            }
        }
        Ok(id)
    }

    async fn audit_partial(
        &mut self,
        message: Message,
        origin: ModelRef,
        usage: Usage,
        reasoning: Option<ReasoningLevel>,
        aborted: bool,
    ) -> Result<(), String> {
        if message.content.is_empty() {
            return Ok(());
        }
        let calls = tool_calls(&message);
        let mut ids = vec![
            self.persist_assistant(
                message,
                origin,
                if aborted {
                    StopReason::Cancelled
                } else {
                    StopReason::Error
                },
                usage,
                reasoning,
            )
            .await?,
        ];
        for call in calls {
            ids.push(
                self.tool_result(
                    &call,
                    error_output(if aborted {
                        "Operation aborted"
                    } else {
                        "No result provided"
                    }),
                )
                .await?,
            );
        }
        self.append(EntryKind::Custom {
            kind: "request_omission".into(),
            data: serde_json::json!(ids),
        })
        .await?;
        self.excluded.extend(ids);
        Ok(())
    }

    async fn execute(&mut self, calls: Vec<ToolCall>, reason: StopReason) -> Result<(), String> {
        let mut ready = Vec::new();
        let mut immediate = Vec::new();
        for call in &calls {
            let arguments = if reason == StopReason::Length {
                Err(format!(
                    "Tool call \"{}\" was not executed: the response hit the output token limit, so its arguments may be truncated. Re-issue the tool call with complete arguments.",
                    call.name
                ))
            } else if self.cancel.is_cancelled() {
                Err("Operation aborted".into())
            } else {
                Ok(call.arguments.clone())
            };
            match arguments {
                Ok(arguments) => {
                    let mut prepared = call.clone();
                    prepared.arguments = arguments;
                    ready.push(prepared);
                    immediate.push(None);
                }
                Err(error) => immediate.push(Some(error_output(error))),
            }
        }
        let (ctx, mut entries) = self.tool_context()?;
        let turn_id = ctx.scope.turn_id.clone();
        for call in &calls {
            let id = ItemId(format!(
                "{}-{}-result-{}",
                turn_id.0, self.request_sequence, call.id
            ));
            self.tool_items.insert(call.id.clone(), id.clone());
            self.emit(EventKind::ItemStarted {
                turn_id: turn_id.clone(),
                item: Item::Known(KnownItem::ToolResult {
                    id,
                    call_id: ItemId(call.id.clone()),
                    output: serde_json::to_value(ToolOutput::default())
                        .map_err(|error| error.to_string())?,
                    is_error: false,
                }),
            });
        }
        let registry = self.registry.clone();
        let scheduler = self.scheduler.clone();
        let mut results = self
            .drive(async move {
                futures::future::join_all(ready.into_iter().map(|call| {
                    interception::execute_call(&registry, &scheduler, call, ctx.clone())
                }))
                .await
            })
            .await?
            .into_iter();
        while let Ok(update) = self.updates.try_recv() {
            self.tool_update(update);
        }
        while let Ok(entry) = entries.try_recv() {
            self.append(EntryKind::Custom {
                kind: entry.kind.clone(),
                data: entry.data.clone(),
            })
            .await?;
            self.runtime(tiechui_plugin_api::RuntimeEvent::Extension {
                name: entry.kind,
                data: entry.data,
            })
            .await?;
        }
        for (call, immediate) in calls.iter().zip(immediate) {
            let output = match immediate {
                Some(output) => output,
                None => results.next().ok_or("missing tool result")?,
            };
            self.tool_result(call, output).await?;
        }
        Ok(())
    }

    /// The next request's system message, absent when every fragment is empty.
    fn system_message(&self) -> Option<Message> {
        let system = assemble_system_prompt(
            &self
                .session_context
                .iter()
                .chain(&self.turn_context)
                .cloned()
                .chain(std::iter::once(self.system.clone()))
                .collect::<Vec<_>>(),
        );
        (!system.is_empty()).then(|| Message {
            role: Role::System,
            content: vec![text_content(system)],
            opaque: None,
        })
    }

    fn visible_tools(&self) -> Vec<tiechui_model::ToolSpec> {
        match self.registry.presentation() {
            Some(presentation) => presentation.present(tiechui_plugin_api::PresentationContext {
                tools: self.registry.tools().callable(),
                window: self.model_info(&self.model).context_window,
                estimate_tools: |tools| tiechui_providers::estimate::input(&[], tools),
            }),
            None => self.registry.tools().specs(),
        }
    }

    /// The active turn's tool context and the receiver of entries its tools append.
    fn tool_context(
        &self,
    ) -> Result<
        (
            ToolContext,
            mpsc::UnboundedReceiver<tiechui_tools::CustomSessionEntry>,
        ),
        String,
    > {
        let Some(turn) = &self.turn else {
            return Err("missing active turn".into());
        };
        let updates = self.updates_tx.clone();
        let (entries_tx, entries) = mpsc::unbounded_channel::<tiechui_tools::CustomSessionEntry>();
        let ctx = ToolContext {
            invoker: Some(Arc::new(interception::Invoker {
                registry: self.registry.clone(),
                scheduler: self.scheduler.clone(),
                model: self
                    .config
                    .providers
                    .get(&self.model.provider)
                    .map(|provider| (self.model.model.clone(), provider.clone())),
                providers: self.config.providers.clone(),
            })),
            branch: Arc::new(self.branch_entries()?),
            parent_session: self.store()?.header().parent_session.clone(),
            cwd: PathBuf::from(&self.store()?.header().cwd),
            cancel: self.cancel.clone(),
            updates: Arc::new(move |update| {
                let _ = updates.send(update);
            }),
            append_entry: Arc::new(move |entry| {
                entries_tx
                    .send(entry)
                    .map_err(|_| tiechui_tools::ToolError("Session entry channel closed".into()))
            }),
            epoch: self.epoch,
            scope: ExecutionScope {
                thread_id: self.thread.clone(),
                turn_id: turn.id.clone(),
            },
            origin: Default::default(),
        };
        Ok((ctx, entries))
    }

    fn open_turn(&mut self, submission: &SubmissionId) {
        self.sequence += 1;
        self.submission = Some(submission.clone());
        self.cancel = CancellationToken::new();
        self.turn_context.clear();
        let turn = Turn {
            id: TurnId(format!(
                "{}-{}-{}",
                self.thread.0, self.epoch.0, self.sequence
            )),
            status: TurnStatus::InProgress,
            items: vec![],
            started_at: tiechui_session::now_ms(),
            ended_at: None,
        };
        self.emit(EventKind::TurnStarted { turn: turn.clone() });
        self.turn = Some(turn);
    }

    /// User bash: the registered `bash` tool runs the command outside the permission
    /// path, shown as a tool call, and the transcript enters the context as a user message.
    async fn shell(&mut self, submission: Submission) -> Result<(), String> {
        self.runtime(tiechui_plugin_api::RuntimeEvent::Shell)
            .await?;
        self.retry_available = false;
        self.emit(EventKind::RetryAvailable { available: false });
        let Op::RunShell { command } = submission.op else {
            return Ok(());
        };
        let Some(tool) = self.registry.tools().get("bash") else {
            self.emit(EventKind::Error {
                message: "the bash tool is not available".into(),
            });
            return Ok(());
        };
        self.open_turn(&submission.id);
        let (mut ctx, _) = self.tool_context()?;
        ctx.origin = tiechui_tools::ToolOrigin::User;
        let call = ToolCall {
            id: format!("{}-shell", ctx.scope.turn_id.0),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": command }),
            opaque: None,
        };
        self.item(KnownItem::ToolCall {
            id: ItemId(call.id.clone()),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        });
        let result_id = ItemId(format!("{}-result", call.id));
        self.tool_items.insert(call.id.clone(), result_id.clone());
        self.emit(EventKind::ItemStarted {
            turn_id: ctx.scope.turn_id.clone(),
            item: Item::Known(KnownItem::ToolResult {
                id: result_id.clone(),
                call_id: ItemId(call.id.clone()),
                output: serde_json::to_value(ToolOutput::default())
                    .map_err(|error| error.to_string())?,
                is_error: false,
            }),
        });
        let running = call.clone();
        let output = self
            .drive(async move { tool.run(running, ctx).await })
            .await?
            .unwrap_or_else(|error| error_output(error.0));
        while let Ok(update) = self.updates.try_recv() {
            self.tool_update(update);
        }
        self.tool_items.remove(&call.id);
        let cancelled = self.cancel.is_cancelled();
        let text: Vec<&str> = output
            .content
            .iter()
            .filter_map(|part| match part {
                Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let text = text.join("\n");
        let mut transcript = format!("Ran `{command}`\n");
        if text.trim().is_empty() {
            transcript.push_str("(no output)");
        } else {
            transcript.push_str(&format!("```\n{}\n```", text.trim_end()));
        }
        if cancelled {
            transcript.push_str("\n\n(command cancelled)");
        }
        self.append(EntryKind::Message {
            reasoning: None,
            usage: None,
            stop_reason: None,
            details: None,
            message: Message {
                role: Role::User,
                content: vec![text_content(transcript)],
                opaque: None,
            },
            model: None,
        })
        .await?;
        self.completed_item(KnownItem::ToolResult {
            id: result_id,
            call_id: ItemId(call.id),
            is_error: output.is_error,
            output: serde_json::to_value(output).map_err(|error| error.to_string())?,
        });
        self.complete(if cancelled {
            TurnStatus::Interrupted
        } else {
            TurnStatus::Completed
        });
        Ok(())
    }

    async fn command(&mut self, name: String, arguments: String) {
        let Some(command) = self
            .registry
            .commands()
            .iter()
            .find(|command| command.info().name == name)
            .cloned()
        else {
            self.emit(EventKind::Error {
                message: format!("unknown command /{name}"),
            });
            return;
        };
        if command.background() {
            if let Err(error) = self.background_command(command, arguments, None).await {
                self.emit(EventKind::Error { message: error.0 });
            }
            return;
        }
        if command.interactive() {
            match self.interactive_command(command, &arguments).await {
                Ok(text) => self.emit(EventKind::CommandOutput { name, text }),
                Err(error) => self.emit(EventKind::Error { message: error.0 }),
            }
            return;
        }
        let cancel = self.shutdown.child_token();
        let session = if command.needs_session_view() {
            match self.session_view(cancel.clone()).await {
                Ok(session) => session,
                Err(message) => {
                    self.emit(EventKind::Error { message });
                    return;
                }
            }
        } else {
            tiechui_plugin_api::SessionView::default()
        };
        match command.inspect(&arguments, session, cancel).await {
            Ok(tiechui_plugin_api::CommandResult::Text(text)) => {
                self.emit(EventKind::CommandOutput { name, text })
            }
            Ok(tiechui_plugin_api::CommandResult::Panel {
                snapshot,
                view,
                text,
            }) => self.emit(EventKind::CommandPanel {
                name,
                panel: snapshot,
                view,
                text,
            }),
            Err(error) => self.emit(EventKind::Error { message: error.0 }),
        }
    }

    async fn tool_result(&mut self, call: &ToolCall, output: ToolOutput) -> Result<String, String> {
        let is_error = output.is_error;
        let result = ToolResult {
            call_id: call.id.clone(),
            content: output.content.clone(),
            is_error: output.is_error,
        };
        let id = self
            .append(EntryKind::Message {
                reasoning: None,
                usage: None,
                stop_reason: None,
                details: output.details.clone(),
                message: Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult(result)],
                    opaque: None,
                },
                model: None,
            })
            .await?;
        let started = self.tool_items.remove(&call.id);
        let item = KnownItem::ToolResult {
            id: started.clone().unwrap_or_else(|| ItemId(id.clone())),
            call_id: ItemId(call.id.clone()),
            output: serde_json::to_value(output).map_err(|error| error.to_string())?,
            is_error,
        };
        if started.is_some() {
            self.completed_item(item);
        } else {
            self.item(item);
        }
        Ok(id)
    }

    fn complete(&mut self, status: TurnStatus) {
        if let Some(mut turn) = self.turn.take() {
            self.settled = Some(status);
            turn.status = status;
            turn.ended_at = Some(tiechui_session::now_ms());
            self.emit(EventKind::TurnCompleted { turn });
        }
        self.submission = None;
    }
}

fn provider_error(kind: ErrorKind, message: &str) -> ProviderError {
    ProviderError {
        kind,
        message: message.into(),
        retry_after: None,
    }
}
fn thinking_level(level: ThinkingLevel) -> Option<ReasoningLevel> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Minimal => Some(ReasoningLevel::Minimal),
        ThinkingLevel::Low => Some(ReasoningLevel::Low),
        ThinkingLevel::Medium => Some(ReasoningLevel::Medium),
        ThinkingLevel::High => Some(ReasoningLevel::High),
        ThinkingLevel::Xhigh => Some(ReasoningLevel::Xhigh),
        ThinkingLevel::Max => Some(ReasoningLevel::Max),
    }
}

/// The requested level, else the nearest supported one above it, else below.
fn clamp(
    level: Option<ReasoningLevel>,
    supported: &[Option<ReasoningLevel>],
) -> Option<ReasoningLevel> {
    let order: Vec<_> = std::iter::once(None)
        .chain(ReasoningLevel::ALL.map(Some))
        .collect();
    let index = order.iter().position(|l| *l == level).unwrap_or(0);
    let (below, above) = order.split_at(index);
    above
        .iter()
        .chain(below.iter().rev())
        .find(|l| supported.contains(l))
        .copied()
        .flatten()
}

fn protocol_thinking(level: Option<ReasoningLevel>) -> ThinkingLevel {
    match level {
        None => ThinkingLevel::Off,
        Some(ReasoningLevel::Minimal) => ThinkingLevel::Minimal,
        Some(ReasoningLevel::Low) => ThinkingLevel::Low,
        Some(ReasoningLevel::Medium) => ThinkingLevel::Medium,
        Some(ReasoningLevel::High) => ThinkingLevel::High,
        Some(ReasoningLevel::Xhigh) => ThinkingLevel::Xhigh,
        Some(ReasoningLevel::Max) => ThinkingLevel::Max,
    }
}
fn text_content(text: String) -> Content {
    Content::Text { text, opaque: None }
}
fn error_output(message: impl Into<String>) -> ToolOutput {
    ToolOutput {
        is_error: true,
        ..ToolOutput::text(message)
    }
}
fn tool_calls(message: &Message) -> Vec<ToolCall> {
    message
        .content
        .iter()
        .filter_map(|content| {
            if let Content::ToolCall(call) = content {
                Some(call.clone())
            } else {
                None
            }
        })
        .collect()
}
fn replace_usage(total: &mut Usage, previous: Usage, next: Usage) {
    total.input = total.input.saturating_sub(previous.input);
    total.output = total.output.saturating_sub(previous.output);
    total.cache_read = total.cache_read.saturating_sub(previous.cache_read);
    total.cache_write = total.cache_write.saturating_sub(previous.cache_write);
    total.reasoning = total.reasoning.saturating_sub(previous.reasoning);
    total.cache_write_1h = total.cache_write_1h.saturating_sub(previous.cache_write_1h);
    if let Some(cost) = &mut total.cost_nano_usd {
        *cost = cost.saturating_sub(previous.cost_nano_usd.unwrap_or(0));
    }
    add_usage(total, next);
}

fn add_usage(total: &mut Usage, usage: Usage) {
    total.input = total.input.saturating_add(usage.input);
    total.output = total.output.saturating_add(usage.output);
    total.cache_read = total.cache_read.saturating_add(usage.cache_read);
    total.cache_write = total.cache_write.saturating_add(usage.cache_write);
    total.reasoning = total.reasoning.saturating_add(usage.reasoning);
    total.cache_write_1h = total.cache_write_1h.saturating_add(usage.cache_write_1h);
    if let Some(cost) = usage.cost_nano_usd {
        total.cost_nano_usd = Some(total.cost_nano_usd.unwrap_or(0).saturating_add(cost));
    }
}
fn tokens(usage: Usage) -> TokenUsage {
    TokenUsage {
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        reasoning: usage.reasoning,
        cache_write_1h: usage.cache_write_1h,
        cost_nano_usd: usage.cost_nano_usd,
    }
}
fn content_item(content: Content, id: ItemId) -> Option<KnownItem> {
    match content {
        Content::Text { text, .. } => Some(KnownItem::AssistantMessage { id, text }),
        Content::Thinking { text, .. } => Some(KnownItem::Thinking { id, text }),
        Content::ToolCall(call) => Some(KnownItem::ToolCall {
            id: ItemId(call.id),
            name: call.name,
            arguments: call.arguments,
        }),
        _ => None,
    }
}
fn item_id(item: &KnownItem) -> ItemId {
    match item {
        KnownItem::Compaction { id, .. } => id.clone(),
        KnownItem::UserMessage { id, .. }
        | KnownItem::AssistantMessage { id, .. }
        | KnownItem::Thinking { id, .. }
        | KnownItem::ToolCall { id, .. }
        | KnownItem::ToolResult { id, .. } => id.clone(),
    }
}

#[cfg(test)]
mod tests;
