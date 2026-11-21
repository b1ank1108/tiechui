//! Claude's own sub-agents (the `Agent` tool). The spawning call stays a root tool call; what a
//! sub-agent produces (frames with `parent_tool_use_id` and `agent_id`) is reported as complete
//! items that name the spawning call, and its lifecycle (`task_started`, its permission requests,
//! `task_notification`) as `Subagent` updates. A background sub-agent outlives the root turn: the
//! session goes idle only once no turn runs, no sub-agent runs and no request is open.

use serde_json::json;
use tiechui_protocol::{ItemId, KnownItem, SubagentState};

use super::super::items::{self, Pending};
use super::super::wire::{Block, Incoming, Message};
use super::{Translator, text_item};
use crate::backend::{BackendEvent, BackendItem, ChildOf, SubagentUpdate};

/// A sub-agent's item id: Claude's ids are its own, the agent id keeps them apart from the root's.
pub fn scoped(agent: &str, id: &str) -> String {
    format!("{agent}:{id}")
}

pub struct Agent {
    parent_call_id: ItemId,
    name: Option<String>,
    state: SubagentState,
    message: Option<String>,
}

impl Agent {
    fn live(&self) -> bool {
        matches!(self.state, SubagentState::Running | SubagentState::Waiting)
    }
}

/// A tool call one of the sub-agents made, by Claude's call id.
pub struct ChildCall {
    agent: String,
    /// The native tool name, which shapes the result.
    name: String,
    open: bool,
}

fn ended(status: &str) -> SubagentState {
    match status {
        "completed" => SubagentState::Completed,
        "failed" => SubagentState::Failed,
        _ => SubagentState::Interrupted,
    }
}

impl Translator {
    /// `task_started` of a sub-agent (a background shell command is a task too, but no
    /// sub-agent) and `task_notification`, which ends a task with its summary.
    pub(super) async fn task(&mut self, incoming: Incoming) -> bool {
        let Some(agent) = incoming.task_id else {
            return true;
        };
        match incoming.subtype.as_deref() {
            Some("task_started") if incoming.task_type.as_deref() == Some("local_agent") => {
                let Some(call) = incoming.tool_use_id else {
                    return true;
                };
                let name = incoming
                    .subagent_type
                    .or(incoming.description)
                    .filter(|name| !name.is_empty());
                self.register(&agent, &call, name).await
            }
            Some("task_notification") => {
                let state = ended(incoming.status.as_deref().unwrap_or_default());
                let summary = incoming.summary.filter(|summary| !summary.is_empty());
                self.set_state(&agent, state, summary).await
            }
            _ => true,
        }
    }

    /// A sub-agent spawned by `call`: a root call, or a call of another sub-agent.
    async fn register(&mut self, agent: &str, call: &str, name: Option<String>) -> bool {
        if self.agents.contains_key(agent) {
            return true;
        }
        let parent_call_id = ItemId(match self.child_calls.get(call) {
            Some(spawner) => scoped(&spawner.agent, call),
            None => call.to_owned(),
        });
        self.agents.insert(
            agent.to_owned(),
            Agent {
                parent_call_id: parent_call_id.clone(),
                name: name.clone(),
                state: SubagentState::Running,
                message: None,
            },
        );
        self.note_derived();
        self.emit_after_flush(BackendEvent::Subagent(SubagentUpdate {
            agent_id: agent.to_owned(),
            parent_call_id,
            name,
            state: SubagentState::Running,
            message: None,
        }))
        .await
    }

    /// Reports a change of state; only an end carries a message. A sub-agent that ends takes its
    /// open requests and calls with it.
    async fn set_state(
        &mut self,
        agent: &str,
        state: SubagentState,
        message: Option<String>,
    ) -> bool {
        let Some(current) = self.agents.get(agent) else {
            return true;
        };
        let ending = !matches!(state, SubagentState::Running | SubagentState::Waiting);
        let message = if ending { message } else { None };
        if current.state == state && current.message == message {
            return true;
        }
        if ending
            && current.live()
            && !(self
                .withdraw_approvals_where(|pending| pending.agent.as_deref() == Some(agent))
                .await
                && self.settle_child_calls(agent).await)
        {
            return false;
        }
        let Some(current) = self.agents.get_mut(agent) else {
            return true;
        };
        current.state = state;
        current.message = message;
        let update = SubagentUpdate {
            agent_id: agent.to_owned(),
            parent_call_id: current.parent_call_id.clone(),
            name: current.name.clone(),
            state,
            message: current.message.clone(),
        };
        self.note_derived();
        self.emit_after_flush(BackendEvent::Subagent(update)).await && self.maybe_idle().await
    }

    /// Ends every sub-agent still running: the process is gone.
    pub(super) async fn interrupt_agents(&mut self) -> bool {
        let mut live: Vec<String> = self
            .agents
            .iter()
            .filter(|(_, agent)| agent.live())
            .map(|(id, _)| id.clone())
            .collect();
        live.sort();
        for agent in live {
            if !self
                .set_state(&agent, SubagentState::Interrupted, None)
                .await
            {
                return false;
            }
        }
        true
    }

    /// A frame a sub-agent produced. Sub-agents stream nothing: their `assistant` frames carry
    /// complete blocks, their `user` frames the tool results (and the prompt, which is no step).
    pub(super) async fn child_frame(&mut self, incoming: Incoming) -> bool {
        let (Some(call), Some(agent), Some(message)) = (
            incoming.parent_tool_use_id,
            incoming.agent_id,
            incoming.message,
        ) else {
            return true;
        };
        if !self.register(&agent, &call, None).await {
            return false;
        }
        if incoming.kind == "assistant" {
            self.child_assistant(&agent, &message).await
        } else {
            self.child_results(&agent, &message).await
        }
    }

