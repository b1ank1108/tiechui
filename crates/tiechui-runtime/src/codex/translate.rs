//! Turns what `codex app-server` sends into `BackendEvent`s: the root thread's turns, items and
//! usage, and the server's requests for approval. Frames of other threads (sub-agents) never
//! start or end a root turn; approvals from them are surfaced with their thread as the actor.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tiechui_protocol::{ApprovalDecision, ApprovalRequest, ItemId, KnownItem, TurnStatus};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Instant;

use super::debug;
use super::items::{self, FileCall};
use super::rpc::Rpc;
use super::wire::{
    AgentMessageItem, Breakdown, CommandApproval, CommandItem, DeltaNotification, Elicitation,
    FileApproval, FileChangeItem, Incoming, ItemNotification, McpItem, OtherItem,
    PermissionsApproval, ReasoningItem, ResolvedNotification, StatusNotification, TurnBody,
    TurnNotification, UsageNotification,
};
use crate::backend::{BackendEvent, BackendItem};
use crate::process::DeltaCoalescer;

/// How long streamed text may wait before it is sent on.
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);
/// Buffered text beyond this size is sent at once.
const FLUSH_BYTES: usize = 64 * 1024;

/// What the root thread's turn is doing, shared with the backend's calls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnState {
    /// The turn Codex runs on the root thread.
    pub active: Option<String>,
    /// A `turn/start` was sent and its `turn/started` has not been seen.
    pub pending: bool,
}

/// An approval request waiting for the user, by the id the session knows it under.
pub struct Pending {
    rpc_id: Value,
    kind: Kind,
}

enum Kind {
    /// A command or a file change.
    Review,
    /// What was requested is what an allow grants.
    Permissions(Value),
    Elicitation,
}

impl Pending {
    pub fn rpc_id(&self) -> &Value {
        &self.rpc_id
    }

    pub fn response(&self, decision: &ApprovalDecision) -> Value {
        match &self.kind {
            Kind::Review => json!({"decision": items::review_decision(decision)}),
            Kind::Permissions(requested) => items::permissions_response(requested, decision),
            Kind::Elicitation => items::elicitation_response(decision),
        }
    }
}

pub struct Shared {
    /// The root thread, once Codex has started or resumed it.
    pub thread: Mutex<Option<String>>,
    pub turn: watch::Sender<TurnState>,
    pub approvals: Mutex<HashMap<String, Pending>>,
    /// Notified when the first frame arrives.
    pub first_frame: Notify,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            thread: Mutex::new(None),
            turn: watch::channel(TurnState::default()).0,
            approvals: Mutex::default(),
            first_frame: Notify::new(),
        }
    }
}

