//! Translates ACP stdio JSON-RPC frames (`session/update`, `session/prompt` replies, and
//! interactive server requests) into `BackendEvent`s in FIFO order.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tiechui_protocol::{
    ApprovalRequest, CommandInfo, ItemId, KnownItem, SubagentState, TurnStatus,
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::debug;
use super::dialect::{AcpDialect, AcpSubagent, TextDisposition, ToolState};
use super::items::{map_commands, map_usage, normalized_call, normalized_result, plan_summary};
use super::requests::{
    PendingApproval, Responder, build_elicitation_approval, build_permission_approval,
};
use super::wire::{
    AcpUsageWire, AvailableCommandWire, PromptResponseWire, SessionNotificationWire,
};
use crate::backend::{BackendEvent, BackendItem, SubagentUpdate};
use crate::process::{DeltaCoalescer, Inbound, StderrTail, stderr_detail};
use crate::rpc::{Incoming, Rpc, RpcError};
use crate::tool_output;

const FLUSH_INTERVAL: Duration = Duration::from_millis(50);
const FLUSH_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct PromptFlight {
    pub rpc_id: i64,
    pub cancel_requested: bool,
}

pub struct Shared {
    session: Mutex<Option<String>>,
    prompt: Mutex<Option<PromptFlight>>,
    replaying_load: AtomicBool,
    saw_frame: AtomicBool,
    approvals: Mutex<HashMap<String, PendingApproval>>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            session: Mutex::new(None),
            prompt: Mutex::new(None),
            replaying_load: AtomicBool::new(false),
            saw_frame: AtomicBool::new(false),
            approvals: Mutex::default(),
        }
    }
}

impl Shared {
    pub fn mark_frame(&self) {
        self.saw_frame.store(true, Ordering::Release);
    }

    pub fn saw_frame(&self) -> bool {
        self.saw_frame.load(Ordering::Acquire)
    }

    pub fn set_session(&self, id: String) {
        *self.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
    }

