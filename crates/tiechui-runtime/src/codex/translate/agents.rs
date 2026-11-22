//! Codex's own sub-agents: their threads, spawn calls and lifecycle. The spawn stays a root tool
//! call; what a sub-agent produces is reported as items that name the spawn call, and its
//! lifecycle as `Subagent` updates. A sub-agent's turn never ends the root turn.

use serde_json::{Value, json};
use tiechui_protocol::{ItemId, KnownItem, SubagentState};

use super::{ToolStub, Translator, TurnBody, debug, items, tool_dones, tool_starts};
use crate::backend::{BackendEvent, BackendItem, ChildOf, SubagentUpdate};

/// A sub-agent's item id: Codex's item ids are only unique within a thread.
pub fn scoped(thread: &str, item: &str) -> String {
    format!("{thread}:{item}")
}

pub struct Agent {
    parent_call_id: ItemId,
    name: Option<String>,
    state: SubagentState,
    message: Option<String>,
}

impl Agent {
    fn running(&self) -> bool {
        matches!(self.state, SubagentState::Running | SubagentState::Waiting)
    }

    fn accepts_message_after_end(&self) -> bool {
        self.message.is_none()
    }

    fn takes_message(&self, message: &Option<String>) -> bool {
        message.is_some()
            && *message != self.message
            && (self.running() || self.accepts_message_after_end())
    }

    fn transition(&mut self, state: SubagentState, message: Option<String>) -> bool {
        let new_message = self.takes_message(&message);
        if self.state == state && !new_message {
            return false;
        }
        self.state = state;
        if state == SubagentState::Running {
            self.message = None;
        } else if new_message {
            self.message = message;
        }
        true
    }
}

