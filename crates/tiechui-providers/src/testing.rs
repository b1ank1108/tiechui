//! File-scripted loopback HTTP server for process-level tests. No real credentials.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Step {
    path: String,
    #[serde(default)]
    body_contains: Vec<String>,
    status: u16,
    #[serde(default)]
    delay_ms: u64,
    body_file: String,
}

struct State {
    next: usize,
    errors: Vec<String>,
}

struct Script {
    steps: Vec<(Step, String)>,
    state: Mutex<State>,
    received: Notify,
}

impl Respond for Script {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let Ok(mut state) = self.state.lock() else {
            return ResponseTemplate::new(500);
        };
        let index = state.next;
        state.next += 1;
        self.received.notify_one();
        let Some((step, body)) = self.steps.get(index) else {
            state
                .errors
                .push(format!("unexpected request {}", index + 1));
            return ResponseTemplate::new(500);
        };
        let text = String::from_utf8_lossy(&request.body);
        if request.method != "POST"
            || request.url.path() != step.path
            || step.body_contains.iter().any(|value| !text.contains(value))
        {
            state
                .errors
                .push(format!("request {} did not match script", index + 1));
            return ResponseTemplate::new(400);
        }
        ResponseTemplate::new(step.status)
            .set_body_raw(
                body.clone(),
                if step.status == 200 {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .set_delay(Duration::from_millis(step.delay_ms))
    }
}

pub struct ScriptedServer {
    server: MockServer,
    script: Arc<Script>,
}

impl ScriptedServer {
    /// A JSON array of ordered POST responses. Body paths are relative to the script.
    pub async fn from_file(path: &Path, cancel: CancellationToken) -> io::Result<Self> {
        if cancel.is_cancelled() {
            return Err(io::Error::other("cancelled"));
        }
        let steps: Vec<Step> = serde_json::from_slice(&std::fs::read(path)?)?;
        let directory = path
            .parent()
            .ok_or_else(|| io::Error::other("missing script directory"))?;
        let steps = steps
            .into_iter()
            .map(|step| {
                let body = std::fs::read_to_string(directory.join(&step.body_file))?;
                Ok((step, body))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let script = Arc::new(Script {
            steps,
            state: Mutex::new(State {
                next: 0,
                errors: vec![],
            }),
            received: Notify::new(),
        });
        let server = tokio::select! {
            _ = cancel.cancelled() => return Err(io::Error::other("cancelled")),
            server = MockServer::builder().disable_request_recording().start() => server,
        };
        let responder = script.clone();
        Mock::given(|_: &Request| true)
            .respond_with(move |request: &Request| responder.respond(request))
            .mount(&server)
            .await;
        Ok(Self { server, script })
    }

    pub fn uri(&self) -> String {
        self.server.uri()
    }

    pub async fn wait_for_requests(
        &self,
        count: usize,
        cancel: CancellationToken,
    ) -> io::Result<()> {
        loop {
            let notified = self.script.received.notified();
            if self
                .script
                .state
                .lock()
                .map_err(|_| io::Error::other("script lock poisoned"))?
                .next
                >= count
            {
                return Ok(());
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err(io::Error::other("cancelled")),
                _ = notified => {}
            }
        }
    }

    pub fn verify(&self) -> io::Result<()> {
        let state = self
            .script
            .state
            .lock()
            .map_err(|_| io::Error::other("script lock poisoned"))?;
        if state.next != self.script.steps.len() || !state.errors.is_empty() {
            return Err(io::Error::other(format!(
                "script consumed {}/{} responses: {:?}",
                state.next,
                self.script.steps.len(),
                state.errors
            )));
        }
        Ok(())
    }
}

/// Construct a real adapter for loopback tests without consulting credential sources.
pub fn provider(
    name: &str,
    config: &tiechui_config::ProviderConfig,
) -> Result<Box<dyn crate::ModelProvider>, crate::ProviderError> {
    let client = crate::client_builder()
        .no_proxy()
        .build()
        .map_err(crate::ProviderError::transport)?;
    match config.wire {
        tiechui_config::Wire::AnthropicMessages => Ok(Box::new(crate::anthropic::Anthropic::new(
            name,
            config,
            "fixture-key",
            client,
        )?)),
        tiechui_config::Wire::OpenaiCompletions => Ok(Box::new(crate::openai::Openai::new(
            name,
            config,
            "fixture-key",
            client,
        )?)),
        tiechui_config::Wire::OpenaiResponses => Ok(Box::new(crate::responses::Responses::new(
            name,
            config,
            "fixture-key",
            client,
        )?)),
    }
}

/// Inspect the complete tool declarations received by a loopback HTTP server.
pub fn tool_definitions(
    wire: tiechui_config::Wire,
    body: &serde_json::Value,
) -> Result<Vec<tiechui_model::ToolSpec>, serde_json::Error> {
    body["tools"].as_array().into_iter().flatten().map(|tool| {
        let value = match wire {
            tiechui_config::Wire::AnthropicMessages => serde_json::json!({
                "name": tool["name"], "description": tool["description"], "input_schema": tool["input_schema"]
            }),
            tiechui_config::Wire::OpenaiCompletions => serde_json::json!({
                "name": tool["function"]["name"], "description": tool["function"]["description"], "input_schema": tool["function"]["parameters"]
            }),
            tiechui_config::Wire::OpenaiResponses => serde_json::json!({
                "name": tool["name"], "description": tool["description"], "input_schema": tool["parameters"]
            }),
        };
        serde_json::from_value(value)
    }).collect()
}
