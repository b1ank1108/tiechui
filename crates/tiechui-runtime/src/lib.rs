//! External agent runtimes behind the session protocol: the registry of runtimes, the actor that
//! runs an external thread as a session, and what drivers share to run an external CLI.
//!
//! A driver implements `Backend`; `open_session` returns the `SessionHandle` the app-server
//! already consumes for native sessions.

mod backend;
mod codex;
#[cfg(feature = "fake-runtime")]
pub mod fake;
mod history;
mod process;
mod registry;
mod session;

pub use backend::{
    Backend, BackendEvent, BackendItem, ChildOf, StartParams, Started, Steered, SubagentUpdate,
};
pub use codex::CodexDriver;
pub use history::{DERIVED_ENTRY, ITEM_ENTRY, THREAD_ENTRY};
pub use process::{DeltaCoalescer, Frame, FrameReader, MAX_FRAME, Process};
pub use registry::{
    Driver, Launch, PROBE_TIMEOUT, Probe, Registry, capabilities_of, parse_version, probe_binary,
    resolve_in_path,
};
pub use session::{INTERRUPT_WINDOW, Open, Opened, SessionConfig, open_session};

/// A tool result's `output` in the shape native tools use, so every client renders it alike.
pub fn tool_output(text: &str, is_error: bool, details: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "content": [{ "kind": "text", "text": text }],
        "is_error": is_error,
        "details": details,
    })
}