fn snake(camel: &str) -> String {
    let mut out = String::new();
    for ch in camel.chars() {
        if ch.is_ascii_uppercase() {
            out.push('_');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

pub fn collab_call(item: &Value) -> Option<(String, Value)> {
    let name = snake(item["tool"].as_str()?);
    let mut arguments = serde_json::Map::new();
    for (key, field) in [
        ("prompt", "prompt"),
        ("model", "model"),
        ("reasoning_effort", "reasoningEffort"),
    ] {
        if let Some(text) = item[field].as_str().filter(|text| !text.is_empty()) {
            arguments.insert(key.into(), json!(text));
        }
    }
    if let Some(ids) = item["receiverThreadIds"]
        .as_array()
        .filter(|ids| !ids.is_empty())
    {
        arguments.insert("receiver_thread_ids".into(), json!(ids));
    }
    Some((name, Value::Object(arguments)))
}

pub fn collab_result(item: &Value) -> (String, bool) {
    let failed = item["status"] == "failed";
    let mut lines: Vec<String> = item["agentsStates"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(thread, state)| {
            let status = state["status"].as_str().unwrap_or("unknown");
            match state["message"].as_str().filter(|text| !text.is_empty()) {
                Some(message) => format!("{thread}: {status}: {message}"),
                None => format!("{thread}: {status}"),
            }
        })
        .collect();
    if lines.is_empty() {
        let ids: Vec<&str> = item["receiverThreadIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        lines.push(match (item["tool"].as_str(), ids.is_empty()) {
            (Some("spawnAgent"), false) => format!("Spawned {}", ids.join(", ")),
            _ if failed => "The call failed".into(),
            _ => "Done".into(),
        });
    }
    (lines.join("\n"), failed)
}

fn state_of(status: &str) -> Option<SubagentState> {
    match status {
        "completed" => Some(SubagentState::Completed),
        "interrupted" | "shutdown" => Some(SubagentState::Interrupted),
        "errored" | "notFound" | "failed" => Some(SubagentState::Failed),
        _ => None,
    }
}

impl Translator {
    fn parent_call(&self, sender: &str, call_id: &str) -> ItemId {
        if self.is_root(sender) {
            ItemId(call_id.to_owned())
        } else {
            ItemId(scoped(sender, call_id))
        }
    }

    async fn child_event(&mut self, thread: &str, item: KnownItem) -> bool {
        let Some(agent) = self.agents.get(thread) else {
            return true;
        };
        let child_of = ChildOf {
            parent_call_id: agent.parent_call_id.clone(),
            agent_id: thread.to_owned(),
        };
        self.note_derived();
        self.out
            .emit_after_flush(BackendEvent::ItemCompleted(BackendItem {
                item,
                child_of: Some(child_of),
            }))
            .await
    }

    async fn child_call(
        &mut self,
        thread: &str,
        stub: &ToolStub,
        result: Option<(Value, bool)>,
    ) -> bool {
        let call = ItemId(stub.id.clone());
        let announced = KnownItem::ToolCall {
            id: call.clone(),
            name: stub.name.clone(),
            arguments: stub.arguments.clone(),
        };
        if !self.child_event(thread, announced).await {
            return false;
        }
        let Some((output, is_error)) = result else {
            return true;
        };
        self.child_event(
            thread,
            KnownItem::ToolResult {
                id: ItemId(format!("{}:result", stub.id)),
                call_id: call,
                output,
                is_error,
            },
        )
        .await
    }

    async fn complete_call_in(&mut self, thread: &str, stub: ToolStub, text: &str) -> bool {
        let output = items::tool_output_text(text, false);
        if self.is_root(thread) {
            self.complete_call(&stub.id, &stub.name, stub.arguments, output, false)
                .await
        } else {
            let scoped_stub = ToolStub {
                id: scoped(thread, &stub.id),
                ..stub
            };
            self.child_call(thread, &scoped_stub, Some((output, false)))
                .await
        }
    }

    pub(super) async fn child_item(&mut self, thread: &str, item: &Value, started: bool) -> bool {
        if !self.agents.contains_key(thread) {
            debug(&format!("dropped an item of thread {thread}"));
            return true;
        }
        let kind = item["type"].as_str().unwrap_or_default();
        if kind == "subAgentActivity" {
            return self.sub_agent_activity(thread, item).await;
        }
        let ok = match kind {
            "agentMessage" | "reasoning" if !started => self.child_text(thread, kind, item).await,
            "agentMessage" | "reasoning" | "userMessage" => true,
            _ if started => self.child_tool_started(thread, kind, item).await,
            _ => self.child_tool_done(thread, kind, item).await,
        };
        ok && self.collab_effects(thread, item).await
    }

    async fn child_text(&mut self, thread: &str, kind: &str, item: &Value) -> bool {
        let Some(id) = item["id"].as_str() else {
            return true;
        };
        let id = ItemId(scoped(thread, id));
        let text = if kind == "agentMessage" {
            item["text"].as_str().unwrap_or_default().to_owned()
        } else {
            let parts = |key: &str| -> Vec<String> {
                item[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part.as_str().map(str::to_owned))
                    .collect()
            };
            let summary = parts("summary");
            let chosen = if summary.is_empty() {
                parts("content")
            } else {
                summary
            };
            chosen.join("\n\n")
        };
        if text.is_empty() {
            return true;
        }
        let known = if kind == "agentMessage" {
            KnownItem::AssistantMessage { id, text }
        } else {
            KnownItem::Thinking { id, text }
        };
        self.child_event(thread, known).await
    }

    async fn child_tool_started(&mut self, thread: &str, kind: &str, item: &Value) -> bool {
        let Some((id, calls)) = tool_starts(kind, item) else {
            return true;
        };
        let stubs: Vec<ToolStub> = calls
            .into_iter()
            .map(|(name, arguments, call_id)| ToolStub {
                id: scoped(thread, &call_id),
                name,
                arguments,
            })
            .collect();
        for stub in &stubs {
            if !self.child_call(thread, stub, None).await {
                return false;
            }
        }
        self.child_tools.insert(scoped(thread, &id), stubs);
        true
    }

    async fn child_tool_done(&mut self, thread: &str, kind: &str, item: &Value) -> bool {
        let Some((id, dones)) = tool_dones(kind, item) else {
            return true;
        };
        let announced = self.child_tools.remove(&scoped(thread, &id)).is_some();
        for done in dones {
            let stub = ToolStub {
                id: scoped(thread, &done.id),
                name: done.name,
                arguments: done.arguments,
            };
            let sent = if announced {
                self.child_event(
                    thread,
                    KnownItem::ToolResult {
                        id: ItemId(format!("{}:result", stub.id)),
                        call_id: ItemId(stub.id.clone()),
                        output: done.output,
                        is_error: done.is_error,
                    },
                )
                .await
            } else {
                self.child_call(thread, &stub, Some((done.output, done.is_error)))
                    .await
            };
            if !sent {
                return false;
            }
        }
        true
    }

    pub(super) async fn sub_agent_activity(&mut self, sender: &str, item: &Value) -> bool {
        let (Some(id), Some(agent)) = (item["id"].as_str(), item["agentThreadId"].as_str()) else {
            return true;
        };
        let path = item["agentPath"].as_str().unwrap_or_default();
        match item["kind"].as_str().unwrap_or_default() {
            "started" => {
                if self.agents.contains_key(agent) {
                    return true;
                }
                let stub = ToolStub {
                    id: id.to_owned(),
                    name: "spawn_agent".into(),
                    arguments: json!({"agent": path}),
                };
                if !self
                    .complete_call_in(sender, stub, &format!("Started {path}"))
                    .await
                {
                    return false;
                }
                let name = path.rsplit('/').next().filter(|name| !name.is_empty());
                self.register_agent(sender, agent, id, name).await
            }
            "completed" => self.set_state(agent, SubagentState::Completed, None).await,
            "interrupted" => {
                self.set_state(agent, SubagentState::Interrupted, None)
                    .await
            }
            _ => self.set_state(agent, SubagentState::Running, None).await,
        }
    }

    pub(super) async fn collab_effects(&mut self, thread: &str, item: &Value) -> bool {
        if item["type"] != "collabAgentToolCall" {
            return true;
        }
        let call_id = item["id"].as_str().unwrap_or_default().to_owned();
        if item["tool"] == "spawnAgent" {
            for receiver in item["receiverThreadIds"].as_array().into_iter().flatten() {
                if let Some(receiver) = receiver.as_str()
                    && !self.register_agent(thread, receiver, &call_id, None).await
                {
                    return false;
                }
            }
        }
        for (agent, state) in item["agentsStates"].as_object().into_iter().flatten() {
            let Some(next) = state["status"].as_str().and_then(state_of) else {
                continue;
            };
            let message = state["message"].as_str().map(str::to_owned);
            if !self.set_state(agent, next, message).await {
                return false;
            }
        }
        true
    }

    async fn register_agent(
        &mut self,
        sender: &str,
        thread: &str,
        call_id: &str,
        name: Option<&str>,
    ) -> bool {
        if self.agents.contains_key(thread) || self.is_root(thread) {
            return true;
        }
        let parent_call_id = self.parent_call(sender, call_id);
        let name = name.map(str::to_owned);
        self.agents.insert(
            thread.to_owned(),
            Agent {
                parent_call_id: parent_call_id.clone(),
                name: name.clone(),
                state: SubagentState::Running,
                message: None,
            },
        );
        self.note_derived();
        self.out
            .emit_after_flush(BackendEvent::Subagent(SubagentUpdate {
                agent_id: thread.to_owned(),
                parent_call_id,
                name,
                state: SubagentState::Running,
                message: None,
            }))
            .await
    }

    async fn set_state(
        &mut self,
        thread: &str,
        state: SubagentState,
        message: Option<String>,
    ) -> bool {
        let Some(agent) = self.agents.get_mut(thread) else {
            return true;
        };
        if !agent.transition(state, message) {
            return true;
        }
        let update = SubagentUpdate {
            agent_id: thread.to_owned(),
            parent_call_id: agent.parent_call_id.clone(),
            name: agent.name.clone(),
            state,
            message: agent.message.clone(),
        };
        self.note_derived();
        self.out
            .emit_after_flush(BackendEvent::Subagent(update))
            .await
            && self.maybe_idle().await
    }

    pub(super) async fn agent_turn_started(&mut self, thread: &str) -> bool {
        self.set_state(thread, SubagentState::Running, None).await
    }

    pub(super) async fn agent_turn_completed(&mut self, thread: &str, turn: TurnBody) -> bool {
        if !self.agents.contains_key(thread) {
            debug(&format!("dropped a turn/completed of thread {thread}"));
            return true;
        }
        let state = state_of(&turn.status).unwrap_or(SubagentState::Completed);
        let message = turn
            .items
            .iter()
            .rev()
            .find(|item| item["type"] == "agentMessage")
            .and_then(|item| item["text"].as_str())
            .filter(|text| !text.is_empty())
            .map(str::to_owned);
        if !self.withdraw_approvals_of(Some(thread)).await {
            return false;
        }
        let prefix = scoped(thread, "");
        self.child_tools.retain(|id, _| !id.starts_with(&prefix));
        self.set_state(thread, state, message).await
    }

    pub(super) async fn agent_waits(&mut self, thread: &str) -> bool {
        match self.agents.get(thread) {
            Some(agent) if agent.state == SubagentState::Running => {
                self.set_state(thread, SubagentState::Waiting, None).await
            }
            _ => true,
        }
    }

    pub(super) async fn approval_settled(&mut self, thread: &str) -> bool {
        let waiting = self
            .agents
            .get(thread)
            .is_some_and(|agent| agent.state == SubagentState::Waiting);
        let open = self
            .shared
            .approvals()
            .values()
            .any(|pending| pending.thread == thread);
        if waiting && !open {
            return self.set_state(thread, SubagentState::Running, None).await;
        }
        self.maybe_idle().await
    }

    pub(super) fn note_derived(&mut self) {
        if self.no_root_turn() {
            self.outside_turn_output_seen = true;
        }
    }

    fn no_root_turn(&self) -> bool {
        let turn = self.shared.turn.borrow();
        turn.active.is_none() && !turn.awaiting_turn_started
    }

    fn no_agent_active(&self) -> bool {
        !self.agents.values().any(Agent::running)
    }

    fn no_approval_pending(&self) -> bool {
        self.shared.approvals().is_empty()
    }

    pub(super) async fn maybe_idle(&mut self) -> bool {
        if !self.outside_turn_output_seen
            || !self.no_root_turn()
            || !self.no_agent_active()
            || !self.no_approval_pending()
        {
            return true;
        }
        self.outside_turn_output_seen = false;
        self.out.emit_after_flush(BackendEvent::Idle).await
    }
}
