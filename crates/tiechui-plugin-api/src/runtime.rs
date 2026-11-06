//! Actor-serialized module state, request projections and idle work.

use crate::{BranchView, ToolError, model::Message, protocol::TurnStatus, serde_json::Value};

pub enum RuntimeEvent {
    Restore {
        navigation: bool,
    },
    Fork {
        entries: Vec<crate::protocol::BranchEntry>,
    },
    BeforeNavigate,
    Input {
        /// The continuation that started this turn; `None` for user input.
        continuation: Option<Continuation>,
        retry: bool,
    },
    Shell,
    Assistant {
        id: String,
        usage: Option<crate::model::Usage>,
        tools: bool,
    },
    Failure(String),
    Settled(TurnStatus),
    Interrupt,
    Extension {
        name: String,
        data: Value,
    },
    Tick,
    Shutdown,
}

#[derive(Default)]
pub struct RuntimeOutput {
    pub entries: Vec<(String, Value)>,
    pub events: Vec<(String, Value)>,
}

#[derive(Clone)]
pub struct Continuation {
    pub kind: String,
    pub generation: u64,
    pub identity: String,
    pub text: String,
    /// Shown in the transcript as the turn's input; the model sees `text` either way.
    pub visible: bool,
}

/// Called in actor order. No callback may block or perform host operations.
pub trait RuntimeExtension: Send + Sync {
    fn event(&self, event: &RuntimeEvent, branch: &BranchView) -> Result<RuntimeOutput, ToolError>;
    fn messages(&self) -> Vec<Message> {
        vec![]
    }
    fn continuation(&self) -> Option<Continuation> {
        None
    }
    fn accepts(&self, continuation: &Continuation) -> bool {
        let _ = continuation;
        false
    }
}
