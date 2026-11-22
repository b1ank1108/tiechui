//! Turns what `codex app-server` sends into `BackendEvent`s: the root thread's turns, items and
//! usage, and the server's requests for approval. Frames of sub-agent threads never start or end
//! a root turn; `agents` maps them to sub-agent items, status updates and approvals.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tiechui_protocol::{ApprovalDecision, ApprovalRequest, ItemId, KnownItem, TurnStatus};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::Instant;

use super::debug;
use super::items;
mod agents;
use agents::Agent;

use super::wire::{
    AgentMessageItem, Breakdown, CommandApproval, CommandItem, DeltaNotification, Elicitation,
    FileApproval, FileChangeItem, ItemNotification, McpItem, OtherItem, PermissionsApproval,
    ReasoningItem, ResolvedNotification, StatusNotification, TurnBody, TurnError, TurnNotification,
    UsageNotification,
};
use crate::backend::{BackendEvent, BackendItem};
use crate::process::{DeltaCoalescer, Inbound, StderrTail, stderr_detail};
use crate::rpc::{Incoming, Rpc};

const FLUSH_INTERVAL: Duration = Duration::from_millis(50);
const FLUSH_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnState {
    pub active: Option<String>,
    pub awaiting_turn_started: bool,
}

impl TurnState {
    pub fn record_turn_id_if_unset(&mut self, id: String) {
        if self.awaiting_turn_started {
            self.active = Some(id);
            self.awaiting_turn_started = false;
        }
    }
}

pub struct Pending {
    rpc_id: Value,
    kind: Kind,
    thread: String,
}

enum Kind {
    Review,
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
    pub thread: Mutex<Option<String>>,
    pub turn: watch::Sender<TurnState>,
    pub approvals: Mutex<HashMap<String, Pending>>,
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

    pub fn take_approval(&self, id: &str) -> Option<Pending> {
        self.approvals().remove(id)
    }

    pub fn approvals(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.approvals.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub(super) struct ToolStub {
    pub id: String,
    pub name: String,
    pub arguments: Value,
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
    open: Vec<String>,
    last_total: Option<Breakdown>,
    context_window: Option<u64>,
    last_completed: Option<String>,
    outside_turn_output_seen: bool,
    agents: HashMap<String, Agent>,
    child_tools: HashMap<String, Vec<ToolStub>>,
    stderr: StderrTail,
}

impl Translator {
    pub fn new(
        shared: Arc<Shared>,
        rpc: Arc<Rpc>,
        out: mpsc::Sender<BackendEvent>,
        stderr: StderrTail,
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
            outside_turn_output_seen: false,
            agents: HashMap::new(),
            child_tools: HashMap::new(),
            stderr,
        }
    }

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
        let detail = stderr_detail(&self.stderr);
        self.send_now(BackendEvent::Error(format!(
            "codex app-server exited{detail}"
        )))
        .await;
    }

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

    pub(super) async fn emit_after_flush(&mut self, event: BackendEvent) -> bool {
        self.flush().await && self.send_now(event).await
    }

    fn push_delta(&mut self, item: &str, delta: &str) {
        self.coalescer.push(&ItemId(item.to_owned()), delta);
        self.buffered += delta.len();
        if self.flush_at.is_none() && !delta.is_empty() {
            self.flush_at = Some(Instant::now() + FLUSH_INTERVAL);
        }
    }