    async fn child_assistant(&mut self, agent: &str, message: &Message) -> bool {
        let msg = message.id.clone().unwrap_or_default();
        for block in message.blocks() {
            let sent = match block.kind.as_str() {
                "text" | "thinking" => self.child_text(agent, &msg, &block).await,
                "tool_use" => {
                    self.announce_child(agent, &block.id, &block.name, &block.input)
                        .await
                }
                _ => true,
            };
            if !sent {
                return false;
            }
        }
        true
    }

    async fn child_text(&mut self, agent: &str, msg: &str, block: &Block) -> bool {
        let thinking = block.kind == "thinking";
        let text = if thinking {
            &block.thinking
        } else {
            &block.text
        };
        if text.is_empty() {
            return true;
        }
        let index = self.child_blocks.entry(scoped(agent, msg)).or_default();
        let id = ItemId(format!("{agent}:{msg}:{index}"));
        *index += 1;
        self.child_event(agent, text_item(&id, thinking, text.clone()))
            .await
    }

    /// Shows a sub-agent's tool call once, from its `assistant` frame or its permission request.
    pub(super) async fn announce_child(
        &mut self,
        agent: &str,
        call: &str,
        tool: &str,
        input: &serde_json::Value,
    ) -> bool {
        if self.child_calls.contains_key(call) || !self.agents.contains_key(agent) {
            return true;
        }
        let (name, arguments) = items::tool_call(tool, input);
        self.child_calls.insert(
            call.to_owned(),
            ChildCall {
                agent: agent.to_owned(),
                name: name.clone(),
                open: true,
            },
        );
        let call = KnownItem::ToolCall {
            id: ItemId(scoped(agent, call)),
            name,
            arguments,
        };
        self.child_event(agent, call).await
    }

    async fn child_results(&mut self, agent: &str, message: &Message) -> bool {
        for block in message.blocks() {
            if block.kind != "tool_result" {
                continue;
            }
            let name = match self.child_calls.get_mut(&block.tool_use_id) {
                Some(call) if call.open && call.agent == agent => {
                    call.open = false;
                    call.name.clone()
                }
                _ => continue,
            };
            let output = items::tool_result(&name, &block.content, block.is_error);
            if !self
                .child_result(agent, &block.tool_use_id, output, block.is_error)
                .await
            {
                return false;
            }
        }
        true
    }

    async fn child_result(
        &mut self,
        agent: &str,
        call: &str,
        output: serde_json::Value,
        is_error: bool,
    ) -> bool {
        let call = scoped(agent, call);
        let result = KnownItem::ToolResult {
            id: ItemId(format!("{call}:result")),
            call_id: ItemId(call),
            output,
            is_error,
        };
        self.child_event(agent, result).await
    }

    /// Fails the calls an ending sub-agent left without a result and forgets its calls.
    async fn settle_child_calls(&mut self, agent: &str) -> bool {
        let mut open: Vec<String> = self
            .child_calls
            .iter()
            .filter(|(_, call)| call.agent == agent && call.open)
            .map(|(id, _)| id.clone())
            .collect();
        open.sort();
        for call in open {
            let output = items::tool_result("", &json!("The tool call did not finish."), true);
            if !self.child_result(agent, &call, output, true).await {
                return false;
            }
        }
        self.child_calls.retain(|_, call| call.agent != agent);
        let prefix = scoped(agent, "");
        self.child_blocks.retain(|id, _| !id.starts_with(&prefix));
        true
    }

    async fn child_event(&mut self, agent: &str, item: KnownItem) -> bool {
        let Some(spawned) = self.agents.get(agent) else {
            return true;
        };
        let child_of = ChildOf {
            parent_call_id: spawned.parent_call_id.clone(),
            agent_id: agent.to_owned(),
        };
        self.note_derived();
        self.emit_after_flush(BackendEvent::ItemCompleted(BackendItem {
            item,
            child_of: Some(child_of),
        }))
        .await
    }

    pub(super) async fn agent_waits(&mut self, agent: &str) -> bool {
        match self.agents.get(agent) {
            Some(current) if current.state == SubagentState::Running => {
                self.set_state(agent, SubagentState::Waiting, None).await
            }
            _ => true,
        }
    }

    /// A request of the sub-agent was answered or withdrawn: it runs again once none is open.
    pub(super) async fn approval_settled(&mut self, agent: &str) -> bool {
        let waiting = self
            .agents
            .get(agent)
            .is_some_and(|current| current.state == SubagentState::Waiting);
        let open = self
            .shared
            .approvals()
            .values()
            .any(|pending: &Pending| pending.agent.as_deref() == Some(agent));
        if waiting && !open {
            return self.set_state(agent, SubagentState::Running, None).await;
        }
        self.maybe_idle().await
    }

    /// Notes output that reaches the session while none of its turns runs: it opens a derived
    /// turn, which the next `Idle` closes.
    pub(super) fn note_derived(&mut self) {
        if !self.shared.turn_active() {
            self.outside_turn_output_seen = true;
        }
    }

    pub(super) async fn maybe_idle(&mut self) -> bool {
        let busy = self.shared.turn_active()
            || self.shared.own_turn()
            || self.agents.values().any(Agent::live)
            || !self.shared.approvals().is_empty();
        if !self.outside_turn_output_seen || busy {
            return true;
        }
        self.outside_turn_output_seen = false;
        self.emit_after_flush(BackendEvent::Idle).await
    }
}
