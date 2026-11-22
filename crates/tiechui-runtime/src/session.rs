//! `ExternalSession`: one external runtime's thread as a session. It consumes the same
//! submissions and publishes the same events as `tiechui_core::SessionActor`, so the
//! app-server serves it through the same `SessionHandle`.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::time::Duration;

use tiechui_core::SessionHandle;
use tiechui_model::ModelId;
use tiechui_protocol::{
    ApprovalDecision, ApprovalId, ApprovalRequest, CommandInfo, DerivedReason, Event, EventKind,
    InputContent, InputDelivery, Item, ItemId, KnownItem, ModelInfo, Op, PERMISSION_MODE_EVENT,
    QueuedInput, RuntimeCapabilities, SessionEpoch, SteeringMode, SubagentItem, SubagentStatus,
    Submission, SubmissionId, ThinkingLevel, Thread, ThreadId, TokenUsage, Turn, TurnId,
    TurnStatus,
};
use tiechui_session::{EntryKind, ModelRef, SessionStore};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::backend::{
    Backend, BackendEvent, BackendItem, CommandAction, StartParams, Started, Steered,
};
use crate::history;

/// How long an interrupt may take to end the turn before the session ends it itself.
pub const INTERRUPT_WINDOW: Duration = Duration::from_secs(10);

const EVENT_CAPACITY: usize = 8192;

/// Which session `open_session` opens.
#[derive(Debug, Clone)]
pub enum Open {
    New {
        cwd: PathBuf,
    },
    /// This session file, in its own working directory.
    Path(PathBuf),
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub home: PathBuf,
    pub open: Open,
    pub epoch: SessionEpoch,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
    /// `rules`, `auto` or `yolo`.
    pub mode: Option<String>,
    pub interrupt_window: Duration,
}

impl SessionConfig {
    pub fn new(home: PathBuf, open: Open) -> Self {
        Self {
            home,
            open,
            epoch: SessionEpoch(0),
            model: None,
            thinking: None,
            mode: None,
            interrupt_window: INTERRUPT_WINDOW,
        }
    }
}

/// A started session.
pub struct Opened {
    pub handle: SessionHandle,
    /// The thread id.
    pub id: String,
    pub path: PathBuf,
    pub cwd: PathBuf,
}

