//! Serialized interactive commands. Modules decide what a panel and its actions mean.

use super::*;
use tiechui_plugin_api::{
    BranchView, CommandHost, CommandSession, SessionEffect, ToolError, async_trait,
};
use tokio::sync::oneshot;

pub(super) enum Request {
    Scoped(SessionEpoch, Box<Request>),
    Finished(String, Result<String, ToolError>),
    Prompt(interactions::Message),
    Update(ApprovalId, ApprovalRequest),
    Apply(SessionEffect, oneshot::Sender<Result<(), ToolError>>),
}

struct Host {
    tx: mpsc::UnboundedSender<Request>,
    current: std::sync::Mutex<Option<ApprovalId>>,
    epoch: Option<SessionEpoch>,
    model: Option<tiechui_plugin_api::ModelContext>,
    providers: BTreeMap<String, Arc<dyn ModelProvider>>,
    registry: Arc<Registry>,
    scheduler: Arc<ToolScheduler>,
    branch: BranchView,
    lifetime: CancellationToken,
}
impl Host {
    fn send(&self, request: Request) -> Result<(), ToolError> {
        let request = match self.epoch {
            Some(epoch) => Request::Scoped(epoch, Box::new(request)),
            None => request,
        };
        self.tx
            .send(request)
            .map_err(|_| ToolError("Session closed".into()))
    }
}
struct Withdraw(
    mpsc::UnboundedSender<Request>,
    ApprovalId,
    Option<SessionEpoch>,
);
impl Drop for Withdraw {
    fn drop(&mut self) {
        let request = Request::Prompt(interactions::Message::Withdraw(self.1.clone()));
        let request = match self.2 {
            Some(epoch) => Request::Scoped(epoch, Box::new(request)),
            None => request,
        };
        let _ = self.0.send(request);
    }
}

#[async_trait]
impl CommandHost for Host {
    fn tool_specs(&self) -> Vec<tiechui_model::ToolSpec> {
        self.registry.tools().specs()
    }
    async fn execute_tool(
        &self,
        scope: &str,
        mut call: ToolCall,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        if cancel.is_cancelled() || self.lifetime.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        call.id = format!("command-{scope}-{}", call.id);
        let tx = self.tx.clone();
        let epoch = self.epoch;
        let ctx = ToolContext {
            invoker: Some(Arc::new(interception::Invoker {
                registry: self.registry.clone(),
                scheduler: self.scheduler.clone(),
                model: None,
            })),
            branch: Arc::new(self.branch.entries.clone()),
            parent_session: self.branch.parent_session.clone(),
            cwd: self.branch.cwd.clone(),
            cancel: cancel.clone(),
            updates: Arc::new(|_| {}),
            append_entry: Arc::new(move |entry| {
                let (reply, _) = oneshot::channel();
                let request = Request::Apply(
                    SessionEffect::Custom {
                        kind: entry.kind,
                        data: entry.data,
                    },
                    reply,
                );
                let request = match epoch {
                    Some(epoch) => Request::Scoped(epoch, Box::new(request)),
                    None => request,
                };
                tx.send(request)
                    .map_err(|_| ToolError("Session closed".into()))
            }),
            epoch: self.epoch.unwrap_or(SessionEpoch(0)),
            scope: ExecutionScope {
                thread_id: ThreadId(self.branch.session_id.clone()),
                turn_id: TurnId(format!("command-{scope}")),
            },
            origin: Default::default(),
        };
        let execution = interception::execute_call(&self.registry, &self.scheduler, call, ctx);
        tokio::pin!(execution);
        tokio::select! {
            biased;
            _ = self.lifetime.cancelled() => {
                cancel.cancel();
                Ok(execution.await)
            }
            output = &mut execution => Ok(output),
        }
    }
    fn model_context(&self) -> Result<tiechui_plugin_api::ModelContext, ToolError> {
        self.model
            .clone()
            .ok_or_else(|| ToolError("Model context is unavailable".into()))
    }
    async fn model_stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<tiechui_plugin_api::ModelStream, ToolError> {
        if request
            .tools
            .iter()
            .any(|spec| self.registry.tools().get(&spec.name).is_none())
        {
            return Err(ToolError("Unknown command tool".into()));
        }
        let name = request.model.0.split('/').next().unwrap_or_default();
        let provider = self
            .providers
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError("Unknown model provider".into()))?;
        let mut model = request.model.clone();
        let stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ToolError("Operation aborted".into())),
            result = provider.stream(request, cancel.clone()) => result.map_err(|error| ToolError(error.to_string()))?,
        };
        Ok(stream
            .map(move |event| {
                let mut event = event.map_err(|error| ToolError(error.to_string()))?;
                match &mut event {
                    StreamEvent::ResponseModel { model: returned } => model = returned.clone(),
                    StreamEvent::Usage { usage } | StreamEvent::Finish { usage, .. } => {
                        usage.cost_nano_usd = provider.cost_nano_usd(&model, *usage);
                    }
                    _ => {}
                }
                Ok(event)
            })
            .boxed())
    }

    async fn ask(
        &self,
        request: ApprovalRequest,
        cancel: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = ApprovalId(format!(
            "command-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let (reply, answer) = oneshot::channel();
        *self.current.lock().map_err(|e| ToolError(e.to_string()))? = Some(id.clone());
        self.send(Request::Prompt(interactions::Message::Ask(Box::new(
            interactions::Prompt {
                id: id.clone(),
                request,
                reply,
            },
        ))))
        .map_err(|_| ToolError("Session closed".into()))?;
        let _withdraw = Withdraw(self.tx.clone(), id, self.epoch);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Ok(ApprovalDecision::Cancel),
            result = answer => result.map_err(|_| ToolError("Session closed".into())),
        }
    }
    async fn update(&self, request: ApprovalRequest) -> Result<(), ToolError> {
        let id = self
            .current
            .lock()
            .map_err(|e| ToolError(e.to_string()))?
            .clone()
            .ok_or_else(|| ToolError("No open command panel".into()))?;
        self.send(Request::Update(id, request))
            .map_err(|_| ToolError("Session closed".into()))
    }
    async fn apply(&self, effect: SessionEffect) -> Result<(), ToolError> {
        let (reply, result) = oneshot::channel();
        self.send(Request::Apply(effect, reply))
            .map_err(|_| ToolError("Session closed".into()))?;
        result
            .await
            .map_err(|_| ToolError("Session closed".into()))?
    }
}

