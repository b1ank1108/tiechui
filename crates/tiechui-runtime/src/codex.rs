//! Codex (`codex app-server`). The probe is complete; the backend is added with the Codex driver.

use async_trait::async_trait;
use tiechui_protocol::RuntimeCapabilities;

use crate::backend::Backend;
use crate::registry::{Driver, Launch, Probe, probe_binary};

pub struct CodexDriver;

#[async_trait]
impl Driver for CodexDriver {
    fn id(&self) -> &'static str {
        "codex"
    }

    fn label(&self) -> &'static str {
        "Codex"
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::minimal()
    }

    fn gated_ops(&self) -> &'static [&'static str] {
        &[]
    }

    async fn probe(&self, launch: &Launch) -> Probe {
        probe_binary("codex", "--version", launch).await
    }

    fn backend(&self, _: &Launch) -> Result<Box<dyn Backend>, String> {
        Err("the codex runtime is not implemented yet".into())
    }
}
