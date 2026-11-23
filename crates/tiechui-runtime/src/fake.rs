//! A scripted backend registered as runtime `fake`, for tests. `CHUI_FAKE_RUNTIME_SCRIPT` names
//! a JSON file with the turns to play and `CHUI_FAKE_RUNTIME_LOG`, when set, a file that gets one
//! line for each start, approval answer, steer, interrupt, model or thinking switch and command
//! the backend receives. The `start` line shows the model and thinking level it was started with.
//! The runtime reports a fixed model catalogue and command list (one of them a skill).
//!
//! ```json
//! { "steer": "accept", "turns": [ { "steps": [
//!   { "op": "text", "text": "hello", "chunks": 2 },
//!   { "op": "thinking", "text": "hm" },
//!   { "op": "command", "command": "ls", "output": "a\n", "is_error": false },
//!   { "op": "usage", "input": 10, "output": 5 },
//!   { "op": "approval", "tool": "bash", "values": ["touch x"], "agent": "a1" },
//!   { "op": "subagent", "id": "a1", "name": "worker", "final": "completed", "steps": [],
//!     "phase": "full" },
//!   { "op": "sleep", "ms": 50 },
//!   { "op": "hang" },
//!   { "op": "turn_end", "status": "completed" },
//!   { "op": "idle" }
//! ] } ] }
//! ```
//!
//! A `subagent` step with `"phase": "spawn"` only spawns the sub-agent (its spawn call and a
//! running status); with `"phase": "output"` it only plays the child steps and the final status,
//! as a sub-agent that outlives its parent turn does.
//!
//! Each `start_turn` plays the next turn; a turn that lacks `turn_end` and `idle` gets them at
//! its end. Steps after `turn_end` are events after the turn ended. `approval` waits for its
//! answer, then says it in an assistant text.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tiechui_protocol::{
    ApprovalDecision, ApprovalRequest, CommandInfo, CommandKind, InputContent, ItemId, KnownItem,
    ModelInfo, PERMISSIONS_COMMAND, RuntimeCapabilities, SubagentState, ThinkingLevel, TokenUsage,
    TurnStatus,
};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::backend::{
    Backend, BackendEvent, BackendItem, ChildOf, CommandAction, StartParams, Started, Steered,
    SubagentUpdate,
};
use crate::registry::{Driver, Launch, Probe};
use crate::tool_output;

pub const SCRIPT_ENV: &str = "CHUI_FAKE_RUNTIME_SCRIPT";
pub const LOG_ENV: &str = "CHUI_FAKE_RUNTIME_LOG";

pub struct FakeDriver;

#[async_trait]
impl Driver for FakeDriver {
    fn id(&self) -> &'static str {
        "fake"
    }

    fn label(&self) -> &'static str {
        "Fake"
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            model: true,
            thinking: true,
            commands: true,
            permissions: true,
            ..RuntimeCapabilities::minimal()
        }
    }

    fn gated_ops(&self) -> &'static [&'static str] {
        &[
            "run_command",
            "command_input",
            "permissions",
            "set_thinking",
            "set_model",
        ]
    }

    async fn probe(&self, _: &Launch) -> Probe {
        match std::env::var_os(SCRIPT_ENV) {
            Some(path) => Probe {
                available: true,
                version: Some("0.0.0-fake".into()),
                path: Some(PathBuf::from(path)),
                reason: None,
            },
            None => Probe::unavailable(format!("{SCRIPT_ENV} is not set")),
        }
    }

    fn backend(&self, _: &Launch) -> Result<Box<dyn Backend>, String> {
        let path = std::env::var_os(SCRIPT_ENV).ok_or(format!("{SCRIPT_ENV} is not set"))?;
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("{}: {error}", PathBuf::from(&path).display()))?;
        let script: Value = serde_json::from_str(&text)
            .map_err(|error| format!("{}: {error}", PathBuf::from(&path).display()))?;
        Ok(Box::new(FakeBackend {
            script,
            log: std::env::var_os(LOG_ENV).map(PathBuf::from),
            events: None,
            next_turn: 0,
            shared: Arc::default(),
            model: catalogue().remove(0),
        }))
    }
}

#[derive(Default)]
struct Shared {
    answers: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
    interrupted: Notify,
}

struct FakeBackend {
    script: Value,
    log: Option<PathBuf>,
    events: Option<mpsc::Sender<BackendEvent>>,
    next_turn: usize,
    shared: Arc<Shared>,
    model: ModelInfo,
}

const LEVELS: [ThinkingLevel; 4] = [
    ThinkingLevel::Off,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
];