const DRAFT: &str = "tiechui.composer.draft";

impl SessionActor {
    pub(super) fn branch_entries(&self) -> Result<Vec<BranchEntry>, String> {
        let store = self.store()?;
        Ok(store
            .path_to_root(store.current_leaf().unwrap_or(&store.header().id))
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|entry| BranchEntry {
                id: entry.id.clone(),
                parent_id: entry.parent_id.clone(),
                timestamp: entry.timestamp,
                content: match &entry.kind {
                    EntryKind::Message { message, .. } if message.role == Role::User => Some(
                        message
                            .content
                            .iter()
                            .filter_map(|part| match part {
                                Content::Text { text, .. } => {
                                    Some(InputContent::Text { text: text.clone() })
                                }
                                Content::Image { mime_type, data } => Some(InputContent::Image {
                                    mime_type: mime_type.clone(),
                                    data: data.clone(),
                                }),
                                _ => None,
                            })
                            .collect(),
                    ),
                    _ => None,
                },
                custom: match &entry.kind {
                    EntryKind::Custom { kind, data } => Some((kind.clone(), data.clone())),
                    _ => None,
                },
            })
            .collect())
    }

    pub(super) fn saved_draft(&self) -> Option<Vec<InputContent>> {
        self.branch_entries()
            .ok()?
            .into_iter()
            .rev()
            .find_map(|entry| {
                entry
                    .custom
                    .filter(|(kind, _)| kind == DRAFT)
                    .map(|(_, data)| serde_json::from_value::<Vec<InputContent>>(data).ok())
            })
            .flatten()
            .filter(|content| {
                content
                    .iter()
                    .any(|part| matches!(part, InputContent::Image { .. }))
            })
    }

    pub(super) async fn save_draft(&mut self, content: Vec<InputContent>) -> Result<(), String> {
        let data = if content
            .iter()
            .any(|part| matches!(part, InputContent::Image { .. }))
        {
            serde_json::to_value(content).map_err(|e| e.to_string())?
        } else {
            if self.saved_draft().is_none() {
                return Ok(());
            }
            serde_json::Value::Null
        };
        self.append(EntryKind::Custom {
            kind: DRAFT.into(),
            data,
        })
        .await?;
        Ok(())
    }

    pub(super) async fn effect(&mut self, effect: SessionEffect) -> Result<(), String> {
        match effect {
            SessionEffect::Validate { session_id, leaf } => {
                let store = self.store()?;
                if session_id != self.thread.0
                    || leaf != store.current_leaf().unwrap_or(&store.header().id)
                    || self.turn.is_some()
                    || !self.steering.is_empty()
                    || !self.follow_up.is_empty()
                {
                    return Err("The session changed during the command. Open it again.".into());
                }
            }
            SessionEffect::Navigate { entry_id } => {
                // Validate first so command callers receive failures, not just a UI notice.
                self.store()?
                    .path_to_root(&entry_id)
                    .map_err(|e| e.to_string())?;
                Box::pin(self.control(Submission {
                    id: SubmissionId("command-navigation".into()),
                    thread_id: self.thread.clone(),
                    epoch: self.epoch,
                    op: Op::Branch { entry_id },
                }))
                .await?;
            }
            SessionEffect::Refill { content } => {
                self.save_draft(content.clone()).await?;
                self.emit(EventKind::ComposerRefilled { content });
            }
            SessionEffect::Quiesce => self.quiesce().await,
            SessionEffect::DetachImages => {
                self.save_draft(vec![]).await?;
                self.emit(EventKind::DraftImagesDetached);
            }
            SessionEffect::Message {
                kind,
                text,
                details,
            } => {
                self.append(EntryKind::CustomMessage {
                    hidden: false,
                    kind: kind.clone(),
                    text: text.clone(),
                    details,
                })
                .await?;
                self.emit(EventKind::CommandOutput { name: kind, text });
            }
            SessionEffect::Custom { kind, data } => {
                self.append(EntryKind::Custom { kind, data }).await?;
            }
            SessionEffect::Emit { kind, data } => {
                self.runtime(tiechui_plugin_api::RuntimeEvent::Extension {
                    name: kind.clone(),
                    data: data.clone(),
                })
                .await?;
                self.emit(EventKind::Extension { name: kind, data })
            }
        }
        Ok(())
    }

    pub(super) async fn command_request(&mut self, request: Request) {
        let Request::Scoped(epoch, request) = request else {
            return;
        };
        if epoch != self.epoch {
            return;
        }
        match *request {
            Request::Apply(effect, reply) => {
                let _ = reply.send(self.effect(effect).await.map_err(ToolError));
            }
            Request::Prompt(message) => self.interaction(Some(message)),
            Request::Update(id, request) => {
                self.prompts.update(&id, request.clone());
                if self.shown().as_ref() == Some(&id) {
                    self.emit(EventKind::ApprovalRequested {
                        turn_id: None,
                        approval_id: id,
                        request,
                    });
                }
            }
            Request::Finished(name, result) => match result {
                Ok(text) => self.emit(EventKind::CommandOutput { name, text }),
                Err(error) => self.emit(EventKind::Error { message: error.0 }),
            },
            Request::Scoped(..) => {}
        }
    }

    pub(super) async fn background_command(
        &mut self,
        command: Arc<dyn tiechui_plugin_api::Command>,
        arguments: String,
        input: Option<serde_json::Value>,
    ) -> Result<(), ToolError> {
        if self.inbox.is_none() {
            return command
                .run(&arguments, self.shutdown.child_token())
                .await
                .map(|text| {
                    self.emit(EventKind::CommandOutput {
                        name: command.info().name,
                        text,
                    })
                });
        }
        let store = self.store().map_err(ToolError)?;
        let branch = BranchView {
            session_id: self.thread.0.clone(),
            leaf: store.current_leaf().unwrap_or(&store.header().id).into(),
            cwd: store.header().cwd.clone(),
            parent_session: store.header().parent_session.clone(),
            entries: vec![],
        };
        let messages = store.build_context_excluding(&self.excluded);
        let mut system = self.system.clone();
        if system.is_empty() {
            let mut fragments = self.session_context.clone();
            fragments.extend(self.turn_context.clone());
            let turn = TurnInput {
                cwd: branch.cwd.clone(),
                content: vec![],
            };
            for provider in self.registry.context_providers() {
                fragments.extend(
                    provider
                        .fragments(&turn, self.shutdown.child_token())
                        .await?,
                );
            }
            system = assemble_system_prompt(&fragments);
        }
        let model = tiechui_plugin_api::ModelContext {
            model: self.model_info(&self.model),
            thinking: protocol_thinking(self.thinking),
            system,
            messages,
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
        };
        let host = Arc::new(Host {
            tx: self.commands_tx.clone(),
            current: std::sync::Mutex::new(None),
            epoch: Some(self.epoch),
            model: Some(model),
            providers: self.config.providers.clone(),
            registry: self.registry.clone(),
            scheduler: self.scheduler.clone(),
            branch: BranchView {
                entries: self.branch_entries().map_err(ToolError)?,
                ..branch.clone()
            },
            lifetime: self.command_cancel.clone(),
        });
        let session = CommandSession {
            branch: branch.clone(),
            host: host.clone(),
        };
        let cancel = self.command_cancel.child_token();
        // Short handlers run to completion in submission order; only their independent
        // model work survives the handler. The actor continues driving its main stream.
        let work = async move {
            let result = match input {
                Some(data) => command.input(data, session, cancel).await,
                None => command.session_run(&arguments, session, cancel).await,
            };
            let _ = host.send(Request::Finished(command.info().name, result));
        };
        tokio::pin!(work);
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => return Err(ToolError("Operation aborted".into())),
                Some(request) = self.commands_rx.recv() => self.command_request(request).await,
                _ = &mut work => return Ok(()),
            }
        }
    }

    pub(super) async fn interactive_command(
        &mut self,
        command: Arc<dyn tiechui_plugin_api::Command>,
        arguments: &str,
    ) -> Result<String, ToolError> {
        if self.inbox.is_none() {
            return command.run(arguments, self.shutdown.child_token()).await;
        }
        if !command.allows_busy(arguments)
            && (self.turn.is_some()
                || !self.steering.is_empty()
                || !self.follow_up.is_empty()
                || self.prompts.head().is_some())
        {
            return Err(ToolError(
                "Stop the current task and resolve queued messages before opening this command."
                    .into(),
            ));
        }
        let store = self.store().map_err(ToolError)?;
        let branch = BranchView {
            session_id: self.thread.0.clone(),
            leaf: store.current_leaf().unwrap_or(&store.header().id).into(),
            cwd: store.header().cwd.clone(),
            parent_session: store.header().parent_session.clone(),
            entries: self.branch_entries().map_err(ToolError)?,
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = CommandSession {
            branch: branch.clone(),
            host: Arc::new(Host {
                tx,
                current: std::sync::Mutex::new(None),
                epoch: None,
                model: None,
                providers: BTreeMap::new(),
                registry: self.registry.clone(),
                scheduler: self.scheduler.clone(),
                branch,
                lifetime: self.command_cancel.clone(),
            }),
        };
        let cancel = self.shutdown.child_token();
        let work = command.session_run(arguments, session, cancel.clone());
        tokio::pin!(work);
        let mut owned = HashSet::new();
        let result = loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => { cancel.cancel(); break Err(ToolError("Operation aborted".into())); }
                request = rx.recv() => match request {
                    Some(Request::Scoped(_, _) | Request::Finished(_, _)) => {},
                    Some(Request::Prompt(message)) => {
                        match &message {
                            interactions::Message::Ask(prompt) => { owned.insert(prompt.id.clone()); }
                            interactions::Message::Withdraw(id) => { owned.remove(id); }
                        }
                        self.interaction(Some(message));
                    }
                    Some(Request::Update(id, request)) => {
                        self.prompts.update(&id, request.clone());
                        if self.shown().as_ref() == Some(&id) {
                            self.emit(EventKind::ApprovalRequested { turn_id: None, approval_id: id, request });
                        }
                    }
                    Some(Request::Apply(effect, reply)) => { let _ = reply.send(self.effect(effect).await.map_err(ToolError)); }
                    None => break Err(ToolError("Command closed".into())),
                },
                Some(request) = self.commands_rx.recv() => self.command_request(request).await,
                message = interactions::next(&mut self.inbox) => self.interaction(message),
                submission = self.ops.recv() => match submission {
                    Some(submission) if self.accepts(&submission) => match submission.op {
                        Op::ApprovalResponse { approval_id, decision } => {
                            let shown = self.shown(); self.prompts.answer(&approval_id, decision); self.publish_prompts(shown);
                        }
                        Op::InputState { blocked } => self.input_blocked = blocked,
                        _ => self.emit(EventKind::Error { message: "Finish or cancel the open command before sending input or changing the session.".into() }),
                    },
                    None => { cancel.cancel(); self.closing = true; break Err(ToolError("Session closed".into())); }
                    _ => {}
                },
                result = &mut work => break result,
            }
        };
        cancel.cancel();
        let shown = self.shown();
        for id in owned {
            self.prompts.apply(interactions::Message::Withdraw(id));
        }
        self.publish_prompts(shown);
        result
    }
}