    pub fn session(&self) -> Option<String> {
        self.session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_replaying_load(&self, replaying: bool) {
        self.replaying_load.store(replaying, Ordering::Release);
    }

    pub fn is_replaying_load(&self) -> bool {
        self.replaying_load.load(Ordering::Acquire)
    }

    pub fn begin_prompt(&self, rpc_id: i64) {
        self.set_replaying_load(false);
        *self.prompt.lock().unwrap_or_else(|e| e.into_inner()) = Some(PromptFlight {
            rpc_id,
            cancel_requested: false,
        });
    }

    pub fn clear_prompt_if(&self, rpc_id: i64) -> Option<PromptFlight> {
        let mut guard = self.prompt.lock().unwrap_or_else(|e| e.into_inner());
        match *guard {
            Some(flight) if flight.rpc_id == rpc_id => guard.take(),
            _ => None,
        }
    }

    pub fn mark_cancel(&self) -> bool {
        let mut guard = self.prompt.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(flight) = guard.as_mut() {
            flight.cancel_requested = true;
            return true;
        }
        false
    }

    pub fn has_active_prompt(&self) -> bool {
        self.prompt
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    pub fn insert_approval(&self, id: String, pending: PendingApproval) {
        self.approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, pending);
    }

    pub fn take_approval(&self, id: &str) -> Option<PendingApproval> {
        self.approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
    }

    pub fn drain_approvals(&self) -> Vec<(String, PendingApproval)> {
        std::mem::take(&mut *self.approvals.lock().unwrap_or_else(|e| e.into_inner()))
            .into_iter()
            .collect()
    }

    pub fn approvals_empty(&self) -> bool {
        self.approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

struct StreamSegment {
    id: String,
    key: Option<String>,
    thinking: bool,
    text: String,
}

struct OpenPlan {
    id: String,
    markdown: String,
    arguments: Value,
}

pub enum Control {
    SealDerived(oneshot::Sender<()>),
    CancelDerived,
}

pub struct AcpTranslator {
    shared: Arc<Shared>,
    rpc: Arc<Rpc>,
    dialect: Arc<dyn AcpDialect>,
    out: mpsc::Sender<BackendEvent>,
    stderr: StderrTail,
    wakeup_idle: Duration,
    coalescer: DeltaCoalescer,
    buffered: usize,
    flush_at: Option<Instant>,
    idle_at: Option<Instant>,
    seq: u64,
    segment: Option<StreamSegment>,
    plan: Option<OpenPlan>,
    tools: HashMap<String, ToolState>,
    open_tools: HashSet<String>,
    active_subagents: HashSet<String>,
    last_commands: Option<Vec<CommandInfo>>,
    context_window: Option<u64>,
    turn_failure: Option<String>,
    outside_turn_active: bool,
}

async fn sleep_opt(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

enum Step {
    Inbound(Option<Inbound>),
    Control(Option<Control>),
    Flush,
    Idle,
}

impl AcpTranslator {
    pub fn new(
        shared: Arc<Shared>,
        rpc: Arc<Rpc>,
        dialect: Arc<dyn AcpDialect>,
        out: mpsc::Sender<BackendEvent>,
        stderr: StderrTail,
        wakeup_idle: Duration,
    ) -> Self {
        Self {
            shared,
            rpc,
            dialect,
            out,
            stderr,
            wakeup_idle,
            coalescer: DeltaCoalescer::default(),
            buffered: 0,
            flush_at: None,
            idle_at: None,
            seq: 0,
            segment: None,
            plan: None,
            tools: HashMap::new(),
            open_tools: HashSet::new(),
            active_subagents: HashSet::new(),
            last_commands: None,
            context_window: None,
            turn_failure: None,
            outside_turn_active: false,
        }
    }

    pub async fn run(
        mut self,
        mut inbound: mpsc::Receiver<Inbound>,
        mut control: mpsc::Receiver<Control>,
    ) {
        loop {
            let step = tokio::select! {
                biased;
                ctrl = control.recv() => Step::Control(ctrl),
                msg = inbound.recv() => Step::Inbound(msg),
                () = sleep_opt(self.flush_at) => Step::Flush,
                () = sleep_opt(self.idle_at) => Step::Idle,
            };
            if !self.handle_step(step).await {
                break;
            }
        }
        self.rpc.close();
    }

    async fn handle_step(&mut self, step: Step) -> bool {
        match step {
            Step::Control(Some(ctrl)) => self.on_control(ctrl).await,
            Step::Control(None) => true,
            Step::Inbound(Some(Inbound::Frame(frame))) => self.on_frame(&frame).await,
            Step::Inbound(Some(Inbound::Oversize(size))) => {
                debug(&format!("dropped an oversize ACP frame of {size} bytes"));
                true
            }
            Step::Inbound(Some(Inbound::Eof) | None) => {
                self.on_eof().await;
                false
            }
            Step::Flush => self.flush().await,
            Step::Idle => self.on_idle_timer().await,
        }
    }

    async fn on_control(&mut self, ctrl: Control) -> bool {
        match ctrl {
            Control::SealDerived(done) => {
                let ok = self.seal_wakeup_items().await;
                self.outside_turn_active = false;
                self.idle_at = None;
                let _ = done.send(());
                ok
            }
            Control::CancelDerived => {
                if !self.shared.has_active_prompt() && self.outside_turn_active {
                    self.outside_turn_active = false;
                    self.idle_at = None;
                    return self.seal_wakeup_items().await && self.emit(BackendEvent::Idle).await;
                }
                true
            }
        }
    }

    async fn on_eof(&mut self) {
        let _ = self.flush().await;
        tokio::task::yield_now().await;
        let detail = stderr_detail(&self.stderr);
        let _ = self
            .send_raw(BackendEvent::Error(format!(
                "{} exited{detail}",
                self.dialect.provider_id()
            )))
            .await;
    }

    fn next_item_id(&mut self, prefix: &str) -> String {
        self.seq += 1;
        format!("acp-{prefix}-{}", self.seq)
    }

    async fn send_raw(&self, event: BackendEvent) -> bool {
        self.out.send(event).await.is_ok()
    }

    async fn flush(&mut self) -> bool {
        self.flush_at = None;
        self.buffered = 0;
        for (item_id, delta) in self.coalescer.drain() {
            if !self
                .send_raw(BackendEvent::ItemDelta { item_id, delta })
                .await
            {
                return false;
            }
        }
        true
    }

    async fn emit(&mut self, event: BackendEvent) -> bool {
        self.flush().await && self.send_raw(event).await
    }

    fn push_delta(&mut self, id: &str, delta: &str) {
        self.coalescer.push(&ItemId(id.to_owned()), delta);
        self.buffered += delta.len();
        if self.flush_at.is_none() && !delta.is_empty() {
            self.flush_at = Some(Instant::now() + FLUSH_INTERVAL);
        }
    }

    fn note_wakeup_activity(&mut self) {
        if !self.shared.has_active_prompt() {
            self.outside_turn_active = true;
            self.refresh_idle_deadline();
        } else {
            self.idle_at = None;
        }
    }

    pub fn refresh_idle_deadline(&mut self) {
        let settled = self.outside_turn_active
            && !self.shared.has_active_prompt()
            && self.open_tools.is_empty()
            && self.active_subagents.is_empty()
            && self.shared.approvals_empty();
        self.idle_at = settled.then(|| Instant::now() + self.wakeup_idle);
    }

    async fn on_idle_timer(&mut self) -> bool {
        self.idle_at = None;
        if !self.outside_turn_active || self.shared.has_active_prompt() {
            return true;
        }
        if !self.open_tools.is_empty()
            || !self.active_subagents.is_empty()
            || !self.shared.approvals_empty()
        {
            return true;
        }
        self.outside_turn_active = false;
        self.seal_wakeup_items().await && self.emit(BackendEvent::Idle).await
    }

    async fn seal_wakeup_items(&mut self) -> bool {
        self.close_segment().await && self.close_plan().await
    }

    async fn on_frame(&mut self, frame: &[u8]) -> bool {
        self.shared.mark_frame();
        let incoming: Incoming = match serde_json::from_slice(frame) {
            Ok(parsed) => parsed,
            Err(error) => {
                debug(&format!("dropped non-JSON-RPC line: {error}"));
                return true;
            }
        };
        match (incoming.id, incoming.method) {
            (Some(id), Some(method)) => self.on_server_request(id, &method, incoming.params).await,
            (Some(id), None) => self.on_response(id, incoming.result, incoming.error).await,
            (None, Some(method)) => self.on_notification(&method, incoming.params).await,
            (None, None) => true,
        }
    }

    async fn on_response(
        &mut self,
        id: Value,
        result: Option<Value>,
        error: Option<RpcError>,
    ) -> bool {
        let numeric_id = id.as_i64().or_else(|| id.as_str()?.parse().ok());
        if let Some(rpc_id) = numeric_id
            && let Some(flight) = self.shared.clear_prompt_if(rpc_id)
        {
            return self.on_prompt_finished(flight, result, error).await;
        }
        let outcome = match (result, error) {
            (_, Some(err)) => Err(err),
            (Some(val), None) => Ok(val),
            (None, None) => Ok(Value::Null),
        };
        self.rpc.resolve(&id, outcome);
        self.refresh_idle_deadline();
        true
    }

    async fn on_prompt_finished(
        &mut self,
        flight: PromptFlight,
        result: Option<Value>,
        error: Option<RpcError>,
    ) -> bool {
        self.idle_at = None;
        self.outside_turn_active = false;
        let (status, turn_err, usage) = self.classify_prompt_end(flight, result, error);
        if !self.close_segment().await || !self.close_plan().await {
            return false;
        }
        if !self.finish_turn_tools(status).await || !self.withdraw_all_approvals().await {
            return false;
        }
        if let Some(usage) = usage
            && !self.emit(BackendEvent::Usage(map_usage(&usage))).await
        {
            return false;
        }
        self.emit(BackendEvent::TurnCompleted {
            status,
            error: turn_err,
        })
        .await
    }

    fn classify_prompt_end(
        &mut self,
        flight: PromptFlight,
        result: Option<Value>,
        error: Option<RpcError>,
    ) -> (TurnStatus, Option<String>, Option<AcpUsageWire>) {
        let notice_failure = self.turn_failure.take();
        if let Some(err) = error {
            return self.classify_prompt_rpc_error(flight, err);
        }
        let parsed: PromptResponseWire = result
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let usage = parsed.usage.or_else(|| {
            parsed
                .meta
                .as_ref()
                .and_then(|m| serde_json::from_value(m.get("usage")?.clone()).ok())
        });
        if let Some(failure) = notice_failure {
            return (TurnStatus::Failed, Some(failure), usage);
        }
        let (status, reason) = stop_reason_status(&parsed.stop_reason, flight.cancel_requested);
        (status, reason, usage)
    }

    fn classify_prompt_rpc_error(
        &self,
        flight: PromptFlight,
        err: RpcError,
    ) -> (TurnStatus, Option<String>, Option<AcpUsageWire>) {
        let lower = err.message.to_ascii_lowercase();
        if flight.cancel_requested
            && (err.code == -32800 || lower.contains("cancel") || lower.contains("abort"))
        {
            return (TurnStatus::Interrupted, None, None);
        }
        let formatted = self
            .dialect
            .format_error(err.code, &err.message, err.data.as_ref());
        (TurnStatus::Failed, Some(formatted), None)
    }

    async fn finish_turn_tools(&mut self, status: TurnStatus) -> bool {
        let ids: Vec<String> = self.open_tools.drain().collect();
        let background = status == TurnStatus::Completed && self.dialect.reactive_wakeups();
        for id in ids {
            let Some(state) = self.tools.get(&id).cloned() else {
                continue;
            };
            let (name, args) = normalized_call(&state);
            let (msg, is_err) = if background {
                ("Running in background", false)
            } else {
                ("The turn ended before this finished", true)
            };
            let out = tool_output(msg, is_err, Value::Null);
            if !self
                .emit_completed_tool(&id, &name, args, out, is_err)
                .await
            {
                return false;
            }
        }
        true
    }

    async fn withdraw_all_approvals(&mut self) -> bool {
        for (id, pending) in self.shared.drain_approvals() {
            let _ = self
                .rpc
                .respond(
                    &pending.rpc_id,
                    pending
                        .responder
                        .into_response(&tiechui_protocol::ApprovalDecision::Cancel),
                )
                .await;
            if !self.emit(BackendEvent::ApprovalWithdrawn { id }).await {
                return false;
            }
        }
        true
    }

    async fn on_notification(&mut self, method: &str, params: Value) -> bool {
        if method != "session/update" {
            return true;
        }
        let Ok(note) = serde_json::from_value::<SessionNotificationWire>(params) else {
            return true;
        };
        if !self.matches_session(&note.session_id) {
            return true;
        }
        let kind = note.update["sessionUpdate"].as_str().unwrap_or_default();
        if is_replay(&note) || (self.shared.is_replaying_load() && is_timeline_update(kind)) {
            return true;
        }
        self.dispatch_session_update(kind, &note.update).await
    }

    fn matches_session(&self, session_id: &str) -> bool {
        match self.shared.session() {
            Some(expected) => expected == session_id,
            None => true,
        }
    }

    async fn dispatch_session_update(&mut self, kind: &str, update: &Value) -> bool {
        match kind {
            "agent_message_chunk" | "agent_thought_chunk" => {
                self.on_text_chunk(kind == "agent_thought_chunk", update)
                    .await
            }
            "tool_call" | "tool_call_update" => self.on_tool_update(update).await,
            "plan" | "plan_update" => self.on_plan_update(kind, update).await,
            "available_commands_update" => self.on_commands_update(update).await,
            "usage_update" => self.on_usage_update(update).await,
            _ => true,
        }
    }

    async fn on_text_chunk(&mut self, thinking: bool, update: &Value) -> bool {
        if update["content"]["type"].as_str() != Some("text") {
            return true;
        }
        let delta = update["content"]["text"].as_str().unwrap_or_default();
        if delta.is_empty() {
            return true;
        }
        match self.dialect.on_text_chunk(thinking, delta) {
            TextDisposition::Ignore => return true,
            TextDisposition::FailTurn(reason) => {
                self.turn_failure = Some(reason);
                return true;
            }
            TextDisposition::Keep => {}
        }
        if !self.dialect.reactive_wakeups() && !self.shared.has_active_prompt() {
            return true;
        }
        self.note_wakeup_activity();
        let key = update["messageId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        self.append_stream_delta(thinking, key, delta).await
    }

    async fn append_stream_delta(
        &mut self,
        thinking: bool,
        key: Option<String>,
        delta: &str,
    ) -> bool {
        let same_segment = self
            .segment
            .as_ref()
            .is_some_and(|seg| seg.thinking == thinking && seg.key == key);
        if !same_segment && !self.close_segment().await {
            return false;
        }
        let id = match &mut self.segment {
            Some(seg) => {
                seg.text.push_str(delta);
                seg.id.clone()
            }
            None => {
                let id = self.next_item_id(if thinking { "thought" } else { "msg" });
                let item = make_text_item(&id, thinking, String::new());
                if !self
                    .emit(BackendEvent::ItemStarted(BackendItem::root(item)))
                    .await
                {
                    return false;
                }
                self.segment = Some(StreamSegment {
                    id: id.clone(),
                    key,
                    thinking,
                    text: delta.to_owned(),
                });
                id
            }
        };
        self.push_delta(&id, delta);
        if self.buffered >= FLUSH_BYTES {
            return self.flush().await;
        }
        true
    }

    async fn close_segment(&mut self) -> bool {
        let Some(seg) = self.segment.take() else {
            return true;
        };
        let item = make_text_item(&seg.id, seg.thinking, seg.text);
        self.emit(BackendEvent::ItemCompleted(BackendItem::root(item)))
            .await
    }

    async fn on_tool_update(&mut self, update: &Value) -> bool {
        if !self.dialect.reactive_wakeups() && !self.shared.has_active_prompt() {
            return true;
        }
        let Some(call_id) = update["toolCallId"].as_str().filter(|s| !s.is_empty()) else {
            return true;
        };
        if !self.close_segment().await {
            return false;
        }
        self.note_wakeup_activity();
        let mut state = self.tools.remove(call_id).unwrap_or_default();
        state.merge_from(update);
        self.dialect.normalize_tool(&mut state);
        let subagent = self.dialect.tool_subagent(&state);
        let terminal = state.is_terminal();
        let (name, args) = normalized_call(&state);
        let ok = if terminal {
            self.open_tools.remove(call_id);
            let (out, is_err) = normalized_result(&name, &state);
            self.tools.insert(call_id.to_owned(), state);
            self.emit_completed_tool(call_id, &name, args, out, is_err)
                .await
        } else {
            self.open_tools.insert(call_id.to_owned());
            self.tools.insert(call_id.to_owned(), state);
            self.emit(BackendEvent::ItemStarted(BackendItem::root(
                KnownItem::ToolCall {
                    id: ItemId(call_id.to_owned()),
                    name,
                    arguments: args,
                },
            )))
            .await
        };
        if !ok {
            return false;
        }
        let ok = match subagent {
            Some(sub) => self.emit_subagent(call_id, sub).await,
            None => true,
        };
        self.refresh_idle_deadline();
        ok
    }

    async fn emit_subagent(&mut self, call_id: &str, sub: AcpSubagent) -> bool {
        if matches!(sub.state, SubagentState::Running | SubagentState::Waiting) {
            self.active_subagents.insert(sub.agent_id.clone());
        } else {
            self.active_subagents.remove(&sub.agent_id);
        }
        self.emit(BackendEvent::Subagent(SubagentUpdate {
            agent_id: sub.agent_id,
            parent_call_id: ItemId(call_id.to_owned()),
            name: sub.name,
            state: sub.state,
            message: sub.message,
        }))
        .await
    }

    async fn emit_completed_tool(
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
                .send_raw(BackendEvent::ItemCompleted(BackendItem::root(
                    KnownItem::ToolResult {
                        id: ItemId(format!("{id}:result")),
                        call_id: call,
                        output,
                        is_error,
                    },
                )))
                .await
    }

    async fn on_plan_update(&mut self, kind: &str, update: &Value) -> bool {
        let Some((markdown, arguments, all_done)) = plan_summary(kind, update) else {
            return true;
        };
        if !self.close_segment().await {
            return false;
        }
        self.note_wakeup_activity();
        let id = match &self.plan {
            Some(open) => open.id.clone(),
            None => self.next_item_id("plan"),
        };
        self.plan = Some(OpenPlan {
            id: id.clone(),
            markdown,
            arguments: arguments.clone(),
        });
        if all_done {
            return self.close_plan().await;
        }
        self.emit(BackendEvent::ItemStarted(BackendItem::root(
            KnownItem::ToolCall {
                id: ItemId(id),
                name: "plan".into(),
                arguments,
            },
        )))
        .await
    }

    async fn close_plan(&mut self) -> bool {
        let Some(plan) = self.plan.take() else {
            return true;
        };
        let out = tool_output(&plan.markdown, false, Value::Null);
        self.emit_completed_tool(&plan.id, "plan", plan.arguments, out, false)
            .await
    }

    async fn on_commands_update(&mut self, update: &Value) -> bool {
        let Ok(raw) = serde_json::from_value::<Vec<AvailableCommandWire>>(
            update["availableCommands"].clone(),
        ) else {
            return true;
        };
        let commands = map_commands(&raw, self.dialect.as_ref());
        if self.last_commands.as_ref() == Some(&commands) {
            return true;
        }
        self.last_commands = Some(commands.clone());
        self.emit(BackendEvent::Commands(commands)).await
    }

    async fn on_usage_update(&mut self, update: &Value) -> bool {
        let Some(size) = update["size"].as_u64().filter(|&s| s > 0) else {
            return true;
        };
        if self.context_window == Some(size) {
            return true;
        }
        self.context_window = Some(size);
        self.emit(BackendEvent::ContextWindow(size)).await
    }

    async fn on_server_request(&mut self, id: Value, method: &str, params: Value) -> bool {
        if let Some(session_id) = params["sessionId"].as_str()
            && !self.matches_session(session_id)
        {
            let _ = self
                .rpc
                .reject(&id, -32602, "request belongs to an unknown ACP session")
                .await;
            return true;
        }
        if let Some(custom) = self.dialect.intercept_request(method, &params) {
            return self
                .open_approval(id, custom.request, Responder::Custom(custom.respond))
                .await;
        }
        match method {
            "session/request_permission" => self.on_permission_request(id, &params).await,
            "elicitation/create" => self.on_elicitation_request(id, &params).await,
            _ => {
                let _ = self
                    .rpc
                    .reject(&id, -32601, "method not supported by tiechui")
                    .await;
                true
            }
        }
    }

    async fn on_permission_request(&mut self, id: Value, params: &Value) -> bool {
        let call_id = params["toolCall"]["toolCallId"]
            .as_str()
            .unwrap_or_default();
        let existing = self.tools.get(call_id);
        let Some((state, request, responder)) =
            build_permission_approval(params, existing, self.dialect.as_ref())
        else {
            let _ = self
                .rpc
                .respond(&id, json!({ "outcome": { "outcome": "cancelled" } }))
                .await;
            return true;
        };
        if !self.open_tools.contains(&state.id) {
            let (name, args) = normalized_call(&state);
            let started = KnownItem::ToolCall {
                id: ItemId(state.id.clone()),
                name,
                arguments: args,
            };
            self.open_tools.insert(state.id.clone());
            self.tools.insert(state.id.clone(), state);
            if !self
                .emit(BackendEvent::ItemStarted(BackendItem::root(started)))
                .await
            {
                return false;
            }
        }
        self.open_approval(id, request, responder).await
    }

    async fn on_elicitation_request(&mut self, id: Value, params: &Value) -> bool {
        let Some((request, responder)) =
            build_elicitation_approval(params, self.dialect.provider_id())
        else {
            let _ = self.rpc.respond(&id, json!({ "action": "decline" })).await;
            return true;
        };
        self.open_approval(id, request, responder).await
    }

    async fn open_approval(
        &mut self,
        rpc_id: Value,
        request: ApprovalRequest,
        responder: Responder,
    ) -> bool {
        if !self.close_segment().await {
            return false;
        }
        self.note_wakeup_activity();
        let approval_id = format_approval_id(&rpc_id);
        self.shared
            .insert_approval(approval_id.clone(), PendingApproval { rpc_id, responder });
        self.idle_at = None;
        self.emit(BackendEvent::Approval {
            id: approval_id,
            request,
        })
        .await
    }
}

fn format_approval_id(rpc_id: &Value) -> String {
    match rpc_id.as_str() {
        Some(text) => format!("acp-{text}"),
        None => format!("acp-{rpc_id}"),
    }
}

fn make_text_item(id: &str, thinking: bool, text: String) -> KnownItem {
    let id = ItemId(id.to_owned());
    if thinking {
        KnownItem::Thinking { id, text }
    } else {
        KnownItem::AssistantMessage { id, text }
    }
}

fn is_replay(note: &SessionNotificationWire) -> bool {
    note.meta.as_ref().is_some_and(|m| m["isReplay"] == true)
        || note.update["_meta"]["isReplay"] == true
}

fn is_timeline_update(kind: &str) -> bool {
    !matches!(
        kind,
        "available_commands_update"
            | "usage_update"
            | "current_mode_update"
            | "config_option_update"
            | "session_info_update"
    )
}

fn stop_reason_status(stop_reason: &str, cancel_requested: bool) -> (TurnStatus, Option<String>) {
    match stop_reason {
        "cancelled" => (TurnStatus::Interrupted, None),
        "end_turn" | "max_tokens" | "max_turn_requests" | "" => (TurnStatus::Completed, None),
        _ if cancel_requested => (TurnStatus::Interrupted, None),
        "refusal" => (
            TurnStatus::Failed,
            Some("The agent refused the request".into()),
        ),
        other => (
            TurnStatus::Failed,
            Some(format!("ACP turn stopped: {other}")),
        ),
    }
}