fn catalogue() -> Vec<ModelInfo> {
    let model = |id: &str, reasoning: bool, levels: &[ThinkingLevel]| ModelInfo {
        provider: "fake".into(),
        id: id.into(),
        context_window: 1000,
        reasoning,
        thinking_levels: levels.to_vec(),
    };
    vec![
        model("fake-model", true, &LEVELS),
        model("fake-small", false, &[ThinkingLevel::Off]),
    ]
}

fn commands() -> Vec<CommandInfo> {
    let info = |name: &str, kind| CommandInfo {
        name: name.into(),
        description: format!("fake {name}"),
        argument_hint: None,
        kind,
    };
    vec![
        info("review", CommandKind::Command),
        info("fake-skill", CommandKind::Skill),
    ]
}

fn level_name(level: ThinkingLevel) -> String {
    serde_json::to_value(level)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

impl FakeBackend {
    fn log(&self, line: &str) {
        if let Some(path) = &self.log
            && let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}

#[async_trait]
impl Backend for FakeBackend {
    async fn start(
        &mut self,
        params: StartParams,
    ) -> Result<(Started, mpsc::Receiver<BackendEvent>), String> {
        let (tx, rx) = mpsc::channel(256);
        let external_id = params
            .resume
            .clone()
            .unwrap_or_else(|| format!("fake-thread-{}", std::process::id()));
        let thinking = params.thinking.unwrap_or(ThinkingLevel::Off);
        self.log(&format!(
            "start resume={} model={} thinking={} id={external_id}",
            params.resume.as_deref().unwrap_or("none"),
            params.model.as_deref().unwrap_or("none"),
            params.thinking.map_or_else(|| "none".into(), level_name),
        ));
        if let Some(model) = catalogue()
            .into_iter()
            .find(|model| Some(&model.id) == params.model.as_ref())
        {
            self.model = model;
        }
        let _ = tx.send(BackendEvent::Models(catalogue())).await;
        let _ = tx.send(BackendEvent::Commands(commands())).await;
        self.events = Some(tx);
        Ok((
            Started {
                resumed: params.resume.is_some(),
                external_id,
                model: self.model.clone(),
                thinking,
            },
            rx,
        ))
    }

    async fn start_turn(&mut self, _: &[InputContent]) -> Result<(), String> {
        let steps = self.script["turns"][self.next_turn]["steps"]
            .as_array()
            .cloned()
            .ok_or_else(|| format!("the script has no turn {}", self.next_turn))?;
        self.next_turn += 1;
        let events = self.events.clone().ok_or("the backend is not started")?;
        tokio::spawn(play(steps, events, self.shared.clone()));
        Ok(())
    }

    async fn steer(&mut self, input: &[InputContent]) -> Result<Steered, String> {
        let text: String = input
            .iter()
            .filter_map(|part| match part {
                InputContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        self.log(&format!("steer {text}"));
        Ok(if self.script["steer"] == "reject" {
            Steered::Rejected("the script rejects steering".into())
        } else {
            Steered::Accepted
        })
    }

    async fn interrupt(&mut self) -> Result<(), String> {
        self.log("interrupt");
        self.shared.interrupted.notify_waiters();
        self.shared.interrupted.notify_one();
        Ok(())
    }

    async fn answer_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), String> {
        self.log(&format!(
            "answer {approval_id} {}",
            serde_json::to_value(&decision)
                .ok()
                .and_then(|value| value["kind"].as_str().map(str::to_owned))
                .unwrap_or_default()
        ));
        let waiting = self
            .shared
            .answers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(approval_id);
        if let Some(waiting) = waiting {
            let _ = waiting.send(decision);
        }
        Ok(())
    }

    async fn run_command(&mut self, name: &str, arguments: &str) -> Result<CommandAction, String> {
        self.log(&format!("run_command {name} {arguments}"));
        Ok(if name == PERMISSIONS_COMMAND {
            CommandAction::PermissionMode(arguments.trim().to_owned())
        } else {
            CommandAction::Prompt(format!("/{name} {arguments}"))
        })
    }

    async fn command_input(&mut self, name: &str, _: Value) -> Result<(), String> {
        self.log(&format!("command_input {name}"));
        Ok(())
    }

    async fn set_model(&mut self, model: &str) -> Result<ModelInfo, String> {
        self.log(&format!("set_model {model}"));
        self.model = catalogue()
            .into_iter()
            .find(|candidate| candidate.id == model)
            .ok_or_else(|| format!("unknown fake model {model}"))?;
        Ok(self.model.clone())
    }

    async fn set_thinking(&mut self, level: ThinkingLevel) -> Result<ThinkingLevel, String> {
        self.log(&format!("set_thinking {}", level_name(level)));
        Ok(level)
    }

    async fn shutdown(&mut self) {
        self.events = None;
    }
}

struct Player {
    events: mpsc::Sender<BackendEvent>,
    shared: Arc<Shared>,
    counter: u64,
    ended: bool,
    idled: bool,
}

async fn play(steps: Vec<Value>, events: mpsc::Sender<BackendEvent>, shared: Arc<Shared>) {
    let mut player = Player {
        events,
        shared,
        counter: 0,
        ended: false,
        idled: false,
    };
    for step in &steps {
        if !player.step(step, None).await {
            return;
        }
    }
    if !player.ended {
        player
            .send(BackendEvent::TurnCompleted {
                status: TurnStatus::Completed,
                error: None,
            })
            .await;
    }
    if !player.idled {
        player.send(BackendEvent::Idle).await;
    }
}

type Parent<'a> = Option<(&'a str, &'a str)>;

impl Player {
    async fn send(&self, event: BackendEvent) -> bool {
        self.events.send(event).await.is_ok()
    }

    fn id(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}-{}", self.counter)
    }

    fn child(parent: Parent<'_>) -> Option<ChildOf> {
        parent.map(|(call, agent)| ChildOf {
            parent_call_id: ItemId(call.into()),
            agent_id: agent.into(),
        })
    }

    fn item_id(&mut self, prefix: &str, parent: Parent<'_>) -> ItemId {
        let id = self.id(prefix);
        ItemId(match parent {
            Some((_, agent)) => format!("{agent}:{id}"),
            None => id,
        })
    }

    fn step<'a>(
        &'a mut self,
        step: &'a Value,
        parent: Parent<'a>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            match step["op"].as_str().unwrap_or_default() {
                op @ ("text" | "thinking") => self.play_text(op, step, parent).await,
                "command" => self.play_command(step, parent).await,
                "usage" => self.play_usage(step).await,
                "approval" => self.play_approval(step, parent).await,
                "subagent" => self.play_subagent(step).await,
                op => self.play_control(op, step).await,
            }
        })
    }

    async fn play_usage(&self, step: &Value) -> bool {
        let usage = TokenUsage {
            input: step["input"].as_u64().unwrap_or(0),
            output: step["output"].as_u64().unwrap_or(0),
            ..TokenUsage::default()
        };
        self.send(BackendEvent::Usage(usage)).await
    }

    async fn play_control(&mut self, op: &str, step: &Value) -> bool {
        match op {
            "sleep" => {
                tokio::time::sleep(Duration::from_millis(step["ms"].as_u64().unwrap_or(0))).await;
                true
            }
            "hang" => {
                self.shared.interrupted.notified().await;
                self.ended = true;
                self.send(BackendEvent::TurnCompleted {
                    status: TurnStatus::Interrupted,
                    error: None,
                })
                .await;
                false
            }
            "turn_end" => {
                self.ended = true;
                let status = match step["status"].as_str() {
                    Some("failed") => TurnStatus::Failed,
                    Some("interrupted") => TurnStatus::Interrupted,
                    _ => TurnStatus::Completed,
                };
                self.send(BackendEvent::TurnCompleted {
                    status,
                    error: None,
                })
                .await
            }
            "idle" => {
                self.idled = true;
                self.send(BackendEvent::Idle).await
            }
            other => {
                self.send(BackendEvent::Error(format!("unknown fake step {other:?}")))
                    .await
            }
        }
    }

    async fn play_text(&mut self, op: &str, step: &Value, parent: Parent<'_>) -> bool {
        let text = step["text"].as_str().unwrap_or_default().to_owned();
        let id = self.item_id(op, parent);
        let known = |text: String| match op {
            "text" => KnownItem::AssistantMessage {
                id: id.clone(),
                text,
            },
            _ => KnownItem::Thinking {
                id: id.clone(),
                text,
            },
        };
        if parent.is_none() {
            let chunks = step["chunks"].as_u64().unwrap_or(1).max(1) as usize;
            let chars: Vec<char> = text.chars().collect();
            if !self
                .send(BackendEvent::ItemStarted(BackendItem::root(known(
                    String::new(),
                ))))
                .await
            {
                return false;
            }
            for part in chars.chunks(chars.len().div_ceil(chunks).max(1)) {
                let delta = part.iter().collect();
                if !self
                    .send(BackendEvent::ItemDelta {
                        item_id: id.clone(),
                        delta,
                    })
                    .await
                {
                    return false;
                }
            }
        }
        self.send(BackendEvent::ItemCompleted(BackendItem {
            item: known(text),
            child_of: Self::child(parent),
        }))
        .await
    }

    async fn play_command(&mut self, step: &Value, parent: Parent<'_>) -> bool {
        let command = step["command"].as_str().unwrap_or_default();
        let call = self.item_id("call", parent);
        let result = self.item_id("result", parent);
        let is_error = step["is_error"].as_bool().unwrap_or(false);
        self.send(BackendEvent::ItemCompleted(BackendItem {
            item: KnownItem::ToolCall {
                id: call.clone(),
                name: "bash".into(),
                arguments: json!({ "command": command }),
            },
            child_of: Self::child(parent),
        }))
        .await
            && self
                .send(BackendEvent::ItemCompleted(BackendItem {
                    item: KnownItem::ToolResult {
                        id: result,
                        call_id: call,
                        output: tool_output(
                            step["output"].as_str().unwrap_or_default(),
                            is_error,
                            Value::Null,
                        ),
                        is_error,
                    },
                    child_of: Self::child(parent),
                }))
                .await
    }

    async fn play_approval(&mut self, step: &Value, parent: Parent<'_>) -> bool {
        let id = self.id("approval");
        let (tx, rx) = oneshot::channel();
        self.shared
            .answers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), tx);
        let request = ApprovalRequest::Permission {
            actor: step["agent"].as_str().map(Into::into),
            call_id: self.id("call"),
            tool: step["tool"].as_str().unwrap_or("bash").into(),
            arguments: step.get("arguments").cloned().unwrap_or(Value::Null),
            surface: "command".into(),
            values: step["values"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect(),
            matched_pattern: None,
            origin: "fake".into(),
            session_patterns: vec![],
            preview: None,
        };
        if !self
            .send(BackendEvent::Approval {
                id: id.clone(),
                request,
            })
            .await
        {
            return false;
        }
        let decision = tokio::select! {
            decision = rx => decision.ok(),
            () = self.shared.interrupted.notified() => {
                self.send(BackendEvent::TurnCompleted {
                    status: TurnStatus::Interrupted,
                    error: None,
                }).await;
                self.ended = true;
                return false;
            }
        };
        let kind = decision
            .and_then(|decision| serde_json::to_value(decision).ok())
            .and_then(|value| value["kind"].as_str().map(str::to_owned))
            .unwrap_or_else(|| "cancelled".into());
        let text = json!({ "op": "text", "text": format!("approval: {kind}") });
        self.step(&text, parent).await
    }

    async fn play_subagent(&mut self, step: &Value) -> bool {
        let agent = step["id"].as_str().unwrap_or("agent").to_owned();
        let name = step["name"].as_str().map(str::to_owned);
        let call = ItemId(format!("spawn-{agent}"));
        let update = |state, message: Option<String>| {
            BackendEvent::Subagent(SubagentUpdate {
                agent_id: agent.clone(),
                parent_call_id: call.clone(),
                name: name.clone(),
                state,
                message,
            })
        };
        let phase = step["phase"].as_str().unwrap_or("full");
        let spawned = phase == "output"
            || self.send_spawn(&agent, name.as_deref(), &call).await
                && self.send(update(SubagentState::Running, None)).await;
        if !spawned {
            return false;
        }
        if phase == "spawn" {
            return true;
        }
        for child in step["steps"].as_array().into_iter().flatten() {
            if !self
                .step(child, Some((call.0.as_str(), agent.as_str())))
                .await
            {
                return false;
            }
        }
        self.send(update(final_state(step), None)).await
    }

    async fn send_spawn(&self, agent: &str, name: Option<&str>, call: &ItemId) -> bool {
        self.send(BackendEvent::ItemCompleted(BackendItem::root(
            KnownItem::ToolCall {
                id: call.clone(),
                name: "spawn_agent".into(),
                arguments: json!({ "agent": agent, "name": name }),
            },
        )))
        .await
            && self
                .send(BackendEvent::ItemCompleted(BackendItem::root(
                    KnownItem::ToolResult {
                        id: ItemId(format!("spawn-{agent}-result")),
                        call_id: call.clone(),
                        output: tool_output("spawned", false, Value::Null),
                        is_error: false,
                    },
                )))
                .await
    }
}

fn final_state(step: &Value) -> SubagentState {
    match step["final"].as_str() {
        Some("failed") => SubagentState::Failed,
        Some("interrupted") => SubagentState::Interrupted,
        Some("waiting") => SubagentState::Waiting,
        Some("running") => SubagentState::Running,
        _ => SubagentState::Completed,
    }
}