impl Shared {
    pub fn root(&self) -> Option<String> {
        self.thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn approvals(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.approvals.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What the reader task hands over.
pub enum Inbound {
    Frame(Vec<u8>),
    Oversize(usize),
    Eof,
}

struct ToolStub {
    id: String,
    name: String,
    arguments: Value,
}

enum Live {
    Text {
        thinking: bool,
        started: bool,
        text: String,
    },
    Tool(Vec<ToolStub>),
}

pub struct Translator {
    shared: Arc<Shared>,
    rpc: Arc<Rpc>,
    out: mpsc::Sender<BackendEvent>,
    coalescer: DeltaCoalescer,
    buffered: usize,
    flush_at: Option<Instant>,
    live: HashMap<String, Live>,
    /// Item ids in the order they started, for finishing what a turn leaves open.
    open: Vec<String>,
    last_total: Option<Breakdown>,
    context_window: Option<u64>,
    last_completed: Option<String>,
    /// Something of the root thread arrived with no turn running: a late item or approval.
    late: bool,
    stderr: Arc<Mutex<std::collections::VecDeque<String>>>,
}

impl Translator {
    pub fn new(
        shared: Arc<Shared>,
        rpc: Arc<Rpc>,
        out: mpsc::Sender<BackendEvent>,
        stderr: Arc<Mutex<std::collections::VecDeque<String>>>,
    ) -> Self {
        Self {
            shared,
            rpc,
            out,
            coalescer: DeltaCoalescer::default(),
            buffered: 0,
            flush_at: None,
            live: HashMap::new(),
            open: Vec::new(),
            last_total: None,
            context_window: None,
            last_completed: None,
            late: false,
            stderr,
        }
    }

    /// Runs until Codex closes its output or the session stops listening.
    pub async fn run(mut self, mut inbound: mpsc::Receiver<Inbound>) {
        loop {
            let wake = async {
                match self.flush_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            let alive = tokio::select! {
                biased;
                message = inbound.recv() => match message {
                    Some(Inbound::Frame(frame)) => self.frame(&frame).await,
                    Some(Inbound::Oversize(size)) => {
                        debug(&format!("dropped a frame of {size} bytes over the limit"));
                        true
                    }
                    Some(Inbound::Eof) | None => {
                        self.exited().await;
                        false
                    }
                },
                () = wake => self.flush().await,
            };
            if !alive {
                break;
            }
        }
        self.rpc.close();
    }

    async fn exited(&mut self) {
        self.flush().await;
        let tail: Vec<String> = self
            .stderr
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        let detail = if tail.is_empty() {
            String::new()
        } else {
            format!(": {}", tail.join(" | "))
        };
        self.send_now(BackendEvent::Error(format!(
            "codex app-server exited{detail}"
        )))
        .await;
    }

    // ----- output -----

    /// Sends the merged text deltas; false when the session is gone.
    async fn flush(&mut self) -> bool {
        self.flush_at = None;
        self.buffered = 0;
        for (item_id, delta) in self.coalescer.drain() {
            if !self
                .send_now(BackendEvent::ItemDelta { item_id, delta })
                .await
            {
                return false;
            }
        }
        true
    }

    async fn send_now(&self, event: BackendEvent) -> bool {
        self.out.send(event).await.is_ok()
    }

    /// Sends an event after the text that came before it.
    async fn emit(&mut self, event: BackendEvent) -> bool {
        self.flush().await && self.send_now(event).await
    }

    fn push_delta(&mut self, item: &str, delta: &str) {
        self.coalescer.push(&ItemId(item.to_owned()), delta);
        self.buffered += delta.len();
        if self.flush_at.is_none() && !delta.is_empty() {
            self.flush_at = Some(Instant::now() + FLUSH_INTERVAL);
        }
    }

    // ----- frames -----

    fn parse<T: DeserializeOwned>(params: Value, what: &str) -> Option<T> {
        match serde_json::from_value(params) {
            Ok(parsed) => Some(parsed),
            Err(error) => {
                debug(&format!("cannot read {what}: {error}"));
                None
            }
        }
    }

    async fn frame(&mut self, frame: &[u8]) -> bool {
        self.shared.first_frame.notify_one();
        let incoming: Incoming = match serde_json::from_slice(frame) {
            Ok(incoming) => incoming,
            Err(error) => {
                debug(&format!("dropped a frame that is not JSON-RPC: {error}"));
                return true;
            }
        };
        match (incoming.id, incoming.method) {
            (Some(id), Some(method)) => self.server_request(id, &method, incoming.params).await,
            (Some(id), None) => {
                let outcome = match (incoming.result, incoming.error) {
                    (_, Some(error)) => Err(error),
                    (Some(result), None) => Ok(result),
                    (None, None) => Ok(Value::Null),
                };
                self.rpc.resolve(&id, outcome);
                true
            }
            (None, Some(method)) => self.notification(&method, incoming.params).await,
            (None, None) => true,
        }
    }

    fn is_root(&self, thread: &str) -> bool {
        self.shared.root().as_deref() == Some(thread)
    }

    async fn notification(&mut self, method: &str, params: Value) -> bool {
        match method {
            "turn/started" => {
                if let Some(note) = Self::parse::<TurnNotification>(params, method)
                    && self.is_root(&note.thread_id)
                {
                    self.shared.turn.send_modify(|state| {
                        state.active = Some(note.turn.id);
                        state.pending = false;
                    });
                } else {
                    debug("dropped a turn/started of another thread");
                }
                true
            }
            "turn/completed" => match Self::parse::<TurnNotification>(params, method) {
                Some(note) if self.is_root(&note.thread_id) => self.turn_completed(note.turn).await,
                Some(note) => {
                    debug(&format!(
                        "dropped a turn/completed of thread {}",
                        note.thread_id
                    ));
                    true
                }
                None => true,
            },
            "item/started" | "item/completed" => {
                let Some(note) = Self::parse::<ItemNotification>(params, method) else {
                    return true;
                };
                if !self.is_root(&note.thread_id) {
                    debug(&format!("dropped an item of thread {}", note.thread_id));
                    return true;
                }
                self.note_late();
                if method == "item/started" {
                    self.item_started(&note.item).await
                } else {
                    self.item_completed(&note.item).await
                }
            }
            "item/agentMessage/delta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/textDelta" => {
                let Some(note) = Self::parse::<DeltaNotification>(params, method) else {
                    return true;
                };
                if !self.is_root(&note.thread_id) {
                    return true;
                }
                self.text_delta(
                    &note.item_id,
                    method != "item/agentMessage/delta",
                    &note.delta,
                )
                .await
            }
            "item/reasoning/summaryPartAdded" => {
                if let Some(note) = Self::parse::<DeltaNotification>(params, method)
                    && self.is_root(&note.thread_id)
                    && note.summary_index.unwrap_or(0) > 0
                {
                    return self.text_delta(&note.item_id, true, "\n\n").await;
                }
                true
            }
            "thread/tokenUsage/updated" => match Self::parse::<UsageNotification>(params, method) {
                Some(note) if self.is_root(&note.thread_id) => self.token_usage(note).await,
                _ => true,
            },
            "serverRequest/resolved" => {
                if let Some(note) = Self::parse::<ResolvedNotification>(params, method) {
                    let id = approval_id(&note.request_id);
                    if self.shared.approvals().remove(&id).is_some() {
                        return self.emit(BackendEvent::ApprovalWithdrawn { id }).await;
                    }
                }
                true
            }
            "thread/status/changed" => {
                if let Some(note) = Self::parse::<StatusNotification>(params, method)
                    && self.is_root(&note.thread_id)
                    && note.status.kind == "idle"
                    && self.late
                {
                    self.late = false;
                    return self.emit(BackendEvent::Idle).await;
                }
                true
            }
            _ => true,
        }
    }

    /// Records that root-thread content arrived while no turn ran.
    fn note_late(&mut self) {
        if self.shared.turn.borrow().active.is_none() {
            self.late = true;
        }
    }

    // ----- turns -----

    async fn turn_completed(&mut self, turn: TurnBody) -> bool {
        let status = match turn.status.as_str() {
            "completed" => TurnStatus::Completed,
            "interrupted" => TurnStatus::Interrupted,
            "failed" => TurnStatus::Failed,
            _ => return true,
        };
        if self.last_completed.as_deref() == Some(turn.id.as_str()) {
            return true;
        }
        let active = self.shared.turn.borrow().active.clone();
        if active.is_some_and(|active| active != turn.id) {
            debug(&format!(
                "dropped the end of turn {}, not the active one",
                turn.id
            ));
            return true;
        }
        if !self.finish_open_items().await {
            return false;
        }
        let withdrawn: Vec<String> = self.shared.approvals().drain().map(|(id, _)| id).collect();
        for id in withdrawn {
            if !self.emit(BackendEvent::ApprovalWithdrawn { id }).await {
                return false;
            }
        }
        self.shared.turn.send_modify(|state| {
            state.active = None;
            state.pending = false;
        });
        self.last_completed = Some(turn.id);
        self.late = false;
        let error = turn.error.map(|error| match error.additional_details {
            Some(details) if !details.is_empty() => format!("{}: {details}", error.message),
            _ => error.message,
        });
        self.emit(BackendEvent::TurnCompleted { status, error })
            .await
    }

    /// Completes what a turn left running, so no client shows a step that never ends.
    async fn finish_open_items(&mut self) -> bool {
        for id in std::mem::take(&mut self.open) {
            match self.live.remove(&id) {
                Some(Live::Text {
                    thinking,
                    started: true,
                    text,
                }) => {
                    let sent = self.emit(text_item(&id, thinking, text)).await;
                    if !sent {
                        return false;
                    }
                }
                Some(Live::Tool(stubs)) => {
                    for stub in stubs {
                        let output =
                            items::tool_output_text("The turn ended before this finished", true);
                        if !self
                            .complete_call(&stub.id, &stub.name, stub.arguments, output, true)
                            .await
                        {
                            return false;
                        }
                    }
                }
                _ => {}
            }
        }
        self.live.clear();
        true
    }

    // ----- items -----

    async fn item_started(&mut self, item: &Value) -> bool {
        let kind = item["type"].as_str().unwrap_or_default();
        match kind {
            "agentMessage" => {
                let Some(item) = Self::parse::<AgentMessageItem>(item.clone(), kind) else {
                    return true;
                };
                self.start_text(&item.id, false).await
            }
            "reasoning" => {
                if let Some(item) = Self::parse::<ReasoningItem>(item.clone(), kind) {
                    self.track(
                        &item.id,
                        Live::Text {
                            thinking: true,
                            started: false,
                            text: String::new(),
                        },
                    );
                }
                true
            }
            "commandExecution" => {
                let Some(item) = Self::parse::<CommandItem>(item.clone(), kind) else {
                    return true;
                };
                let arguments =
                    json!({"command": items::command_text(&item.command, &item.command_actions)});
                self.start_tools(&item.id, vec![("bash".into(), arguments, item.id.clone())])
                    .await
            }
            "fileChange" => {
                let Some(item) = Self::parse::<FileChangeItem>(item.clone(), kind) else {
                    return true;
                };
                let calls = items::file_calls(&item.id, &item.changes)
                    .into_iter()
                    .map(|call| (call.name.to_owned(), call.arguments, call.id))
                    .collect();
                self.start_tools(&item.id, calls).await
            }
            "mcpToolCall" => {
                let Some(item) = Self::parse::<McpItem>(item.clone(), kind) else {
                    return true;
                };
                let call = (
                    items::mcp_name(&item),
                    item.arguments.clone(),
                    item.id.clone(),
                );
                self.start_tools(&item.id, vec![call]).await
            }
            _ => match Self::parse::<OtherItem>(item.clone(), kind)
                .and_then(|other| items::other_call(kind, &other).map(|call| (other.id, call)))
            {
                Some((id, (name, arguments, _, _))) => {
                    self.start_tools(&id, vec![(name, arguments, id.clone())])
                        .await
                }
                None => {
                    if kind != "userMessage" {
                        debug(&format!("skipped an item of kind {kind}"));
                    }
                    true
                }
            },
        }
    }

    async fn item_completed(&mut self, item: &Value) -> bool {
        let kind = item["type"].as_str().unwrap_or_default();
        match kind {
            "agentMessage" => {
                let Some(item) = Self::parse::<AgentMessageItem>(item.clone(), kind) else {
                    return true;
                };
                self.finish_text(&item.id, false, item.text).await
            }
            "reasoning" => {
                let Some(item) = Self::parse::<ReasoningItem>(item.clone(), kind) else {
                    return true;
                };
                let parts = if item.summary.is_empty() {
                    item.content
                } else {
                    item.summary
                };
                self.finish_text(&item.id, true, parts.join("\n\n")).await
            }
            "commandExecution" => {
                let Some(item) = Self::parse::<CommandItem>(item.clone(), kind) else {
                    return true;
                };
                self.live.remove(&item.id);
                let arguments =
                    json!({"command": items::command_text(&item.command, &item.command_actions)});
                let (output, is_error) = items::command_result(&item);
                self.complete_call(&item.id, "bash", arguments, output, is_error)
                    .await
            }
            "fileChange" => {
                let Some(item) = Self::parse::<FileChangeItem>(item.clone(), kind) else {
                    return true;
                };
                self.live.remove(&item.id);
                let failed = matches!(item.status.as_str(), "failed" | "declined");
                for FileCall {
                    id,
                    name,
                    arguments,
                    summary,
                    ..
                } in items::file_calls(&item.id, &item.changes)
                {
                    let text = match item.status.as_str() {
                        "declined" => "The change was declined".to_owned(),
                        "failed" => "The change failed".to_owned(),
                        _ => summary,
                    };
                    let output = items::tool_output_text(&text, failed);
                    if !self
                        .complete_call(&id, name, arguments, output, failed)
                        .await
                    {
                        return false;
                    }
                }
                true
            }
            "mcpToolCall" => {
                let Some(item) = Self::parse::<McpItem>(item.clone(), kind) else {
                    return true;
                };
                self.live.remove(&item.id);
                let (output, is_error) = items::mcp_result(&item);
                self.complete_call(
                    &item.id,
                    &items::mcp_name(&item),
                    item.arguments.clone(),
                    output,
                    is_error,
                )
                .await
            }
            _ => {
                let Some(other) = Self::parse::<OtherItem>(item.clone(), kind) else {
                    return true;
                };
                let Some((name, arguments, text, failed)) = items::other_call(kind, &other) else {
                    return true;
                };
                self.live.remove(&other.id);
                let output = items::tool_output_text(&text, failed);
                self.complete_call(&other.id, &name, arguments, output, failed)
                    .await
            }
        }
    }

    fn track(&mut self, id: &str, live: Live) {
        if self.live.insert(id.to_owned(), live).is_none() {
            self.open.push(id.to_owned());
        }
    }

    async fn start_text(&mut self, id: &str, thinking: bool) -> bool {
        self.track(
            id,
            Live::Text {
                thinking,
                started: true,
                text: String::new(),
            },
        );
        self.emit(BackendEvent::ItemStarted(BackendItem::root(text_known(
            id,
            thinking,
            String::new(),
        ))))
        .await
    }

    /// Appends streamed text, starting the item first when its start has not been seen.
    async fn text_delta(&mut self, id: &str, thinking: bool, delta: &str) -> bool {
        self.note_late();
        let start = match self.live.get_mut(id) {
            Some(Live::Text { started, text, .. }) => {
                text.push_str(delta);
                !std::mem::replace(started, true)
            }
            _ => {
                self.track(
                    id,
                    Live::Text {
                        thinking,
                        started: true,
                        text: delta.to_owned(),
                    },
                );
                true
            }
        };
        if start {
            let started = BackendItem::root(text_known(id, thinking, String::new()));
            if !self.emit(BackendEvent::ItemStarted(started)).await {
                return false;
            }
        }
        self.push_delta(id, delta);
        if self.buffered >= FLUSH_BYTES {
            return self.flush().await;
        }
        true
    }

    async fn finish_text(&mut self, id: &str, thinking: bool, text: String) -> bool {
        let started = matches!(self.live.remove(id), Some(Live::Text { started: true, .. }));
        self.open.retain(|open| open != id);
        if text.is_empty() && !started {
            return true;
        }
        if !started
            && !self
                .emit(BackendEvent::ItemStarted(BackendItem::root(text_known(
                    id,
                    thinking,
                    String::new(),
                ))))
                .await
        {
            return false;
        }
        self.emit(text_item(id, thinking, text)).await
    }

    async fn start_tools(&mut self, id: &str, calls: Vec<(String, Value, String)>) -> bool {
        let mut stubs = Vec::new();
        for (name, arguments, call_id) in calls {
            let event = BackendEvent::ItemStarted(BackendItem::root(KnownItem::ToolCall {
                id: ItemId(call_id.clone()),
                name: name.clone(),
                arguments: arguments.clone(),
            }));
            if !self.emit(event).await {
                return false;
            }
            stubs.push(ToolStub {
                id: call_id,
                name,
                arguments,
            });
        }
        self.track(id, Live::Tool(stubs));
        true
    }

    /// A tool call and its result, both complete.
    async fn complete_call(
        &mut self,
        id: &str,
        name: &str,
        arguments: Value,
        output: Value,
        is_error: bool,
    ) -> bool {
        let call = ItemId(id.to_owned());
        self.emit(BackendEvent::ItemCompleted(BackendItem::root(
            KnownItem::ToolCall {
                id: call.clone(),
                name: name.to_owned(),
                arguments,
            },
        )))
        .await
            && self
                .send_now(BackendEvent::ItemCompleted(BackendItem::root(
                    KnownItem::ToolResult {
                        id: ItemId(format!("{id}:result")),
                        call_id: call,
                        output,
                        is_error,
                    },
                )))
                .await
    }

    // ----- usage -----

    async fn token_usage(&mut self, note: UsageNotification) -> bool {
        let active = self.shared.turn.borrow().active.clone();
        let current = active.as_deref() == Some(note.turn_id.as_str())
            || self.last_completed.as_deref() == Some(note.turn_id.as_str());
        if !current {
            // A resume replays the usage of the thread's last turn.
            debug("dropped a usage notification of another turn");
            return true;
        }
        if self.last_total.as_ref() == Some(&note.token_usage.total) {
            return true;
        }
        self.last_total = Some(note.token_usage.total.clone());
        if let Some(window) = note.token_usage.model_context_window
            && self.context_window != Some(window)
        {
            self.context_window = Some(window);
            if !self.emit(BackendEvent::ContextWindow(window)).await {
                return false;
            }
        }
        self.emit(BackendEvent::Usage(items::usage(&note.token_usage.last)))
            .await
    }

    // ----- server requests -----

    async fn server_request(&mut self, id: Value, method: &str, params: Value) -> bool {
        let answered = match method {
            "item/commandExecution/requestApproval" => {
                match Self::parse::<CommandApproval>(params, method) {
                    Some(request) => {
                        let command = items::command_text(
                            request.command.as_deref().unwrap_or_default(),
                            &request.command_actions,
                        );
                        let thread = request.thread_id.clone();
                        let permission = self.permission(
                            &thread,
                            &request.item_id,
                            "bash",
                            json!({"command": command}),
                            "command",
                            vec![command.clone()],
                            vec![command],
                        );
                        return self.ask(id, Kind::Review, permission).await;
                    }
                    None => self.rpc.respond(&id, json!({"decision": "decline"})).await,
                }
            }
            "item/fileChange/requestApproval" => {
                match Self::parse::<FileApproval>(params, method) {
                    Some(request) => {
                        let (tool, arguments, paths) = self.file_subject(&request);
                        let thread = request.thread_id.clone();
                        let permission = self.permission(
                            &thread,
                            &request.item_id,
                            &tool,
                            arguments,
                            "path",
                            paths.clone(),
                            paths,
                        );
                        return self.ask(id, Kind::Review, permission).await;
                    }
                    None => self.rpc.respond(&id, json!({"decision": "decline"})).await,
                }
            }
            "item/permissions/requestApproval" => {
                match Self::parse::<PermissionsApproval>(params, method) {
                    Some(request) => {
                        let paths = items::requested_paths(&request.permissions);
                        let thread = request.thread_id.clone();
                        let permission = self.permission(
                            &thread,
                            &request.item_id,
                            "request_permissions",
                            json!({"permissions": request.permissions, "reason": request.reason}),
                            "permissions",
                            paths,
                            vec![],
                        );
                        return self
                            .ask(id, Kind::Permissions(request.permissions), permission)
                            .await;
                    }
                    None => {
                        self.rpc
                            .respond(&id, json!({"permissions": {}, "scope": "turn"}))
                            .await
                    }
                }
            }
            "mcpServer/elicitation/request" => {
                match Self::parse::<Elicitation>(params.clone(), method) {
                    Some(request) => {
                        self.note_late_for(&request.thread_id);
                        let request = ApprovalRequest::Elicitation {
                            server: request.server_name,
                            request: params,
                        };
                        return self.ask(id, Kind::Elicitation, request).await;
                    }
                    None => self.rpc.respond(&id, json!({"action": "decline"})).await,
                }
            }
            // The user is not asked free-form questions mid-turn: Codex gets no answers.
            "item/tool/requestUserInput" => self.rpc.respond(&id, json!({"answers": {}})).await,
            "item/tool/call" => {
                self.rpc
                    .respond(
                        &id,
                        json!({"success": false, "contentItems": [{"type": "inputText",
                            "text": "Tiechui does not provide client-side tools to Codex"}]}),
                    )
                    .await
            }
            _ => {
                debug(&format!("rejected the request {method}"));
                self.rpc
                    .reject(&id, -32601, "method not supported by tiechui")
                    .await
            }
        };
        if let Err(error) = answered {
            debug(&error);
            return self.emit(BackendEvent::Error(error)).await;
        }
        true
    }

    fn note_late_for(&mut self, thread: &str) {
        if self.is_root(thread) {
            self.note_late();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn permission(
        &mut self,
        thread: &str,
        item_id: &str,
        tool: &str,
        arguments: Value,
        surface: &str,
        values: Vec<String>,
        session_patterns: Vec<String>,
    ) -> ApprovalRequest {
        self.note_late_for(thread);
        ApprovalRequest::Permission {
            actor: (!self.is_root(thread)).then(|| thread.into()),
            call_id: item_id.to_owned(),
            tool: tool.to_owned(),
            arguments,
            surface: surface.to_owned(),
            values,
            matched_pattern: None,
            origin: "codex".into(),
            session_patterns,
            preview: None,
        }
    }

    /// The tool, arguments and paths a file change approval shows: the first file of the change
    /// it asks about, when the change was seen.
    fn file_subject(&self, request: &FileApproval) -> (String, Value, Vec<String>) {
        if let Some(Live::Tool(stubs)) = self.live.get(&request.item_id) {
            let paths = stubs
                .iter()
                .filter_map(|stub| stub.arguments["path"].as_str().map(str::to_owned))
                .collect();
            if let Some(first) = stubs.first() {
                return (first.name.clone(), first.arguments.clone(), paths);
            }
        }
        let paths: Vec<String> = request.grant_root.iter().cloned().collect();
        ("edit".into(), json!({"path": request.grant_root}), paths)
    }

    /// Registers the approval and reports it.
    async fn ask(&mut self, rpc_id: Value, kind: Kind, request: ApprovalRequest) -> bool {
        let id = approval_id(&rpc_id);
        self.shared
            .approvals()
            .insert(id.clone(), Pending { rpc_id, kind });
        self.emit(BackendEvent::Approval { id, request }).await
    }
}

fn text_known(id: &str, thinking: bool, text: String) -> KnownItem {
    let id = ItemId(id.to_owned());
    if thinking {
        KnownItem::Thinking { id, text }
    } else {
        KnownItem::AssistantMessage { id, text }
    }
}

fn text_item(id: &str, thinking: bool, text: String) -> BackendEvent {
    BackendEvent::ItemCompleted(BackendItem::root(text_known(id, thinking, text)))
}

/// The session's id of an approval: Codex's request id.
pub fn approval_id(rpc_id: &Value) -> String {
    match rpc_id.as_str() {
        Some(text) => format!("codex-{text}"),
        None => format!("codex-{rpc_id}"),
    }
}