    pub(super) fn parse<T: DeserializeOwned>(params: Value, what: &str) -> Option<T> {
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

    pub(super) fn is_root(&self, thread: &str) -> bool {
        self.shared.root().as_deref() == Some(thread)
    }

    async fn notification(&mut self, method: &str, params: Value) -> bool {
        match method {
            "turn/started" => self.on_turn_started(params).await,
            "turn/completed" => self.on_turn_completed(params).await,
            "item/started" | "item/completed" => self.on_item(method, params).await,
            "item/agentMessage/delta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/textDelta" => self.on_delta(method, params).await,
            "item/reasoning/summaryPartAdded" => self.on_summary_part(params).await,
            "thread/tokenUsage/updated" => self.on_token_usage(params).await,
            "serverRequest/resolved" => self.on_request_resolved(params).await,
            "thread/status/changed" => self.on_status_changed(params).await,
            _ => true,
        }
    }

    async fn on_turn_started(&mut self, params: Value) -> bool {
        let Some(note) = Self::parse::<TurnNotification>(params, "turn/started") else {
            return true;
        };
        if self.is_root(&note.thread_id) {
            self.shared.turn.send_modify(|state| {
                state.active = Some(note.turn.id);
                state.awaiting_turn_started = false;
            });
            return true;
        }
        self.agent_turn_started(&note.thread_id).await
    }

    async fn on_turn_completed(&mut self, params: Value) -> bool {
        match Self::parse::<TurnNotification>(params, "turn/completed") {
            Some(note) if self.is_root(&note.thread_id) => self.turn_completed(note.turn).await,
            Some(note) => self.agent_turn_completed(&note.thread_id, note.turn).await,
            None => true,
        }
    }

    async fn on_item(&mut self, method: &str, params: Value) -> bool {
        let Some(note) = Self::parse::<ItemNotification>(params, method) else {
            return true;
        };
        let started = method == "item/started";
        if !self.is_root(&note.thread_id) {
            return self.child_item(&note.thread_id, &note.item, started).await;
        }
        self.note_derived();
        let kind = note.item["type"].as_str().unwrap_or_default();
        if kind == "subAgentActivity" {
            return self.sub_agent_activity(&note.thread_id, &note.item).await;
        }
        let ok = if started {
            self.item_started(&note.item).await
        } else {
            self.item_completed(&note.item).await
        };
        ok && self.collab_effects(&note.thread_id, &note.item).await
    }

    async fn on_delta(&mut self, method: &str, params: Value) -> bool {
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

    async fn on_summary_part(&mut self, params: Value) -> bool {
        if let Some(note) =
            Self::parse::<DeltaNotification>(params, "item/reasoning/summaryPartAdded")
            && self.is_root(&note.thread_id)
            && note.summary_index.unwrap_or(0) > 0
        {
            return self.text_delta(&note.item_id, true, "\n\n").await;
        }
        true
    }

    async fn on_token_usage(&mut self, params: Value) -> bool {
        match Self::parse::<UsageNotification>(params, "thread/tokenUsage/updated") {
            Some(note) if self.is_root(&note.thread_id) => self.token_usage(note).await,
            _ => true,
        }
    }

    async fn on_request_resolved(&mut self, params: Value) -> bool {
        let Some(note) = Self::parse::<ResolvedNotification>(params, "serverRequest/resolved")
        else {
            return true;
        };
        let id = approval_id(&note.request_id);
        if self.shared.take_approval(&id).is_some()
            && !self
                .emit_after_flush(BackendEvent::ApprovalWithdrawn { id })
                .await
        {
            return false;
        }
        match note.thread_id {
            Some(thread) => self.approval_settled(&thread).await,
            None => true,
        }
    }

    async fn on_status_changed(&mut self, params: Value) -> bool {
        if let Some(note) = Self::parse::<StatusNotification>(params, "thread/status/changed")
            && self.is_root(&note.thread_id)
            && note.status.kind == "idle"
        {
            return self.maybe_idle().await;
        }
        true
    }

    async fn turn_completed(&mut self, turn: TurnBody) -> bool {
        let Some(status) = turn_status(&turn.status) else {
            return true;
        };
        if self.is_stale_turn_end(&turn.id) {
            return true;
        }
        if !self.finish_open_items().await {
            return false;
        }
        let root = self.shared.root();
        if !self.withdraw_approvals_of(root.as_deref()).await {
            return false;
        }
        self.shared.turn.send_modify(|state| {
            state.active = None;
            state.awaiting_turn_started = false;
        });
        self.last_completed = Some(turn.id);
        self.outside_turn_output_seen = false;
        let error = turn.error.map(turn_error_text);
        self.emit_after_flush(BackendEvent::TurnCompleted { status, error })
            .await
    }

    fn is_stale_turn_end(&self, turn_id: &str) -> bool {
        if self.last_completed.as_deref() == Some(turn_id) {
            return true;
        }
        let active = self.shared.turn.borrow().active.clone();
        if active.is_some_and(|active| active != turn_id) {
            debug(&format!(
                "dropped the end of turn {turn_id}, not the active one"
            ));
            return true;
        }
        false
    }

    pub(super) async fn withdraw_approvals_of(&mut self, thread: Option<&str>) -> bool {
        let withdrawn: Vec<String> = {
            let mut approvals = self.shared.approvals();
            let ids: Vec<String> = approvals
                .iter()
                .filter(|(_, pending)| Some(pending.thread.as_str()) == thread)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &ids {
                approvals.remove(id);
            }
            ids
        };
        for id in withdrawn {
            if !self
                .emit_after_flush(BackendEvent::ApprovalWithdrawn { id })
                .await
            {
                return false;
            }
        }
        true
    }

    async fn finish_open_items(&mut self) -> bool {
        for id in std::mem::take(&mut self.open) {
            match self.live.remove(&id) {
                Some(Live::Text {
                    thinking,
                    started: true,
                    text,
                }) => {
                    let sent = self.emit_after_flush(text_item(&id, thinking, text)).await;
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

    async fn item_started(&mut self, item: &Value) -> bool {
        let kind = item["type"].as_str().unwrap_or_default();
        match kind {
            "agentMessage" => match Self::parse::<AgentMessageItem>(item.clone(), kind) {
                Some(item) => self.start_text(&item.id, false).await,
                None => true,
            },
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
            _ => match tool_starts(kind, item) {
                Some((id, calls)) => self.start_tools(&id, calls).await,
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
            _ => {
                let Some((id, dones)) = tool_dones(kind, item) else {
                    return true;
                };
                self.live.remove(&id);
                for done in dones {
                    if !self
                        .complete_call(
                            &done.id,
                            &done.name,
                            done.arguments,
                            done.output,
                            done.is_error,
                        )
                        .await
                    {
                        return false;
                    }
                }
                true
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
        self.emit_after_flush(BackendEvent::ItemStarted(BackendItem::root(text_known(
            id,
            thinking,
            String::new(),
        ))))
        .await
    }

    async fn text_delta(&mut self, id: &str, thinking: bool, delta: &str) -> bool {
        self.note_derived();
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
            if !self
                .emit_after_flush(BackendEvent::ItemStarted(started))
                .await
            {
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
                .emit_after_flush(BackendEvent::ItemStarted(BackendItem::root(text_known(
                    id,
                    thinking,
                    String::new(),
                ))))
                .await
        {
            return false;
        }
        self.emit_after_flush(text_item(id, thinking, text)).await
    }

    async fn start_tools(&mut self, id: &str, calls: Vec<(String, Value, String)>) -> bool {
        let mut stubs = Vec::new();
        for (name, arguments, call_id) in calls {
            let event = BackendEvent::ItemStarted(BackendItem::root(KnownItem::ToolCall {
                id: ItemId(call_id.clone()),
                name: name.clone(),
                arguments: arguments.clone(),
            }));
            if !self.emit_after_flush(event).await {
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

    pub(super) async fn complete_call(
        &mut self,
        id: &str,
        name: &str,
        arguments: Value,
        output: Value,
        is_error: bool,
    ) -> bool {
        let call = ItemId(id.to_owned());
        self.emit_after_flush(BackendEvent::ItemCompleted(BackendItem::root(
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
            if !self
                .emit_after_flush(BackendEvent::ContextWindow(window))
                .await
            {
                return false;
            }
        }
        self.emit_after_flush(BackendEvent::Usage(items::usage(&note.token_usage.last)))
            .await
    }

    async fn server_request(&mut self, id: Value, method: &str, params: Value) -> bool {
        let answered = match method {
            "item/commandExecution/requestApproval" => {
                return self.command_approval(id, params).await;
            }
            "item/fileChange/requestApproval" => return self.file_approval(id, params).await,
            "item/permissions/requestApproval" => {
                return self.permissions_approval(id, params).await;
            }
            "mcpServer/elicitation/request" => return self.elicitation(id, params).await,
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
        self.after_answer(answered).await
    }

    async fn answer(&mut self, id: &Value, result: Value) -> bool {
        let answered = self.rpc.respond(id, result).await;
        self.after_answer(answered).await
    }

    async fn after_answer(&mut self, answered: Result<(), String>) -> bool {
        if let Err(error) = answered {
            debug(&error);
            return self.emit_after_flush(BackendEvent::Error(error)).await;
        }
        true
    }

    async fn command_approval(&mut self, id: Value, params: Value) -> bool {
        let method = "item/commandExecution/requestApproval";
        let Some(request) = Self::parse::<CommandApproval>(params, method) else {
            return self.answer(&id, json!({"decision": "decline"})).await;
        };
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
        self.ask(id, &thread, Kind::Review, permission).await
    }

    async fn file_approval(&mut self, id: Value, params: Value) -> bool {
        let Some(request) = Self::parse::<FileApproval>(params, "item/fileChange/requestApproval")
        else {
            return self.answer(&id, json!({"decision": "decline"})).await;
        };
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
        self.ask(id, &thread, Kind::Review, permission).await
    }

    async fn permissions_approval(&mut self, id: Value, params: Value) -> bool {
        let method = "item/permissions/requestApproval";
        let Some(request) = Self::parse::<PermissionsApproval>(params, method) else {
            return self
                .answer(&id, json!({"permissions": {}, "scope": "turn"}))
                .await;
        };
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
        self.ask(
            id,
            &thread,
            Kind::Permissions(request.permissions),
            permission,
        )
        .await
    }

    async fn elicitation(&mut self, id: Value, params: Value) -> bool {
        let Some(request) =
            Self::parse::<Elicitation>(params.clone(), "mcpServer/elicitation/request")
        else {
            return self.answer(&id, json!({"action": "decline"})).await;
        };
        self.note_derived();
        let thread = request.thread_id.clone();
        let request = ApprovalRequest::Elicitation {
            server: request.server_name,
            request: params,
        };
        self.ask(id, &thread, Kind::Elicitation, request).await
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
        self.note_derived();
        let child = !self.is_root(thread);
        ApprovalRequest::Permission {
            actor: child.then(|| thread.into()),
            call_id: if child {
                agents::scoped(thread, item_id)
            } else {
                item_id.to_owned()
            },
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

    fn file_subject(&self, request: &FileApproval) -> (String, Value, Vec<String>) {
        let child = self
            .child_tools
            .get(&agents::scoped(&request.thread_id, &request.item_id));
        let stubs = match self.live.get(&request.item_id) {
            Some(Live::Tool(stubs)) => Some(stubs),
            _ => child,
        };
        if let Some(stubs) = stubs {
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

    async fn ask(
        &mut self,
        rpc_id: Value,
        thread: &str,
        kind: Kind,
        request: ApprovalRequest,
    ) -> bool {
        let id = approval_id(&rpc_id);
        self.shared.approvals().insert(
            id.clone(),
            Pending {
                rpc_id,
                kind,
                thread: thread.to_owned(),
            },
        );
        self.emit_after_flush(BackendEvent::Approval { id, request })
            .await
            && self.agent_waits(thread).await
    }
}

fn turn_status(status: &str) -> Option<TurnStatus> {
    match status {
        "completed" => Some(TurnStatus::Completed),
        "interrupted" => Some(TurnStatus::Interrupted),
        "failed" => Some(TurnStatus::Failed),
        _ => None,
    }
}

fn turn_error_text(error: TurnError) -> String {
    match error.additional_details {
        Some(details) if !details.is_empty() => format!("{}: {details}", error.message),
        _ => error.message,
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

pub fn approval_id(rpc_id: &Value) -> String {
    match rpc_id.as_str() {
        Some(text) => format!("codex-{text}"),
        None => format!("codex-{rpc_id}"),
    }
}

type Starts = (String, Vec<(String, Value, String)>);

pub struct ToolDone {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    pub output: Value,
    pub is_error: bool,
}

pub fn tool_starts(kind: &str, item: &Value) -> Option<Starts> {
    match kind {
        "commandExecution" => command_starts(item),
        "fileChange" => file_starts(item),
        "mcpToolCall" => mcp_starts(item),
        "collabAgentToolCall" => collab_starts(item),
        _ => other_starts(kind, item),
    }
}

fn command_starts(item: &Value) -> Option<Starts> {
    let item = Translator::parse::<CommandItem>(item.clone(), "commandExecution")?;
    let arguments = json!({"command": items::command_text(&item.command, &item.command_actions)});
    Some((item.id.clone(), vec![("bash".into(), arguments, item.id)]))
}

fn file_starts(item: &Value) -> Option<Starts> {
    let item = Translator::parse::<FileChangeItem>(item.clone(), "fileChange")?;
    let calls = items::file_calls(&item.id, &item.changes)
        .into_iter()
        .map(|call| (call.name.to_owned(), call.arguments, call.id))
        .collect();
    Some((item.id, calls))
}

fn mcp_starts(item: &Value) -> Option<Starts> {
    let item = Translator::parse::<McpItem>(item.clone(), "mcpToolCall")?;
    let call = (
        items::mcp_name(&item),
        item.arguments.clone(),
        item.id.clone(),
    );
    Some((item.id, vec![call]))
}

fn collab_starts(item: &Value) -> Option<Starts> {
    let (name, arguments) = agents::collab_call(item)?;
    let id = item["id"].as_str()?.to_owned();
    Some((id.clone(), vec![(name, arguments, id)]))
}

fn other_starts(kind: &str, item: &Value) -> Option<Starts> {
    let other = Translator::parse::<OtherItem>(item.clone(), kind)?;
    let (name, arguments, _, _) = items::other_call(kind, &other)?;
    Some((other.id.clone(), vec![(name, arguments, other.id)]))
}

pub fn tool_dones(kind: &str, item: &Value) -> Option<(String, Vec<ToolDone>)> {
    match kind {
        "commandExecution" => command_dones(item),
        "fileChange" => file_dones(item),
        "mcpToolCall" => mcp_dones(item),
        "collabAgentToolCall" => collab_dones(item),
        _ => other_dones(kind, item),
    }
}

fn command_dones(item: &Value) -> Option<(String, Vec<ToolDone>)> {
    let item = Translator::parse::<CommandItem>(item.clone(), "commandExecution")?;
    let arguments = json!({"command": items::command_text(&item.command, &item.command_actions)});
    let (output, is_error) = items::command_result(&item);
    let done = ToolDone {
        id: item.id.clone(),
        name: "bash".into(),
        arguments,
        output,
        is_error,
    };
    Some((item.id, vec![done]))
}

fn file_dones(item: &Value) -> Option<(String, Vec<ToolDone>)> {
    let item = Translator::parse::<FileChangeItem>(item.clone(), "fileChange")?;
    let failed = matches!(item.status.as_str(), "failed" | "declined");
    let dones = items::file_calls(&item.id, &item.changes)
        .into_iter()
        .map(|call| {
            let text = match item.status.as_str() {
                "declined" => "The change was declined".to_owned(),
                "failed" => "The change failed".to_owned(),
                _ => call.summary,
            };
            ToolDone {
                id: call.id,
                name: call.name.to_owned(),
                arguments: call.arguments,
                output: items::tool_output_text(&text, failed),
                is_error: failed,
            }
        })
        .collect();
    Some((item.id, dones))
}

fn mcp_dones(item: &Value) -> Option<(String, Vec<ToolDone>)> {
    let item = Translator::parse::<McpItem>(item.clone(), "mcpToolCall")?;
    let (output, is_error) = items::mcp_result(&item);
    let done = ToolDone {
        id: item.id.clone(),
        name: items::mcp_name(&item),
        arguments: item.arguments.clone(),
        output,
        is_error,
    };
    Some((item.id, vec![done]))
}

fn collab_dones(item: &Value) -> Option<(String, Vec<ToolDone>)> {
    let (name, arguments) = agents::collab_call(item)?;
    let id = item["id"].as_str()?.to_owned();
    let (text, is_error) = agents::collab_result(item);
    let done = ToolDone {
        id: id.clone(),
        name,
        arguments,
        output: items::tool_output_text(&text, is_error),
        is_error,
    };
    Some((id, vec![done]))
}

fn other_dones(kind: &str, item: &Value) -> Option<(String, Vec<ToolDone>)> {
    let other = Translator::parse::<OtherItem>(item.clone(), kind)?;
    let (name, arguments, text, failed) = items::other_call(kind, &other)?;
    let done = ToolDone {
        id: other.id.clone(),
        name,
        arguments,
        output: items::tool_output_text(&text, failed),
        is_error: failed,
    };
    Some((other.id, vec![done]))
}