/// Opens or creates the session file, starts the backend's thread (resuming the one the file
/// names) and runs the session until `shutdown` is cancelled, its submissions close or the
/// backend goes away.
pub async fn open_session(
    runtime: &str,
    capabilities: RuntimeCapabilities,
    mut backend: Box<dyn Backend>,
    config: SessionConfig,
    shutdown: CancellationToken,
) -> Result<Opened, String> {
    let runtime_id = runtime.to_owned();
    let (home, open, cancel) = (config.home.clone(), config.open.clone(), shutdown.clone());
    let store = tokio::task::spawn_blocking(move || match open {
        Open::New { cwd } => SessionStore::create_for_runtime(&home, &cwd, &runtime_id, &cancel),
        Open::Path(path) => SessionStore::open(&path, &cancel),
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())?;
    match store.header().runtime.as_deref() {
        Some(stored) if stored == runtime => {}
        stored => {
            return Err(format!(
                "{} belongs to runtime {}, not {runtime}",
                store.path().display(),
                stored.unwrap_or("chui")
            ));
        }
    }
    let path = store.path().to_owned();
    let cwd = store.header().cwd.clone();
    let resume = history::external_id(&store);
    let requested_model = config
        .model
        .clone()
        .or_else(|| history::stored_model(&store, runtime));
    let (started, backend_events) = match backend
        .start(StartParams {
            cwd: cwd.clone(),
            model: requested_model,
            thinking: config.thinking,
            mode: config.mode.clone(),
            resume: resume.clone(),
        })
        .await
    {
        Ok(started) => started,
        Err(error) => {
            backend.shutdown().await;
            return Err(error);
        }
    };
    let id = store.header().id.clone();
    let (events, receiver) = broadcast::channel(EVENT_CAPACITY);
    let (submissions, ops) = mpsc::channel(64);
    let model_ref = ModelRef {
        provider: runtime.to_owned(),
        model: ModelId(started.model.id.clone()),
    };
    let (context, total) = history::usage(&store);
    let initial_models = vec![started.model.clone()];
    let session = Session {
        runtime: runtime.to_owned(),
        capabilities,
        backend,
        backend_events,
        store: Some(store),
        thread: ThreadId(id.clone()),
        epoch: config.epoch,
        mode: config.mode.clone(),
        started,
        models: initial_models,
        commands: Vec::new(),
        model_ref,
        events,
        turn: None,
        last_turn: None,
        submission: None,
        follow_up: VecDeque::new(),
        approvals: Vec::new(),
        context,
        total,
        interrupt_at: None,
        interrupt_window: config.interrupt_window,
        persisted: HashSet::new(),
        resume,
        home: config.home.clone(),
        cwd: cwd.clone(),
        cancel: shutdown.clone(),
    };
    let task = tokio::spawn(session.run(ops, shutdown));
    Ok(Opened {
        handle: SessionHandle {
            submissions,
            events: receiver,
            task,
        },
        id,
        path,
        cwd,
    })
}

struct OpenTurn {
    turn: Turn,
    derived: bool,
}

enum DriveStep {
    Stop,
    Op(Option<Submission>),
    Backend(Option<BackendEvent>),
    Expired,
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

struct Session {
    runtime: String,
    capabilities: RuntimeCapabilities,
    backend: Box<dyn Backend>,
    backend_events: mpsc::Receiver<BackendEvent>,
    store: Option<SessionStore>,
    thread: ThreadId,
    epoch: SessionEpoch,
    mode: Option<String>,
    started: Started,
    models: Vec<ModelInfo>,
    commands: Vec<CommandInfo>,
    model_ref: ModelRef,
    events: broadcast::Sender<Event>,
    turn: Option<OpenTurn>,
    last_turn: Option<TurnId>,
    submission: Option<SubmissionId>,
    follow_up: VecDeque<(Submission, Vec<InputContent>)>,
    approvals: Vec<String>,
    context: Option<TokenUsage>,
    total: TokenUsage,
    interrupt_at: Option<Instant>,
    interrupt_window: Duration,
    persisted: HashSet<ItemId>,
    resume: Option<String>,
    home: PathBuf,
    cwd: PathBuf,
    cancel: CancellationToken,
}

impl Session {
    async fn run(
        mut self,
        mut ops: mpsc::Receiver<Submission>,
        shutdown: CancellationToken,
    ) -> Result<(), String> {
        let result = self.drive(&mut ops, &shutdown).await;
        self.close().await;
        result
    }

    async fn drive(
        &mut self,
        ops: &mut mpsc::Receiver<Submission>,
        shutdown: &CancellationToken,
    ) -> Result<(), String> {
        self.publish_thread().await?;
        loop {
            let deadline = self.interrupt_at;
            let step = tokio::select! {
                biased;
                () = shutdown.cancelled() => DriveStep::Stop,
                submission = ops.recv() => DriveStep::Op(submission),
                event = self.backend_events.recv() => DriveStep::Backend(event),
                () = sleep_until_opt(deadline) => DriveStep::Expired,
            };
            if !self.apply_step(step).await? {
                return Ok(());
            }
        }
    }

    async fn apply_step(&mut self, step: DriveStep) -> Result<bool, String> {
        match step {
            DriveStep::Stop | DriveStep::Op(None) => Ok(false),
            DriveStep::Op(Some(submission)) => {
                self.handle_op(submission).await?;
                Ok(true)
            }
            DriveStep::Backend(Some(event)) => {
                self.handle_backend(event).await?;
                Ok(true)
            }
            DriveStep::Backend(None) => {
                let message = format!("{} exited", self.runtime);
                self.emit(EventKind::Error {
                    message: message.clone(),
                });
                Err(message)
            }
            DriveStep::Expired => {
                self.interrupt_expired().await?;
                Ok(true)
            }
        }
    }

    async fn close(&mut self) {
        self.cancel_approvals(ApprovalDecision::Cancel).await;
        self.complete(TurnStatus::Interrupted).await;
        self.backend.shutdown().await;
    }

    async fn append(&mut self, kind: EntryKind) -> Result<String, String> {
        let mut store = self.store.take().ok_or("session store unavailable")?;
        let (store, result) = tokio::task::spawn_blocking(move || {
            let result = store
                .append(kind, &CancellationToken::new())
                .map(|entry| entry.id.clone());
            (store, result)
        })
        .await
        .map_err(|error| error.to_string())?;
        self.store = Some(store);
        result.map_err(|error| error.to_string())
    }

    fn store(&self) -> Result<&SessionStore, String> {
        self.store
            .as_ref()
            .ok_or_else(|| "session store unavailable".into())
    }

    fn emit(&self, event: EventKind) {
        let _ = self.events.send(Event {
            submission_id: self.submission.clone(),
            thread_id: self.thread.clone(),
            epoch: self.epoch,
            event,
        });
    }

    fn model_info(&self) -> ModelInfo {
        self.started.model.clone()
    }

    async fn publish_thread(&mut self) -> Result<(), String> {
        if self.resume.as_deref() != Some(self.started.external_id.as_str()) {
            let kind = history::thread_entry(&self.started.external_id);
            self.append(kind).await?;
        }
        let turns = history::turns(self.store()?);
        self.emit(EventKind::ThreadStarted {
            thread: Thread {
                id: self.thread.clone(),
                epoch: self.epoch,
                turns,
            },
            replay: false,
        });
        if let Some(mode) = &self.mode {
            self.emit(EventKind::Extension {
                name: PERMISSION_MODE_EVENT.into(),
                data: mode.clone().into(),
            });
        }
        self.emit(EventKind::SessionUpdated {
            model: self.model_info(),
            thinking: self.started.thinking,
            context: self.context,
            total: self.total,
        });
        self.emit(EventKind::ModelsUpdated {
            models: self.models.clone(),
        });
        self.emit(EventKind::CommandsUpdated {
            commands: self.commands.clone(),
        });
        self.emit(EventKind::RetryAvailable { available: false });
        self.publish_queue();
        if self.resume.is_some() && !self.started.resumed {
            self.emit(EventKind::Error {
                message: format!(
                    "{} could not resume its thread; this session continues on a new one",
                    self.runtime
                ),
            });
        }
        Ok(())
    }

    fn publish_queue(&self) {
        self.emit(EventKind::QueueUpdated {
            steering: vec![],
            follow_up: self
                .follow_up
                .iter()
                .map(|(submission, content)| QueuedInput {
                    submission_id: submission.id.clone(),
                    content: content.clone(),
                })
                .collect(),
        });
    }

    fn turn_id(&self) -> Option<TurnId> {
        self.turn.as_ref().map(|open| open.turn.id.clone())
    }

    fn open_root(&self) -> bool {
        self.turn.as_ref().is_some_and(|open| !open.derived)
    }

    fn complete_item(&mut self, item: Item) {
        if let Some(open) = &mut self.turn {
            let turn_id = open.turn.id.clone();
            history::upsert(&mut open.turn, item.clone());
            self.emit(EventKind::ItemCompleted { turn_id, item });
        }
    }

    fn show_item(&mut self, item: Item) {
        if let Some(turn_id) = self.turn_id() {
            self.emit(EventKind::ItemStarted {
                turn_id,
                item: item.clone(),
            });
        }
        self.complete_item(item);
    }

    async fn start_root_turn(
        &mut self,
        submission: SubmissionId,
        content: Vec<InputContent>,
    ) -> Result<(), String> {
        self.submission = Some(submission);
        let entry = self.append(history::user_entry(&content)).await?;
        let turn = Turn {
            id: TurnId(entry.clone()),
            status: TurnStatus::InProgress,
            items: vec![],
            started_at: tiechui_session::now_ms(),
            ended_at: None,
        };
        self.emit(EventKind::TurnStarted { turn: turn.clone() });
        self.turn = Some(OpenTurn {
            turn,
            derived: false,
        });
        self.show_item(Item::Known(KnownItem::UserMessage {
            id: history::user_item_id(&entry),
            content: content.clone(),
        }));
        let _ = self.capture_snapshot(&entry).await;
        if let Err(error) = self.backend.start_turn(&content).await {
            self.emit(EventKind::Error { message: error });
            self.complete(TurnStatus::Failed).await;
        }
        Ok(())
    }

    async fn flush_and_close_derived(&mut self) -> Result<(), String> {
        if self.turn.as_ref().is_some_and(|open| open.derived) {
            self.backend.finish_derived().await;
            while let Ok(event) = self.backend_events.try_recv() {
                self.handle_backend(event).await?;
            }
            self.close_derived(TurnStatus::Completed).await;
        }
        Ok(())
    }

    fn capture_snapshot(&self, anchor: &str) -> tokio::task::JoinHandle<()> {
        let (home, cwd, anchor) = (self.home.clone(), self.cwd.clone(), anchor.to_owned());
        let (session, cancel) = (self.thread.0.clone(), self.cancel.clone());
        tokio::task::spawn_blocking(move || {
            let Ok(store) = tiechui_module_rewind::store::Store::new(
                &home.join("state/rewind"),
                &cwd,
                &session,
            ) else {
                return;
            };
            tiechui_module_rewind::record_turn_snapshot(
                &store,
                &tiechui_module_rewind::SnapshotOptions::default(),
                &cwd,
                &session,
                &anchor,
                &cancel,
            );
        })
    }

    async fn ensure_turn(&mut self, reason: DerivedReason) -> Result<(), String> {
        if self.turn.is_some() {
            return Ok(());
        }
        let from = self
            .last_turn
            .clone()
            .unwrap_or_else(|| TurnId(self.thread.0.clone()));
        let entry = self.append(history::derived_entry(&from, reason)).await?;
        let marker = history::marker_item(&entry, &from, reason);
        let turn = Turn {
            id: TurnId(entry),
            status: TurnStatus::InProgress,
            items: vec![],
            started_at: tiechui_session::now_ms(),
            ended_at: None,
        };
        self.emit(EventKind::TurnStarted { turn: turn.clone() });
        self.turn = Some(OpenTurn {
            turn,
            derived: true,
        });
        self.show_item(marker);
        Ok(())
    }

    async fn close_derived(&mut self, status: TurnStatus) {
        if self.turn.as_ref().is_some_and(|open| open.derived) {
            self.complete(status).await;
        }
    }

    async fn complete(&mut self, status: TurnStatus) {
        self.interrupt_at = None;
        if let Some(open) = self.turn.take() {
            let mut turn = open.turn;
            turn.status = status;
            turn.ended_at = Some(tiechui_session::now_ms());
            self.last_turn = Some(turn.id.clone());
            if !open.derived {
                let _ = self
                    .capture_snapshot(&tiechui_module_rewind::end_anchor(&turn.id.0))
                    .await;
            }
            self.emit(EventKind::TurnCompleted { turn });
        }
        self.submission = None;
    }

    async fn drain_queue(&mut self) -> Result<(), String> {
        if self.turn.is_some() {
            return Ok(());
        }
        if let Some((submission, content)) = self.follow_up.pop_front() {
            self.publish_queue();
            self.start_root_turn(submission.id, content).await?;
        }
        Ok(())
    }

    async fn handle_op(&mut self, submission: Submission) -> Result<(), String> {
        if let Some(entry) = self.capabilities.unsupported_op(&submission.op) {
            self.emit(EventKind::Error {
                message: format!("runtime {} does not support {entry}", self.runtime),
            });
            return Ok(());
        }
        match submission.op.clone() {
            Op::UserInput { content, delivery } => {
                self.user_input(submission, content, delivery).await
            }
            Op::Interrupt { .. } | Op::InterruptIdle => self.interrupt().await,
            Op::ApprovalResponse {
                approval_id,
                decision,
            } => self.answer(approval_id, decision).await,
            Op::Retract { submission_ids } => {
                self.retract(&submission_ids);
                Ok(())
            }
            Op::InputState { .. } | Op::ClearDraft | Op::Reload => Ok(()),
            _ => self.handle_gated_op(submission).await,
        }
    }

    async fn handle_gated_op(&mut self, submission: Submission) -> Result<(), String> {
        match submission.op.clone() {
            Op::RunCommand { name, arguments } => {
                self.run_command(submission, name, arguments).await
            }
            Op::CommandInput { name, data } => {
                if let Err(message) = self.backend.command_input(&name, data).await {
                    self.emit(EventKind::Error { message });
                }
                Ok(())
            }
            Op::SetModel { provider, model } => self.set_model(&provider, &model).await,
            Op::SetThinking { level } => self.set_thinking(level).await,
            op => {
                self.emit(EventKind::Error {
                    message: format!("runtime {} cannot run {op:?}", self.runtime),
                });
                Ok(())
            }
        }
    }

    async fn run_command(
        &mut self,
        submission: Submission,
        name: String,
        arguments: String,
    ) -> Result<(), String> {
        match self.backend.run_command(&name, &arguments).await {
            Ok(CommandAction::Prompt(prompt)) => {
                let content = vec![InputContent::Text { text: prompt }];
                self.user_input(submission, content, InputDelivery::FollowUp)
                    .await
            }
            Ok(CommandAction::Handled {
                output,
                permission_mode,
            }) => {
                if let Some(mode) = permission_mode {
                    self.set_permission_mode(mode);
                }
                if let Some(text) = output {
                    self.emit(EventKind::CommandOutput { name, text });
                }
                Ok(())
            }
            Err(message) => {
                self.emit(EventKind::Error { message });
                Ok(())
            }
        }
    }

    async fn set_model(&mut self, provider: &str, model: &str) -> Result<(), String> {
        let prefix = format!("{provider}/");
        let id = if !provider.is_empty() {
            model.strip_prefix(&prefix).unwrap_or(model)
        } else {
            model
        };
        match self.backend.set_model(id).await {
            Ok(info) => self.apply_model_change(info, true).await,
            Err(message) => {
                self.emit(EventKind::Error { message });
                Ok(())
            }
        }
    }

    async fn set_thinking(&mut self, level: ThinkingLevel) -> Result<(), String> {
        match self.backend.set_thinking(level).await {
            Ok(next) => {
                self.started.thinking = next;
                self.emit_session_updated();
                Ok(())
            }
            Err(message) => {
                self.emit(EventKind::Error { message });
                Ok(())
            }
        }
    }

    async fn apply_model_change(&mut self, info: ModelInfo, persist: bool) -> Result<(), String> {
        self.started.model = info.clone();
        self.model_ref = ModelRef {
            provider: self.runtime.clone(),
            model: ModelId(info.id.clone()),
        };
        if persist {
            self.append(EntryKind::ModelChange {
                model: self.model_ref.clone(),
            })
            .await?;
        }
        self.emit_session_updated();
        match self.models.iter_mut().find(|item| item.id == info.id) {
            Some(existing) => *existing = info,
            None => self.models.push(info),
        }
        self.emit(EventKind::ModelsUpdated {
            models: self.models.clone(),
        });
        Ok(())
    }

    fn emit_session_updated(&self) {
        self.emit(EventKind::SessionUpdated {
            model: self.model_info(),
            thinking: self.started.thinking,
            context: self.context,
            total: self.total,
        });
    }

    fn set_permission_mode(&mut self, mode: String) {
        self.mode = Some(mode.clone());
        self.emit(EventKind::Extension {
            name: PERMISSION_MODE_EVENT.into(),
            data: mode.into(),
        });
    }

    async fn user_input(
        &mut self,
        submission: Submission,
        content: Vec<InputContent>,
        delivery: InputDelivery,
    ) -> Result<(), String> {
        if !self.open_root() {
            self.flush_and_close_derived().await?;
            return self.start_root_turn(submission.id, content).await;
        }
        let steer =
            delivery == InputDelivery::Steer && self.capabilities.steering == SteeringMode::Inject;
        if steer {
            match self.backend.steer(&content).await {
                Ok(Steered::Accepted) => {
                    let entry = self.append(history::user_entry(&content)).await?;
                    self.show_item(Item::Known(KnownItem::UserMessage {
                        id: history::user_item_id(&entry),
                        content,
                    }));
                    return Ok(());
                }
                Ok(Steered::Rejected(_)) => {}
                Err(error) => self.emit(EventKind::Error { message: error }),
            }
        }
        self.follow_up.push_back((submission, content));
        self.publish_queue();
        Ok(())
    }

    fn retract(&mut self, ids: &[SubmissionId]) {
        let mut inputs = Vec::new();
        self.follow_up.retain(|(submission, content)| {
            if !ids.contains(&submission.id) {
                return true;
            }
            inputs.push(QueuedInput {
                submission_id: submission.id.clone(),
                content: content.clone(),
            });
            false
        });
        self.emit(EventKind::InputRetracted { inputs });
        self.publish_queue();
    }

    async fn interrupt(&mut self) -> Result<(), String> {
        if self.turn.is_none() {
            return Ok(());
        }
        if let Err(error) = self.backend.interrupt().await {
            self.emit(EventKind::Error { message: error });
        }
        self.interrupt_at = Some(Instant::now() + self.interrupt_window);
        Ok(())
    }

    async fn interrupt_expired(&mut self) -> Result<(), String> {
        self.interrupt_at = None;
        if self.turn.is_some() {
            self.emit(EventKind::Error {
                message: format!(
                    "{} did not confirm the interrupt within {} s",
                    self.runtime,
                    self.interrupt_window.as_secs()
                ),
            });
            self.cancel_approvals(ApprovalDecision::Cancel).await;
            self.complete(TurnStatus::Interrupted).await;
            self.drain_queue().await?;
        }
        Ok(())
    }

    async fn answer(
        &mut self,
        approval_id: ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), String> {
        let Some(index) = self.approvals.iter().position(|id| *id == approval_id.0) else {
            return Ok(());
        };
        self.approvals.remove(index);
        if let Err(error) = self.backend.answer_approval(&approval_id.0, decision).await {
            self.emit(EventKind::Error { message: error });
        }
        self.emit(EventKind::ApprovalResolved { approval_id });
        Ok(())
    }

    async fn cancel_approvals(&mut self, decision: ApprovalDecision) {
        for id in std::mem::take(&mut self.approvals) {
            let _ = self.backend.answer_approval(&id, decision.clone()).await;
            self.emit(EventKind::ApprovalResolved {
                approval_id: ApprovalId(id),
            });
        }
    }

    async fn handle_backend(&mut self, event: BackendEvent) -> Result<(), String> {
        match event {
            BackendEvent::ItemStarted(BackendItem { item, child_of }) => {
                self.item_started(item, child_of).await
            }
            BackendEvent::ItemDelta { item_id, delta } => {
                self.item_delta(item_id, delta);
                Ok(())
            }
            BackendEvent::ItemCompleted(BackendItem { item, child_of }) => {
                self.item_completed(item, child_of).await
            }
            BackendEvent::Usage(usage) => self.usage(usage).await,
            BackendEvent::ContextWindow(window) => {
                self.context_window(window);
                Ok(())
            }
            BackendEvent::Subagent(update) => self.subagent(update).await,
            other => self.handle_flow(other).await,
        }
    }

    async fn handle_flow(&mut self, event: BackendEvent) -> Result<(), String> {
        match event {
            BackendEvent::Approval { id, request } => self.approval(id, request).await,
            BackendEvent::ApprovalWithdrawn { id } => {
                self.withdraw_approval(id);
                Ok(())
            }
            BackendEvent::TurnCompleted { status, error } => {
                if let Some(message) = error {
                    self.emit(EventKind::Error { message });
                }
                self.complete(status).await;
                self.drain_queue().await
            }
            BackendEvent::Idle => {
                self.close_derived(TurnStatus::Completed).await;
                self.drain_queue().await
            }
            BackendEvent::Error(message) => {
                self.emit(EventKind::Error { message });
                Ok(())
            }
            other => self.handle_meta(other).await,
        }
    }

    async fn handle_meta(&mut self, event: BackendEvent) -> Result<(), String> {
        match event {
            BackendEvent::Commands(commands) => {
                self.commands = commands.clone();
                self.emit(EventKind::CommandsUpdated { commands });
                Ok(())
            }
            BackendEvent::Models(models) => {
                self.update_models(models);
                Ok(())
            }
            BackendEvent::ModelChanged(info) => self.apply_model_change(info, false).await,
            BackendEvent::PermissionMode(mode) => {
                self.set_permission_mode(mode);
                Ok(())
            }
            BackendEvent::ItemStarted(_)
            | BackendEvent::ItemDelta { .. }
            | BackendEvent::ItemCompleted(_)
            | BackendEvent::Usage(_)
            | BackendEvent::ContextWindow(_)
            | BackendEvent::Approval { .. }
            | BackendEvent::ApprovalWithdrawn { .. }
            | BackendEvent::Subagent(_)
            | BackendEvent::TurnCompleted { .. }
            | BackendEvent::Idle
            | BackendEvent::Error(_) => Ok(()),
        }
    }

    fn update_models(&mut self, models: Vec<ModelInfo>) {
        if let Some(found) = models.iter().find(|m| m.id == self.started.model.id) {
            let mut next = found.clone();
            if next.context_window == 0 {
                next.context_window = self.started.model.context_window;
            }
            self.started.model = next;
            self.emit_session_updated();
        }
        self.models = models.clone();
        self.emit(EventKind::ModelsUpdated { models });
    }

    fn item_delta(&mut self, item_id: ItemId, delta: String) {
        if let Some(turn_id) = self.turn_id() {
            self.emit(EventKind::ItemDelta {
                turn_id,
                item_id,
                delta,
            });
        }
    }

    async fn item_started(
        &mut self,
        item: KnownItem,
        child_of: Option<crate::backend::ChildOf>,
    ) -> Result<(), String> {
        if child_of.is_some() {
            return Ok(());
        }
        self.ensure_turn(DerivedReason::Runtime).await?;
        if let Some(turn_id) = self.turn_id() {
            self.emit(EventKind::ItemStarted {
                turn_id,
                item: Item::Known(item),
            });
        }
        Ok(())
    }

    fn context_window(&mut self, window: u64) {
        if self.started.model.context_window == window {
            return;
        }
        self.started.model.context_window = window;
        if let Some(entry) = self
            .models
            .iter_mut()
            .find(|m| m.id == self.started.model.id)
        {
            entry.context_window = window;
        }
        self.emit_session_updated();
        self.emit(EventKind::ModelsUpdated {
            models: self.models.clone(),
        });
    }

    fn withdraw_approval(&mut self, id: String) {
        if let Some(index) = self.approvals.iter().position(|pending| *pending == id) {
            self.approvals.remove(index);
            self.emit(EventKind::ApprovalResolved {
                approval_id: ApprovalId(id),
            });
        }
    }

    async fn subagent(&mut self, update: crate::backend::SubagentUpdate) -> Result<(), String> {
        self.ensure_turn(DerivedReason::Subagent).await?;
        let status = SubagentStatus {
            id: SubagentStatus::item_id(&update.agent_id),
            parent_call_id: update.parent_call_id,
            agent_id: update.agent_id,
            name: update.name,
            state: update.state,
            message: update.message,
        }
        .into_item();
        let kind = history::item_entry(&status);
        self.append(kind).await?;
        self.complete_item(status);
        Ok(())
    }

    async fn item_completed(
        &mut self,
        item: KnownItem,
        child_of: Option<crate::backend::ChildOf>,
    ) -> Result<(), String> {
        let item = match child_of {
            Some(child) => {
                self.ensure_turn(DerivedReason::Subagent).await?;
                let item = SubagentItem {
                    id: history::item_id(&Item::Known(item.clone()))
                        .unwrap_or_else(|| ItemId(String::new())),
                    parent_call_id: child.parent_call_id,
                    agent_id: child.agent_id,
                    item,
                }
                .into_item();
                let kind = history::item_entry(&item);
                self.append(kind).await?;
                item
            }
            None => {
                self.ensure_turn(DerivedReason::Runtime).await?;
                if let Some(id) = history::item_id(&Item::Known(item.clone()))
                    && self.persisted.insert(id)
                    && let Some(kind) = history::item_message(&item, &self.model_ref)
                {
                    self.append(kind).await?;
                }
                Item::Known(item)
            }
        };
        self.complete_item(item);
        Ok(())
    }

    async fn approval(&mut self, id: String, request: ApprovalRequest) -> Result<(), String> {
        self.ensure_turn(DerivedReason::Approval).await?;
        self.approvals.push(id.clone());
        self.emit(EventKind::ApprovalRequested {
            turn_id: self.turn_id(),
            approval_id: ApprovalId(id),
            request,
        });
        Ok(())
    }

    async fn usage(&mut self, usage: TokenUsage) -> Result<(), String> {
        let kind = history::usage_entry(&usage, self.model_ref.clone());
        self.append(kind).await?;
        self.context = Some(usage);
        self.total.input += usage.input;
        self.total.output += usage.output;
        self.total.cache_read += usage.cache_read;
        self.total.cache_write += usage.cache_write;
        self.total.reasoning += usage.reasoning;
        self.total.cache_write_1h += usage.cache_write_1h;
        self.total.cost_nano_usd = match (self.total.cost_nano_usd, usage.cost_nano_usd) {
            (None, None) => None,
            (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
        };
        let turn_id = self
            .turn_id()
            .or_else(|| self.last_turn.clone())
            .unwrap_or_else(|| TurnId(self.thread.0.clone()));
        self.emit(EventKind::TokenUsageUpdated {
            turn_id,
            last: usage,
            total: self.total,
        });
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tiechui_protocol::{DerivedTurnMarker, ModelInfo};

    use super::*;

    struct Mock {
        calls: Arc<Mutex<Vec<String>>>,
        events: Option<mpsc::Receiver<BackendEvent>>,
        steer: Steered,
    }

    #[async_trait]
    impl Backend for Mock {
        async fn start(
            &mut self,
            _: StartParams,
        ) -> Result<(Started, mpsc::Receiver<BackendEvent>), String> {
            Ok((
                Started {
                    external_id: "ext-1".into(),
                    resumed: false,
                    model: ModelInfo {
                        provider: "mock".into(),
                        id: "m".into(),
                        context_window: 0,
                        reasoning: false,
                        thinking_levels: vec![ThinkingLevel::Off],
                    },
                    thinking: ThinkingLevel::Off,
                },
                self.events.take().unwrap(),
            ))
        }
        async fn start_turn(&mut self, input: &[InputContent]) -> Result<(), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("turn {}", text(input)));
            Ok(())
        }
        async fn steer(&mut self, input: &[InputContent]) -> Result<Steered, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("steer {}", text(input)));
            Ok(self.steer.clone())
        }
        async fn interrupt(&mut self) -> Result<(), String> {
            self.calls.lock().unwrap().push("interrupt".into());
            Ok(())
        }
        async fn answer_approval(&mut self, id: &str, _: ApprovalDecision) -> Result<(), String> {
            self.calls.lock().unwrap().push(format!("answer {id}"));
            Ok(())
        }
        async fn shutdown(&mut self) {
            self.calls.lock().unwrap().push("shutdown".into());
        }
    }

    fn text(input: &[InputContent]) -> String {
        input
            .iter()
            .filter_map(|part| match part {
                InputContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    struct Harness {
        _home: tempfile::TempDir,
        submissions: mpsc::Sender<Submission>,
        events: broadcast::Receiver<Event>,
        backend: mpsc::Sender<BackendEvent>,
        calls: Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<Result<(), String>>,
        shutdown: CancellationToken,
        next: u64,
    }

    async fn harness(
        steer: Steered,
        capabilities: RuntimeCapabilities,
        window: Duration,
    ) -> Harness {
        let home = tempfile::tempdir().unwrap();
        let cwd = home.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let (backend, rx) = mpsc::channel(64);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut config = SessionConfig::new(home.path().join("home"), Open::New { cwd });
        config.interrupt_window = window;
        let shutdown = CancellationToken::new();
        let opened = open_session(
            "mock",
            capabilities,
            Box::new(Mock {
                calls: calls.clone(),
                events: Some(rx),
                steer,
            }),
            config,
            shutdown.clone(),
        )
        .await
        .unwrap();
        Harness {
            _home: home,
            submissions: opened.handle.submissions,
            events: opened.handle.events,
            backend,
            calls,
            task: opened.handle.task,
            shutdown,
            next: 0,
        }
    }

    impl Harness {
        async fn submit(&mut self, op: Op) -> SubmissionId {
            self.next += 1;
            let id = SubmissionId(format!("s{}", self.next));
            self.submissions
                .send(Submission {
                    id: id.clone(),
                    thread_id: ThreadId("t".into()),
                    epoch: SessionEpoch(0),
                    op,
                })
                .await
                .unwrap();
            id
        }

        async fn input(&mut self, text: &str, delivery: InputDelivery) -> SubmissionId {
            self.submit(Op::UserInput {
                content: vec![InputContent::Text { text: text.into() }],
                delivery,
            })
            .await
        }

        async fn until(&mut self, matches: impl Fn(&EventKind) -> bool) -> Vec<EventKind> {
            let mut seen = Vec::new();
            loop {
                let event = tokio::time::timeout(Duration::from_secs(5), self.events.recv())
                    .await
                    .expect("no event")
                    .unwrap();
                let done = matches(&event.event);
                seen.push(event.event);
                if done {
                    return seen;
                }
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        async fn calls_len(&self, count: usize) -> Vec<String> {
            for _ in 0..100 {
                if self.calls().len() >= count {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            self.calls()
        }
    }

    fn completed() -> BackendEvent {
        BackendEvent::TurnCompleted {
            status: TurnStatus::Completed,
            error: None,
        }
    }

    fn is_turn_completed(kind: &EventKind) -> bool {
        matches!(kind, EventKind::TurnCompleted { .. })
    }

    fn started(kind: &EventKind) -> bool {
        matches!(kind, EventKind::TurnStarted { .. })
    }

    const WINDOW: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn a_rejected_steer_runs_after_the_turn_in_order() {
        let mut h = harness(
            Steered::Rejected("no".into()),
            RuntimeCapabilities::minimal(),
            WINDOW,
        )
        .await;
        h.input("first", InputDelivery::FollowUp).await;
        h.until(started).await;
        let steered = h.input("second", InputDelivery::Steer).await;
        let queued = h
            .until(|kind| matches!(kind, EventKind::QueueUpdated { follow_up, .. } if !follow_up.is_empty()))
            .await;
        let EventKind::QueueUpdated { follow_up, .. } = queued.last().unwrap() else {
            unreachable!()
        };
        assert_eq!(follow_up[0].submission_id, steered);
        h.backend.send(completed()).await.unwrap();
        h.until(is_turn_completed).await;
        h.until(started).await;
        assert_eq!(
            h.calls_len(3).await,
            ["turn first", "steer second", "turn second"]
        );
    }

    #[tokio::test]
    async fn a_queue_runtime_never_steers_and_input_can_be_retracted() {
        let queue = RuntimeCapabilities {
            steering: SteeringMode::Queue,
            ..RuntimeCapabilities::minimal()
        };
        let mut h = harness(Steered::Accepted, queue, WINDOW).await;
        h.input("first", InputDelivery::FollowUp).await;
        h.until(started).await;
        let steered = h.input("second", InputDelivery::Steer).await;
        h.until(|kind| matches!(kind, EventKind::QueueUpdated { follow_up, .. } if follow_up.len() == 1))
            .await;
        h.submit(Op::Retract {
            submission_ids: vec![steered],
        })
        .await;
        let seen = h.until(|kind| matches!(kind, EventKind::QueueUpdated { follow_up, .. } if follow_up.is_empty())).await;
        assert!(
            seen.iter().any(
                |kind| matches!(kind, EventKind::InputRetracted { inputs } if inputs.len() == 1)
            )
        );
        assert_eq!(h.calls(), ["turn first"]);
    }

    #[tokio::test]
    async fn an_accepted_steer_joins_the_running_turn() {
        let mut h = harness(Steered::Accepted, RuntimeCapabilities::minimal(), WINDOW).await;
        h.input("first", InputDelivery::FollowUp).await;
        h.until(started).await;
        h.input("more", InputDelivery::Steer).await;
        let seen = h
            .until(|kind| matches!(kind, EventKind::ItemCompleted { item: Item::Known(KnownItem::UserMessage { content, .. }), .. } if text(content) == "more"))
            .await;
        assert!(
            !seen
                .iter()
                .any(|kind| matches!(kind, EventKind::TurnStarted { .. }))
        );
        assert_eq!(h.calls(), ["turn first", "steer more"]);
    }

    #[tokio::test]
    async fn an_unconfirmed_interrupt_ends_the_turn_when_the_window_passes() {
        let mut h = harness(
            Steered::Accepted,
            RuntimeCapabilities::minimal(),
            Duration::from_millis(100),
        )
        .await;
        h.input("long", InputDelivery::FollowUp).await;
        h.until(started).await;
        h.submit(Op::Interrupt {
            turn_id: TurnId("any".into()),
        })
        .await;
        let seen = h.until(is_turn_completed).await;
        assert!(seen
            .iter()
            .any(|kind| matches!(kind, EventKind::Error { message } if message.contains("did not confirm"))));
        let EventKind::TurnCompleted { turn } = seen.last().unwrap() else {
            unreachable!()
        };
        assert_eq!(turn.status, TurnStatus::Interrupted);
        assert!(h.calls().contains(&"interrupt".to_string()));
    }

    #[tokio::test]
    async fn a_confirmed_interrupt_ends_the_turn_with_the_backends_status() {
        let mut h = harness(Steered::Accepted, RuntimeCapabilities::minimal(), WINDOW).await;
        h.input("long", InputDelivery::FollowUp).await;
        h.until(started).await;
        h.submit(Op::Interrupt {
            turn_id: TurnId("any".into()),
        })
        .await;
        h.backend
            .send(BackendEvent::TurnCompleted {
                status: TurnStatus::Interrupted,
                error: None,
            })
            .await
            .unwrap();
        let seen = h.until(is_turn_completed).await;
        assert!(
            !seen
                .iter()
                .any(|kind| matches!(kind, EventKind::Error { .. }))
        );
    }

    #[tokio::test]
    async fn output_after_the_turn_opens_a_derived_turn_that_idle_closes() {
        let mut h = harness(Steered::Accepted, RuntimeCapabilities::minimal(), WINDOW).await;
        h.input("go", InputDelivery::FollowUp).await;
        let first = h.until(started).await;
        let EventKind::TurnStarted { turn: root } = first.last().unwrap() else {
            unreachable!()
        };
        h.backend.send(completed()).await.unwrap();
        h.until(is_turn_completed).await;
        h.backend
            .send(BackendEvent::Approval {
                id: "ap".into(),
                request: ApprovalRequest::Information {
                    title: "t".into(),
                    text: "x".into(),
                },
            })
            .await
            .unwrap();
        let seen = h
            .until(|kind| matches!(kind, EventKind::ApprovalRequested { .. }))
            .await;
        let EventKind::TurnStarted { turn: derived } = &seen[0] else {
            panic!("{seen:?}");
        };
        let marker = DerivedTurnMarker::from_item(match &seen[2] {
            EventKind::ItemCompleted { item, .. } => item,
            other => panic!("{other:?}"),
        })
        .unwrap();
        assert_eq!(marker.derived_from, root.id);
        assert_eq!(marker.reason, DerivedReason::Approval);
        let EventKind::ApprovalRequested { turn_id, .. } = seen.last().unwrap() else {
            unreachable!()
        };
        assert_eq!(turn_id.as_ref(), Some(&derived.id));
        h.backend.send(BackendEvent::Idle).await.unwrap();
        let done = h.until(is_turn_completed).await;
        let EventKind::TurnCompleted { turn } = done.last().unwrap() else {
            unreachable!()
        };
        assert_eq!(turn.id, derived.id);
        assert!(DerivedTurnMarker::from_item(&turn.items[0]).is_some());
    }

    #[tokio::test]
    async fn a_new_user_turn_closes_an_open_derived_turn_first() {
        let mut h = harness(Steered::Accepted, RuntimeCapabilities::minimal(), WINDOW).await;
        h.input("go", InputDelivery::FollowUp).await;
        h.until(started).await;
        h.backend.send(completed()).await.unwrap();
        h.until(is_turn_completed).await;
        h.backend
            .send(BackendEvent::Error("late".into()))
            .await
            .unwrap();
        h.backend
            .send(BackendEvent::ItemCompleted(BackendItem::root(
                KnownItem::AssistantMessage {
                    id: ItemId("late".into()),
                    text: "late text".into(),
                },
            )))
            .await
            .unwrap();
        h.until(|kind| {
            matches!(
                kind,
                EventKind::ItemCompleted {
                    item: Item::Known(KnownItem::AssistantMessage { .. }),
                    ..
                }
            )
        })
        .await;
        h.input("again", InputDelivery::Steer).await;
        let seen = h
            .until(|kind| matches!(kind, EventKind::TurnStarted { .. }))
            .await;
        assert!(
            matches!(&seen[0], EventKind::TurnCompleted { .. }),
            "{seen:?}"
        );
        assert_eq!(h.calls_len(2).await, ["turn go", "turn again"]);
    }

    #[tokio::test]
    async fn an_idle_decided_before_a_queued_turn_starts_does_not_close_it() {
        let mut h = harness(Steered::Accepted, RuntimeCapabilities::minimal(), WINDOW).await;
        h.input("go", InputDelivery::FollowUp).await;
        h.until(started).await;
        h.backend.send(BackendEvent::Idle).await.unwrap();
        h.backend
            .send(BackendEvent::ItemCompleted(BackendItem::root(
                KnownItem::AssistantMessage {
                    id: ItemId("reply".into()),
                    text: "still the same turn".into(),
                },
            )))
            .await
            .unwrap();
        h.backend.send(completed()).await.unwrap();
        let done = h.until(is_turn_completed).await;
        let EventKind::TurnCompleted { turn } = done.last().unwrap() else {
            unreachable!()
        };
        assert!(
            turn.items.iter().any(|item| matches!(item,
                Item::Known(KnownItem::AssistantMessage { id, .. }) if id.0 == "reply")),
            "{:?}",
            turn.items
        );
        assert!(DerivedTurnMarker::from_item(&turn.items[0]).is_none());
    }

    #[tokio::test]
    async fn the_backend_going_away_settles_the_turn_and_fails_the_task() {
        let mut h = harness(Steered::Accepted, RuntimeCapabilities::minimal(), WINDOW).await;
        h.input("go", InputDelivery::FollowUp).await;
        h.until(started).await;
        h.backend
            .send(BackendEvent::Approval {
                id: "ap".into(),
                request: ApprovalRequest::Information {
                    title: "t".into(),
                    text: "x".into(),
                },
            })
            .await
            .unwrap();
        drop(std::mem::replace(&mut h.backend, mpsc::channel(1).0));
        let seen = h.until(is_turn_completed).await;
        assert!(
            seen.iter()
                .any(|kind| matches!(kind, EventKind::ApprovalResolved { .. }))
        );
        let result = h.task.await.unwrap();
        assert_eq!(result.unwrap_err(), "mock exited");
    }

    #[tokio::test]
    async fn shutdown_settles_approvals_and_stops_the_backend() {
        let mut h = harness(Steered::Accepted, RuntimeCapabilities::minimal(), WINDOW).await;
        h.input("go", InputDelivery::FollowUp).await;
        h.until(started).await;
        h.shutdown.cancel();
        h.until(is_turn_completed).await;
        let calls = h.calls.clone();
        h.task.await.unwrap().unwrap();
        assert_eq!(
            calls.lock().unwrap().last().map(String::as_str),
            Some("shutdown")
        );
    }

    #[tokio::test]
    async fn a_gated_op_that_reaches_the_actor_is_refused_not_run() {
        let mut h = harness(Steered::Accepted, RuntimeCapabilities::minimal(), WINDOW).await;
        h.submit(Op::Retry).await;
        let seen = h
            .until(|kind| matches!(kind, EventKind::Error { message } if message.contains("does not support retry")))
            .await;
        assert!(!seen.is_empty());
    }

    #[tokio::test]
    async fn reopening_resumes_the_stored_external_thread_and_rebuilds_turns() {
        let home = tempfile::tempdir().unwrap();
        let cwd = home.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let first = {
            let mut h = harness(Steered::Accepted, RuntimeCapabilities::minimal(), WINDOW).await;
            h.input("remember", InputDelivery::FollowUp).await;
            h.until(started).await;
            h.backend
                .send(BackendEvent::ItemCompleted(BackendItem::root(
                    KnownItem::AssistantMessage {
                        id: ItemId("a".into()),
                        text: "answer".into(),
                    },
                )))
                .await
                .unwrap();
            h.backend.send(completed()).await.unwrap();
            h.until(is_turn_completed).await;
            h.shutdown.cancel();
            let _ = h.task.await;
            let path = std::fs::read_dir(h._home.path().join("home/sessions"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let file = std::fs::read_dir(path)
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path();
            let copy = home.path().join("kept.jsonl");
            std::fs::copy(&file, &copy).unwrap();
            copy
        };
        let store = SessionStore::open_read_only(&first, &CancellationToken::new()).unwrap();
        assert_eq!(store.header().runtime.as_deref(), Some("mock"));
        assert_eq!(history::external_id(&store).as_deref(), Some("ext-1"));
        let turns = history::turns(&store);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].items.len(), 2);
    }
}
