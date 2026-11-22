mod agents;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tiechui_protocol::{ItemId, KnownItem, TurnStatus};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::debug;
use super::items;
use super::items::Pending;
use super::run::{Handshake, Inbound, Shared, StderrTail, SupervisorCommand, Writer};
use super::wire::{Block, Incoming, Request, StreamEvent, control_error};
use crate::backend::{BackendEvent, BackendItem};
use crate::process::DeltaCoalescer;
use agents::{Agent, ChildCall};

const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

pub type CountedMessageIds = Arc<Mutex<HashSet<String>>>;

struct StreamingBlock {
    id: ItemId,
    thinking: bool,
    started: bool,
    text: String,
}

struct UnresolvedCall {
    id: String,
    native_name: String,
    input: Value,
}

pub struct Translator {
    shared: Arc<Shared>,
    writer: Writer,
    out: mpsc::Sender<BackendEvent>,
    counted: CountedMessageIds,
    reap_grace: Duration,
    coalescer: DeltaCoalescer,
    flush_at: Option<Instant>,
    streaming_message_id: String,
    blocks: HashMap<u64, StreamingBlock>,
    unresolved_calls: Vec<UnresolvedCall>,
    announced: HashSet<String>,
    context_window: u64,
    stderr: StderrTail,
    agents: HashMap<String, Agent>,
    child_calls: HashMap<String, ChildCall>,
    next_child_block_index: HashMap<String, u32>,
    outside_turn_output_seen: bool,
}

impl Translator {
    pub fn new(
        shared: Arc<Shared>,
        writer: Writer,
        out: mpsc::Sender<BackendEvent>,
        counted: CountedMessageIds,
        reap_grace: Duration,
        stderr: StderrTail,
    ) -> Self {
        Self {
            shared,
            writer,
            out,
            counted,
            reap_grace,
            coalescer: DeltaCoalescer::default(),
            flush_at: None,
            streaming_message_id: String::new(),
            blocks: HashMap::new(),
            unresolved_calls: Vec::new(),
            announced: HashSet::new(),
            context_window: 0,
            stderr,
            agents: HashMap::new(),
            child_calls: HashMap::new(),
            next_child_block_index: HashMap::new(),
            outside_turn_output_seen: false,
        }
    }

