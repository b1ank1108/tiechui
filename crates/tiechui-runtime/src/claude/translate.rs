mod agents;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tiechui_protocol::{ItemId, KnownItem, TurnStatus};
use tokio::sync::mpsc;

use super::catalogue::SharedCatalogue;
use super::debug;
use super::items;
use super::items::Pending;
use super::run::{Handshake, Shared, SupervisorCommand, Writer};
use super::wire::{
    Block, ControlResponse, Incoming, Initialized, Request, RequestIdOnly, StreamEvent,
    control_error,
};
use crate::backend::{BackendEvent, BackendItem};
use crate::process::{Outbox, StderrTail};
use agents::{Agent, ChildCall};

pub type CountedMessageIds = Arc<Mutex<HashSet<String>>>;

#[derive(Default)]
struct StreamingMessage {
    id: String,
    cache_write_1h: u64,
}

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
    out: Outbox,
    counted: CountedMessageIds,
    catalogue: SharedCatalogue,
    reap_grace: Duration,
    streaming: StreamingMessage,
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
        catalogue: SharedCatalogue,
        reap_grace: Duration,
        stderr: StderrTail,
    ) -> Self {
        Self {
            shared,
            writer,
            out: Outbox::new(out),
            counted,
            catalogue,
            reap_grace,
            streaming: StreamingMessage::default(),
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
        mut frames: mpsc::Receiver<Vec<u8>>,
        mut answered: mpsc::UnboundedReceiver<String>,
    ) {
        loop {
            let alive = tokio::select! {
                biased;
                frame = frames.recv() => match frame {
                    Some(frame) => self.frame(&frame).await,
                    None => {
                        self.exited().await;
                        false
                    }
                },
                Some(agent) = answered.recv() => self.approval_settled(&agent).await,
                () = self.out.flush_due() => self.out.flush().await,
            };
            if !alive {
                break;
            }
        }
    }

    async fn exited(&mut self) {
        self.shared.usable.store(false, Ordering::SeqCst);
        self.shared.drop_replies();
        let detail = self.stderr.detail();
        if self
            .shared
            .try_finish_handshake(Handshake::Failed(format!("claude exited{detail}")))
        {
            return;
        }
        self.out.flush().await;
        self.shared.claude_turn.store(false, Ordering::SeqCst);
        if !self.withdraw_approvals_where(|_| true).await
            || !self.finish_open_items().await
            || !self.interrupt_agents().await
        {
            return;
        }
        if self.shared.turn_active.swap(false, Ordering::SeqCst) {
            self.out
                .send_now(BackendEvent::TurnCompleted {
                    status: TurnStatus::Failed,
                    error: Some(format!("claude exited{detail}")),
                })
                .await;
        } else {
            self.maybe_idle().await;
        }
    }

    async fn frame(&mut self, frame: &[u8]) -> bool {
        let incoming: Incoming = match serde_json::from_slice(frame) {
            Ok(incoming) => incoming,
            Err(error) => {
                debug(&format!("dropped a frame that is not stream-json: {error}"));
                self.refuse_unreadable_request(frame, &error);
                return true;
            }
        };
        match incoming.kind.as_str() {
            "control_request" => self.control_request(incoming).await,
            "control_cancel_request" => self.cancel_request(incoming).await,
            "control_response" => self.control_response(incoming).await,
            "result" => self.result(incoming).await,
            "system" => self.system(incoming).await,
            _ => self.content(incoming).await,
        }
    }

    /// Frames of a sub-agent carry the `parent_tool_use_id` of the call that spawned it.
    async fn content(&mut self, incoming: Incoming) -> bool {
        let of_child = incoming.parent_tool_use_id.is_some();
        match (incoming.kind.as_str(), of_child) {
            ("stream_event", true) => true,
            ("assistant" | "user", true) => self.child_frame(incoming).await,
            ("stream_event", false) => self.stream_event(incoming).await,
            ("assistant", false) => self.assistant(incoming).await,
            ("user", false) => self.user(incoming).await,
            _ => true,
        }
    }

    async fn system(&mut self, incoming: Incoming) -> bool {
        match incoming.subtype.as_deref() {
            // Every turn begins with `init`; one while no turn of the session runs is Claude's own.
            Some("init") => {
                if !self.shared.turn_active() {
                    self.shared.interrupt_sent.store(false, Ordering::SeqCst);
                    self.shared.claude_turn.store(true, Ordering::SeqCst);
                    self.outside_turn_output_seen = true;
                }
                let Some(skills) = incoming.skills else {
                    return true;
                };
                let menu = self.catalogue.lock().loaded(skills);
                match menu {
                    Some(menu) => {
                        self.out
                            .emit_after_flush(BackendEvent::Commands(menu))
                            .await
                    }
                    None => true,
                }
            }
            Some("commands_changed") => {
                let Some(commands) = incoming.commands else {
                    return true;
                };
                let menu = self.catalogue.lock().set_commands(commands);
                self.out
                    .emit_after_flush(BackendEvent::Commands(menu))
                    .await
            }
            Some("task_started" | "task_notification") => self.task(incoming).await,
            _ => true,
        }
    }

    async fn control_response(&mut self, incoming: Incoming) -> bool {
        let Some(response) = incoming.response else {
            return true;
        };
        if response.request_id.as_deref() != Some(self.shared.initialize_request_id.as_str()) {
            self.shared.deliver_reply(response);
            return true;
        }
        self.initialized(response).await
    }

    /// The catalogue is in place before the driver learns that Claude is up.
    async fn initialized(&mut self, response: ControlResponse) -> bool {
        if response.subtype != "success" {
            let error = response.error.unwrap_or_else(|| "initialize failed".into());
            self.shared.try_finish_handshake(Handshake::Failed(error));
            return true;
        }
        let listed =
            serde_json::from_value::<Initialized>(response.response).unwrap_or_else(|error| {
                debug(&format!("cannot read what initialize listed: {error}"));
                Initialized::default()
            });
        let (models, menu) = self.catalogue.lock().initialized(listed);
        if !self.shared.try_finish_handshake(Handshake::Ready) {
            return true;
        }
        self.out.send_now(BackendEvent::Models(models)).await
            && self.out.send_now(BackendEvent::Commands(menu)).await
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
        self.refuse(
            &id,
            &format!("unsupported control request: {}", request.subtype),
        );
        true
    }

    fn refuse_unreadable_request(&self, frame: &[u8], error: &serde_json::Error) {
        if let Ok(request) = serde_json::from_slice::<RequestIdOnly>(frame)
            && request.kind == "control_request"
        {
            self.refuse(
                &request.request_id,
                &format!("unreadable control request: {error}"),
            );
        }
    }

    fn refuse(&self, request_id: &str, reason: &str) {
        if self
            .writer
            .send(&control_error(request_id, reason))
            .is_err()
        {
            debug("cannot answer a control request: claude is not taking input");
        }
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
            .out
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
            .out
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
                .out
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
                .unwrap_or_else(|| self.streaming.id.clone()),
        }
    }

    async fn stream_event(&mut self, incoming: Incoming) -> bool {
        let id = self.message_id(&incoming);
        let Some(event) = incoming.event else {
            return true;
        };
        match event.kind.as_str() {
            "message_start" => {
                let cache_write_1h = event.message.map_or(0, |message| {
                    message.usage.cache_creation.ephemeral_1h_input_tokens
                });
                self.streaming = StreamingMessage { id, cache_write_1h };
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
                id: ItemId(format!("{}:{}", self.streaming.id, event.index)),
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
                .out
                .emit_after_flush(BackendEvent::ItemStarted(BackendItem::root(text_item(
                    &id,
                    thinking,
                    String::new(),
                ))))
                .await
        {
            return false;
        }
        self.out.push_delta(&id, text).await
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
        self.out
            .emit_after_flush(BackendEvent::ItemCompleted(BackendItem::root(text_item(
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
        let cache_write_1h = if id == self.streaming.id {
            self.streaming.cache_write_1h
        } else {
            0
        };
        self.out
            .emit_after_flush(BackendEvent::Usage(items::usage(usage, cache_write_1h)))
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
        self.out
            .emit_after_flush(BackendEvent::ItemCompleted(BackendItem::root(call)))
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
        self.out
            .emit_after_flush(BackendEvent::ItemCompleted(BackendItem::root(result)))
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
        let (failed, message) = outcome(&incoming);
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
            || !self.report_context_window(&incoming).await
        {
            return false;
        }
        let aborted = self.aborted(&incoming);
        self.shared.claude_turn.store(false, Ordering::SeqCst);
        if !self.shared.turn_active.swap(false, Ordering::SeqCst) {
            return self
                .end_result_outside_session_turn(failed, aborted, message)
                .await;
        }
        self.outside_turn_output_seen = false;
        let (status, error) = turn_end(failed, aborted, message);
        if status == TurnStatus::Failed {
            self.stop_after_error_grace();
        }
        self.out
            .send_now(BackendEvent::TurnCompleted { status, error })
            .await
    }

    async fn report_context_window(&mut self, incoming: &Incoming) -> bool {
        let window = incoming
            .model_usage
            .values()
            .map(|usage| usage.context_window)
            .max()
            .unwrap_or(0);
        if window <= self.context_window {
            return true;
        }
        self.context_window = window;
        self.out.send_now(BackendEvent::ContextWindow(window)).await
    }

    fn aborted(&self, incoming: &Incoming) -> bool {
        let interrupt_sent = self.shared.interrupt_sent.swap(false, Ordering::SeqCst);
        interrupt_sent
            || incoming
                .terminal_reason
                .as_deref()
                .is_some_and(|reason| reason.starts_with("aborted"))
    }

    async fn end_result_outside_session_turn(
        &mut self,
        failed: bool,
        aborted: bool,
        message: String,
    ) -> bool {
        if failed && !aborted {
            self.stop_after_error_grace();
            if !self.out.send_now(BackendEvent::Error(message)).await {
                return false;
            }
        }
        self.maybe_idle().await
    }

    fn stop_after_error_grace(&self) {
        self.shared.usable.store(false, Ordering::SeqCst);
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

fn outcome(incoming: &Incoming) -> (bool, String) {
    let subtype = incoming.subtype.as_deref().unwrap_or_default();
    let failed = incoming.is_error || !matches!(subtype, "" | "success");
    (failed, failure_text(incoming, subtype))
}

fn turn_end(failed: bool, aborted: bool, message: String) -> (TurnStatus, Option<String>) {
    match (failed, aborted) {
        (false, _) => (TurnStatus::Completed, None),
        (true, true) => (TurnStatus::Interrupted, None),
        (true, false) => (TurnStatus::Failed, Some(message)),
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::task::Poll;

    use serde_json::json;

    use super::*;

    fn translator(out: mpsc::Sender<BackendEvent>) -> (Translator, Arc<Shared>) {
        let (shared, _supervisor, _answered) = Shared::new("init".into());
        shared.try_finish_handshake(Handshake::Ready);
        let (writer, _) = Writer::spawn(tokio::io::sink(), shared.clone());
        let translator = Translator::new(
            shared.clone(),
            writer,
            out,
            CountedMessageIds::default(),
            SharedCatalogue::default(),
            Duration::from_secs(9),
            StderrTail::default(),
        );
        (translator, shared)
    }

    fn error_result() -> Vec<u8> {
        json!({"type": "result", "subtype": "error_during_execution", "is_error": true,
            "errors": ["boom"]})
        .to_string()
        .into_bytes()
    }

    #[tokio::test]
    async fn an_error_result_marks_the_run_unusable_before_it_fails_the_turn() {
        let (out, mut events) = mpsc::channel(1);
        let fills_the_channel = BackendEvent::Idle;
        out.try_send(fills_the_channel).unwrap();
        let (mut translator, shared) = translator(out);
        shared.turn_active.store(true, Ordering::SeqCst);
        let frame = error_result();
        let mut handling = std::pin::pin!(translator.frame(&frame));
        let first_poll = std::future::poll_fn(|cx| Poll::Ready(handling.as_mut().poll(cx))).await;
        assert!(first_poll.is_pending(), "waiting to send TurnCompleted");
        assert!(!shared.is_usable());
        assert!(matches!(events.recv().await, Some(BackendEvent::Idle)));
        assert!(handling.await);
        assert!(matches!(
            events.recv().await,
            Some(BackendEvent::TurnCompleted {
                status: TurnStatus::Failed,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn an_interrupt_flag_left_from_an_earlier_turn_does_not_hide_an_error_of_claudes_own_turn()
     {
        let (out, mut events) = mpsc::channel(8);
        let (mut translator, shared) = translator(out);
        let interrupt_after_the_last_result = true;
        shared
            .interrupt_sent
            .store(interrupt_after_the_last_result, Ordering::SeqCst);
        assert!(
            translator
                .frame(br#"{"type": "system", "subtype": "init"}"#)
                .await
        );
        assert!(translator.frame(&error_result()).await);
        assert!(matches!(events.try_recv(), Ok(BackendEvent::Error(message)) if message == "boom"));
    }
}
