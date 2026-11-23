//! Agent runtimes: the native `chui` runtime and external ones (Codex, Claude Code, ...) behind
//! the same session protocol. A runtime's static abilities gate the entries only the native
//! runtime implements; sub-agent items and derived turns ride on `Item::Custom`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{ItemId, KnownItem, Op, TurnId};

/// The runtime of sessions opened without one.
pub const DEFAULT_RUNTIME: &str = "chui";

/// The JSON-RPC error code of `RuntimeUnsupported`.
pub const RUNTIME_UNSUPPORTED_CODE: i64 = -32010;

/// `Item::Custom` kind of an item produced inside a sub-agent; payload `SubagentItem`.
pub const SUBAGENT_ITEM_KIND: &str = "runtime.subagent_item";
/// `Item::Custom` kind of a sub-agent's lifecycle status; payload `SubagentStatus`.
pub const SUBAGENT_STATUS_KIND: &str = "runtime.subagent_status";
/// `Item::Custom` kind of the first item of a derived turn; payload `DerivedTurnMarker`.
pub const DERIVED_TURN_KIND: &str = "runtime.derived_turn";

/// The `Op::RunCommand` name that switches the permission mode.
pub const PERMISSIONS_COMMAND: &str = "permissions";

/// What a user input does while a turn runs.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SteeringMode {
    /// Input sent while a turn runs reaches the running turn.
    Inject,
    /// Input waits and starts the next turn once the running one ends.
    Queue,
}

/// What a runtime implements beyond running turns. A client hides the entries a runtime lacks;
/// the server refuses them with `RuntimeUnsupported`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeCapabilities {
    pub steering: SteeringMode,
    /// `session/branch` and `Op::Branch`: moving the conversation to an earlier turn.
    pub branch: bool,
    /// `session/fork`: copying a conversation through a turn into a new session.
    pub fork: bool,
    /// `session/rewind`: conversation rewind. `turn/check` and `turn/undo` are not gated.
    pub rewind: bool,
    /// `Op::Retry`: continuing a failed model request.
    pub retry: bool,
    /// `Op::Compact`.
    pub compact: bool,
    /// `btw/*`: the side conversation.
    pub side_chat: bool,
    /// Commands and skills: `Op::RunCommand` (but for `permissions`) and `Op::CommandInput`.
    pub commands: bool,
    /// `Op::RunCommand` named `permissions`: switching the permission mode.
    pub permissions: bool,
    /// `Op::SetThinking`.
    pub thinking: bool,
    /// `Op::SetModel`, which switches among the native providers' models.
    pub model: bool,
    /// `Op::RunShell`: the user's own shell command kept in the model context.
    pub shell: bool,
}

/// A capability that gates entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feature {
    Branch,
    Fork,
    Rewind,
    Retry,
    Compact,
    SideChat,
    Commands,
    Permissions,
    Thinking,
    Model,
    Shell,
}

/// The ops `RuntimeCapabilities` gates, by the name `RuntimeUnsupported::feature` reports.
pub const GATED_OPS: [(&str, Feature); 9] = [
    ("branch", Feature::Branch),
    ("retry", Feature::Retry),
    ("compact", Feature::Compact),
    ("run_command", Feature::Commands),
    ("command_input", Feature::Commands),
    ("permissions", Feature::Permissions),
    ("set_thinking", Feature::Thinking),
    ("set_model", Feature::Model),
    ("run_shell", Feature::Shell),
];

fn gated_op_name(op: &Op) -> Option<&'static str> {
    Some(match op {
        Op::Branch { .. } => "branch",
        Op::Retry => "retry",
        Op::Compact { .. } => "compact",
        Op::RunCommand { name, .. } if name == PERMISSIONS_COMMAND => "permissions",
        Op::RunCommand { .. } => "run_command",
        Op::CommandInput { .. } => "command_input",
        Op::SetThinking { .. } => "set_thinking",
        Op::SetModel { .. } => "set_model",
        Op::RunShell { .. } => "run_shell",
        _ => return None,
    })
}

impl RuntimeCapabilities {
    /// The native runtime implements everything.
    pub const fn native() -> Self {
        Self {
            steering: SteeringMode::Inject,
            branch: true,
            fork: true,
            rewind: true,
            retry: true,
            compact: true,
            side_chat: true,
            commands: true,
            permissions: true,
            thinking: true,
            model: true,
            shell: true,
        }
    }

    /// A runtime that runs turns, steers by injection and implements nothing else.
    pub const fn minimal() -> Self {
        Self {
            steering: SteeringMode::Inject,
            branch: false,
            fork: false,
            rewind: false,
            retry: false,
            compact: false,
            side_chat: false,
            commands: false,
            permissions: false,
            thinking: false,
            model: false,
            shell: false,
        }
    }