    pub async fn run(
        mut self,
        mut inbound: mpsc::Receiver<Inbound>,
        mut answered: mpsc::UnboundedReceiver<String>,
    ) {
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
                Some(agent) = answered.recv() => self.approval_settled(&agent).await,
                () = wake => self.flush().await,
            };
            if !alive {
                break;
            }
        }
    }

    fn stderr_detail(&self) -> String {
        let tail: Vec<String> = self
            .stderr
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        if tail.is_empty() {
            String::new()
        } else {
            format!(": {}", tail.join(" | "))
        }
    }

    async fn exited(&mut self) {
        self.shared.usable.store(false, Ordering::SeqCst);
        let detail = self.stderr_detail();
        if self
            .shared
            .try_finish_handshake(Handshake::Failed(format!("claude exited{detail}")))
        {
            return;
        }
        self.flush().await;
        self.shared.claude_turn.store(false, Ordering::SeqCst);
        if !self.withdraw_approvals_where(|_| true).await
            || !self.finish_open_items().await
            || !self.interrupt_agents().await
        {
            return;
        }
        if self.shared.turn_active.swap(false, Ordering::SeqCst) {
            self.send_now(BackendEvent::TurnCompleted {
                status: TurnStatus::Failed,
                error: Some(format!("claude exited{detail}")),
            })
            .await;
        } else {
            self.maybe_idle().await;
        }
    }

    async fn flush(&mut self) -> bool {
        self.flush_at = None;
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

    async fn emit_after_flush(&mut self, event: BackendEvent) -> bool {
        self.flush().await && self.send_now(event).await
    }

    async fn frame(&mut self, frame: &[u8]) -> bool {
        let incoming: Incoming = match serde_json::from_slice(frame) {
            Ok(incoming) => incoming,
            Err(error) => {
                debug(&format!("dropped a frame that is not stream-json: {error}"));
                return true;
            }
        };
        match incoming.kind.as_str() {
            "control_request" => self.control_request(incoming).await,
            "control_cancel_request" => self.cancel_request(incoming).await,
            "control_response" => self.control_response(incoming),
            "result" => self.result(incoming).await,
            "system" => self.system(incoming).await,
            "stream_event" if incoming.parent_tool_use_id.is_some() => true,
            "assistant" | "user" if incoming.parent_tool_use_id.is_some() => {
                self.child_frame(incoming).await
            }
            "stream_event" => self.stream_event(incoming).await,
            "assistant" => self.assistant(incoming).await,
            "user" => self.user(incoming).await,
            _ => true,
        }
    }

    async fn system(&mut self, incoming: Incoming) -> bool {
        match incoming.subtype.as_deref() {
            // Every turn begins with `init`; one while no turn of the session runs is Claude's own.
            Some("init") if !self.shared.turn_active() => {
                self.shared.claude_turn.store(true, Ordering::SeqCst);
                self.outside_turn_output_seen = true;
                true
            }
            Some("task_started" | "task_notification") => self.task(incoming).await,
            _ => true,
        }
    }

    fn control_response(&mut self, incoming: Incoming) -> bool {
        let Some(response) = incoming.response else {
            return true;
        };
        if response.request_id.as_deref() == Some(self.shared.initialize_request_id.as_str()) {
            self.shared
                .try_finish_handshake(if response.subtype == "success" {
                    Handshake::Ready
                } else {
                    Handshake::Failed(response.error.unwrap_or_else(|| "initialize failed".into()))
                });
        }
        true
    }

    /// Claude waits on every control request until it is answered, so a request nothing maps is
    /// answered with an error at once.
    async fn control_request(&mut self, incoming: Incoming) -> bool {
        let (Some(id), Some(request)) = (incoming.request_id, incoming.request) else {
            return true;
        };
        if request.subtype == "can_use_tool" {
            return self.permission_request(&id, &request).await;
        }
        let reply = control_error(
            &id,
            &format!("unsupported control request: {}", request.subtype),
        );
        if self.writer.send(&reply).is_err() {
            debug("cannot answer a control request: claude is not taking input");
        }
        true
    }

    async fn permission_request(&mut self, request_id: &str, request: &Request) -> bool {
        let mut card_input = request.input.clone();
        let tool = request.tool_name.as_deref().unwrap_or_default();
        let call_id = match (&request.tool_use_id, &request.agent_id) {
            (Some(call), Some(agent)) => {
                if !self
                    .announce_child_once(agent, call, tool, &request.input)
                    .await
                {
                    return false;
                }
                agents::subagent_item_id(agent, call)
            }
            (Some(call), None) => {
                if !self.announce_once(call, tool, &request.input).await {
                    return false;
                }
                // `ExitPlanMode` asks with an empty input: the plan is only in its `tool_use` block.
                if card_input
                    .as_object()
                    .is_some_and(serde_json::Map::is_empty)
                    && let Some(unresolved) = self
                        .unresolved_calls
                        .iter()
                        .find(|unresolved| unresolved.id == *call)
                {
                    card_input = unresolved.input.clone();
                }
                call.clone()
            }
            (None, _) => request_id.to_owned(),
        };
        let (pending, approval) = items::ask(request_id, request, &card_input, &call_id);
        let id = approval_id(request_id);
        self.shared.approvals().insert(id.clone(), pending);
        self.note_outside_turn_output();
        if !self
            .emit_after_flush(BackendEvent::Approval {
                id,
                request: approval,
            })
            .await
        {
            return false;
        }
        match &request.agent_id {
            Some(agent) => self.agent_waits(agent).await,
            None => true,
        }
    }

    async fn cancel_request(&mut self, incoming: Incoming) -> bool {
        let Some(request_id) = incoming.request_id else {
            return true;
        };
        let id = approval_id(&request_id);
        let Some(pending) = self.shared.take_approval(&id) else {
            return true;
        };
        if !self
            .emit_after_flush(BackendEvent::ApprovalWithdrawn { id })
            .await
        {
            return false;
        }
        match pending.subagent_id {
            Some(agent) => self.approval_settled(&agent).await,
            None => true,
        }
    }

    async fn withdraw_approvals_where(&mut self, done: impl Fn(&Pending) -> bool) -> bool {
        let mut ids = vec![];
        self.shared.approvals().retain(|id, pending| {
            let withdraw = done(pending);
            if withdraw {
                ids.push(id.clone());
            }
            !withdraw
        });
        for id in ids {
            if !self
                .emit_after_flush(BackendEvent::ApprovalWithdrawn { id })
                .await
            {
                return false;
            }
        }
        true
    }

    fn message_id(&self, incoming: &Incoming) -> String {
        match &incoming.event {
            Some(StreamEvent {
                message: Some(message),
                ..
            }) if !message.id.is_empty() => message.id.clone(),
            _ => incoming
                .api_message_id
                .clone()
                .unwrap_or_else(|| self.streaming_message_id.clone()),
        }
    }

    async fn stream_event(&mut self, incoming: Incoming) -> bool {
        let id = self.message_id(&incoming);
        let Some(event) = incoming.event else {
            return true;
        };
        match event.kind.as_str() {
            "message_start" => {
                self.streaming_message_id = id;
                true
            }
            "content_block_start" => self.block_start(&event).await,
            "content_block_delta" => self.block_delta(&event).await,
            "content_block_stop" => self.block_stop(event.index).await,
            "message_delta" => self.message_usage(&id, &event).await,
            _ => true,
        }
    }

    async fn block_start(&mut self, event: &StreamEvent) -> bool {
        let thinking = match event
            .content_block
            .as_ref()
            .map(|block| block.kind.as_str())
        {
            Some("text") => false,
            Some("thinking") => true,
            _ => return true,
        };
        self.blocks.insert(
            event.index,
            StreamingBlock {
                id: ItemId(format!("{}:{}", self.streaming_message_id, event.index)),
                thinking,
                started: false,
                text: String::new(),
            },
        );
        true
    }

    async fn block_delta(&mut self, event: &StreamEvent) -> bool {
        let Some(delta) = &event.delta else {
            return true;
        };
        let text = match delta.kind.as_str() {
            "text_delta" => &delta.text,
            "thinking_delta" => &delta.thinking,
            _ => return true,
        };
        if text.is_empty() {
            return true;
        }
        let Some(open) = self.blocks.get_mut(&event.index) else {
            return true;
        };
        open.text.push_str(text);
        let (id, thinking, started) = (open.id.clone(), open.thinking, open.started);
        open.started = true;
        if !started
            && !self
                .emit_after_flush(BackendEvent::ItemStarted(BackendItem::root(text_item(
                    &id,
                    thinking,
                    String::new(),
                ))))
                .await
        {
            return false;
        }
        self.coalescer.push(&id, text);
        if self.flush_at.is_none() {
            self.flush_at = Some(Instant::now() + FLUSH_INTERVAL);
        }
        true
    }

    async fn block_stop(&mut self, index: u64) -> bool {
        let Some(open) = self.blocks.remove(&index) else {
            return true;
        };
        self.finish_block(open).await
    }

    async fn finish_block(&mut self, open: StreamingBlock) -> bool {
        if !open.started {
            return true;
        }
        self.emit_after_flush(BackendEvent::ItemCompleted(BackendItem::root(text_item(
            &open.id,
            open.thinking,
            open.text,
        ))))
        .await
    }

    async fn message_usage(&mut self, id: &str, event: &StreamEvent) -> bool {
        let Some(usage) = &event.usage else {
            return true;
        };
        if !self.is_new_usage(id) {
            return true;
        }
        self.emit_after_flush(BackendEvent::Usage(items::usage(usage)))
            .await
    }

    fn is_new_usage(&self, id: &str) -> bool {
        let first_report = id.is_empty()
            || self
                .counted
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.to_owned());
        first_report && self.shared.user_message_sent.load(Ordering::SeqCst)
    }

    /// The `assistant` frame carries a tool call's complete input; its stream deltas do not.
    async fn assistant(&mut self, incoming: Incoming) -> bool {
        let Some(message) = incoming.message else {
            return true;
        };
        for block in message.blocks() {
            if block.kind == "tool_use"
                && !self
                    .announce_once(&block.id, &block.name, &block.input)
                    .await
            {
                return false;
            }
        }
        true
    }

    async fn announce_once(&mut self, id: &str, tool: &str, input: &Value) -> bool {
        if !self.announced.insert(id.to_owned()) {
            return true;
        }
        let (name, arguments) = items::native_tool_call(tool, input);
        self.unresolved_calls.push(UnresolvedCall {
            id: id.to_owned(),
            native_name: name.clone(),
            input: input.clone(),
        });
        let call = KnownItem::ToolCall {
            id: ItemId(id.to_owned()),
            name,
            arguments,
        };
        self.emit_after_flush(BackendEvent::ItemCompleted(BackendItem::root(call)))
            .await
    }

    async fn user(&mut self, incoming: Incoming) -> bool {
        let Some(message) = incoming.message else {
            return true;
        };
        for block in message.blocks() {
            if block.kind == "tool_result" && !self.tool_result(&block).await {
                return false;
            }
        }
        true
    }

    async fn tool_result(&mut self, block: &Block) -> bool {
        let Some(position) = self
            .unresolved_calls
            .iter()
            .position(|unresolved| unresolved.id == block.tool_use_id)
        else {
            return true;
        };
        let call = self.unresolved_calls.remove(position);
        let output = items::tool_result(&call.native_name, &block.content, block.is_error);
        self.emit_result(call.id, output, block.is_error).await
    }

    async fn emit_result(&mut self, call: String, output: Value, is_error: bool) -> bool {
        let result = result_item(call, output, is_error);
        self.emit_after_flush(BackendEvent::ItemCompleted(BackendItem::root(result)))
            .await
    }

    async fn finish_open_items(&mut self) -> bool {
        let mut open: Vec<_> = self.blocks.drain().collect();
        open.sort_by_key(|(index, _)| *index);
        for (_, block) in open {
            if !self.finish_block(block).await {
                return false;
            }
        }
        for call in std::mem::take(&mut self.unresolved_calls) {
            if !self
                .emit_result(call.id, items::unfinished_result(), true)
                .await
            {
                return false;
            }
        }
        self.announced.clear();
        true
    }

    async fn result(&mut self, incoming: Incoming) -> bool {
        let subtype = incoming.subtype.clone().unwrap_or_default();
        let failed = incoming.is_error || (!subtype.is_empty() && subtype != "success");
        let message = failure_text(&incoming, &subtype);
        if failed
            && self
                .shared
                .try_finish_handshake(Handshake::Failed(message.clone()))
        {
            return true;
        }
        if !self
            .withdraw_approvals_where(|pending| pending.subagent_id.is_none())
            .await
            || !self.finish_open_items().await
        {
            return false;
        }
        let window = incoming
            .model_usage
            .values()
            .map(|usage| usage.context_window)
            .max()
            .unwrap_or(0);
        if window > self.context_window {
            self.context_window = window;
            if !self.send_now(BackendEvent::ContextWindow(window)).await {
                return false;
            }
        }
        let aborted = incoming
            .terminal_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("aborted"))
            || self.shared.interrupt_sent.load(Ordering::SeqCst);
        self.shared.interrupt_sent.store(false, Ordering::SeqCst);
        self.shared.claude_turn.store(false, Ordering::SeqCst);
        if !self.shared.turn_active.swap(false, Ordering::SeqCst) {
            return self
                .end_result_outside_session_turn(failed, aborted, message)
                .await;
        }
        self.outside_turn_output_seen = false;
        let (status, error) = match (failed, aborted) {
            (false, _) => (TurnStatus::Completed, None),
            (true, true) => (TurnStatus::Interrupted, None),
            (true, false) => (TurnStatus::Failed, Some(message)),
        };
        let reap = status == TurnStatus::Failed;
        let sent = self
            .send_now(BackendEvent::TurnCompleted { status, error })
            .await;
        if reap {
            self.stop_after_error_grace();
        }
        sent
    }

    async fn end_result_outside_session_turn(
        &mut self,
        failed: bool,
        aborted: bool,
        message: String,
    ) -> bool {
        if failed && !aborted {
            if !self.send_now(BackendEvent::Error(message)).await {
                return false;
            }
            self.stop_after_error_grace();
        }
        self.maybe_idle().await
    }

    fn stop_after_error_grace(&self) {
        let _ = self
            .shared
            .supervisor
            .send(SupervisorCommand::ReapAfter(self.reap_grace));
    }
}

fn approval_id(request_id: &str) -> String {
    format!("claude-{request_id}")
}

fn text_item(id: &ItemId, thinking: bool, text: String) -> KnownItem {
    let id = id.clone();
    if thinking {
        KnownItem::Thinking { id, text }
    } else {
        KnownItem::AssistantMessage { id, text }
    }
}

fn result_item(call: String, output: Value, is_error: bool) -> KnownItem {
    KnownItem::ToolResult {
        id: ItemId(format!("{call}:result")),
        call_id: ItemId(call),
        output,
        is_error,
    }
}

fn failure_text(incoming: &Incoming, subtype: &str) -> String {
    if !incoming.errors.is_empty() {
        return incoming.errors.join("; ");
    }
    match incoming.result.as_deref() {
        Some(text) if !text.is_empty() => text.to_owned(),
        _ if subtype.is_empty() => "claude reported an error".to_owned(),
        _ => format!("claude reported {subtype}"),
    }
}