    pub const fn allows(&self, feature: Feature) -> bool {
        match feature {
            Feature::Branch => self.branch,
            Feature::Fork => self.fork,
            Feature::Rewind => self.rewind,
            Feature::Retry => self.retry,
            Feature::Compact => self.compact,
            Feature::SideChat => self.side_chat,
            Feature::Commands => self.commands,
            Feature::Permissions => self.permissions,
            Feature::Thinking => self.thinking,
            Feature::Model => self.model,
            Feature::Shell => self.shell,
        }
    }

    /// The entry of a gated op the runtime lacks.
    pub fn unsupported_op(&self, op: &Op) -> Option<&'static str> {
        let name = gated_op_name(op)?;
        let (_, feature) = GATED_OPS.iter().find(|(gated, _)| *gated == name)?;
        (!self.allows(*feature)).then_some(name)
    }

    /// The method of a gated JSON-RPC call the runtime lacks.
    pub fn unsupported_method<'a>(&self, method: &'a str) -> Option<&'a str> {
        let feature = match method {
            "session/branch" => Feature::Branch,
            "session/fork" => Feature::Fork,
            "session/rewind" => Feature::Rewind,
            _ if method.starts_with("btw/") => Feature::SideChat,
            _ => return None,
        };
        (!self.allows(feature)).then_some(method)
    }
}

impl Default for RuntimeCapabilities {
    fn default() -> Self {
        Self::native()
    }
}

/// One runtime of `runtimes/list`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeInfo {
    pub id: String,
    pub label: String,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub version: Option<String>,
    /// The executable the runtime starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub path: Option<String>,
    /// Why the runtime is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub reason: Option<String>,
    pub capabilities: RuntimeCapabilities,
}

/// The `data` of the JSON-RPC error with code `RUNTIME_UNSUPPORTED_CODE`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RuntimeUnsupported {
    pub runtime: String,
    /// The refused method (`session/branch`, `btw/open`) or op (`retry`, `run_command`).
    pub feature: String,
}

/// An item produced inside a sub-agent, shown under the tool call that spawned it. `id` is the
/// item's own id, unique within the session; completing it again replaces it.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SubagentItem {
    pub id: ItemId,
    /// The `ToolCall` item that spawned the sub-agent.
    pub parent_call_id: ItemId,
    pub agent_id: String,
    pub item: KnownItem,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SubagentState {
    Running,
    Waiting,
    Completed,
    Failed,
    Interrupted,
}

/// A sub-agent's lifecycle, independent of the spawn call, which may complete at once while the
/// sub-agent runs on. One item per sub-agent; each change completes it again, and a client keeps
/// the latest one it saw.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SubagentStatus {
    pub id: ItemId,
    pub parent_call_id: ItemId,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub name: Option<String>,
    pub state: SubagentState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub message: Option<String>,
}

impl SubagentStatus {
    /// The id of the status item of one sub-agent.
    pub fn item_id(agent_id: &str) -> ItemId {
        ItemId(format!("subagent-status:{agent_id}"))
    }
}

/// What opened a derived turn.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DerivedReason {
    /// A sub-agent's output after its parent turn ended.
    Subagent,
    /// An approval request after the turn ended.
    Approval,
    /// The runtime started a turn by itself.
    Runtime,
}

/// The first item of a derived turn: output that reached the session after the turn it belongs
/// to had ended.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DerivedTurnMarker {
    pub id: ItemId,
    pub derived_from: TurnId,
    pub reason: DerivedReason,
}

fn custom<T: Serialize>(kind: &str, payload: &T) -> crate::Item {
    let mut map = match serde_json::to_value(payload) {
        Ok(Value::Object(map)) => map,
        _ => Default::default(),
    };
    map.remove("kind");
    crate::Item::Custom {
        kind: kind.into(),
        payload: map,
    }
}

fn payload_of<T: for<'de> Deserialize<'de>>(item: &crate::Item, expected: &str) -> Option<T> {
    let crate::Item::Custom { kind, payload } = item else {
        return None;
    };
    (kind == expected)
        .then(|| serde_json::from_value(Value::Object(payload.clone())).ok())
        .flatten()
}

impl SubagentItem {
    pub fn into_item(self) -> crate::Item {
        custom(SUBAGENT_ITEM_KIND, &self)
    }

    pub fn from_item(item: &crate::Item) -> Option<Self> {
        payload_of(item, SUBAGENT_ITEM_KIND)
    }
}

impl SubagentStatus {
    pub fn into_item(self) -> crate::Item {
        custom(SUBAGENT_STATUS_KIND, &self)
    }

    pub fn from_item(item: &crate::Item) -> Option<Self> {
        payload_of(item, SUBAGENT_STATUS_KIND)
    }
}

impl DerivedTurnMarker {
    pub fn into_item(self) -> crate::Item {
        custom(DERIVED_TURN_KIND, &self)
    }

    pub fn from_item(item: &crate::Item) -> Option<Self> {
        payload_of(item, DERIVED_TURN_KIND)
    }
}
