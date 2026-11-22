//! Generic ACP (Agent Client Protocol) client over stdio JSON-RPC 2.0.
//!
//! Speaks ACP v1 (`initialize`, `session/new`, `session/resume`, `session/load`, `session/prompt`,
//! `session/cancel`, `session/set_mode`, `session/set_model`, `session/update`,
//! `session/request_permission`, and `elicitation/create`) behind `Backend`, with `AcpDialect`
//! hooks for vendor-specific normalization.

mod dialect;
mod items;
mod requests;
mod translate;
mod wire;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tiechui_protocol::{ApprovalDecision, CommandInfo, InputContent, ModelInfo, ThinkingLevel};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::backend::{Backend, BackendEvent, CommandAction, StartParams, Started, Steered};
use crate::process::{self, Process, StderrTail, seconds};
use crate::registry::resolve_in_path;
use crate::rpc::{CallError, Rpc, RpcError};

pub use dialect::{
    AcpConfig, AcpDialect, AcpLaunchSpec, AcpSubagent, AcpTimeouts, CustomApproval,
    GenericAcpDialect, InitializeInfo, TextDisposition, ToolLocation, ToolState,
};
pub use requests::{form_elicitation_as_questions, interaction_permission_as_questions};
use translate::{AcpTranslator, Control, Shared};
use wire::{ACP_PROTOCOL_VERSION, InitializeWire, SessionSetupWire};

const GENERIC_AUTH_PHRASES: &[&str] = &[
    "unauthenticated",
    "unauthorized",
    "authentication required",
    "authentication failed",
    "invalid authentication",
    "not authenticated",
    "please sign in",
    "sign in required",
    "sign-in required",
    "not logged in",
    "login required",
    "please log in",
    "code 401",
    "\"code\":401",
    "\"code\": 401",
    "http 401",
    "status 401",
    "invalid api key",
    "api key not valid",
    "missing api key",
    "missing credentials",
    "invalid credentials",
    "expired credentials",
    "oauth token",
];

pub(crate) fn debug(message: &str) {
    process::debug("acp", message);
}

struct AcpRun {
    process: Option<Process>,
    rpc: Arc<Rpc>,
    shared: Arc<Shared>,
    tail: StderrTail,
    control: mpsc::Sender<Control>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for AcpRun {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub struct AcpBackend {
    config: AcpConfig,
    run: Option<AcpRun>,
    events_tx: Option<mpsc::WeakSender<BackendEvent>>,
    params: Option<StartParams>,
    available_modes: Vec<String>,
    models: Vec<ModelInfo>,
    current_model: Option<ModelInfo>,
}

impl AcpBackend {
    pub fn new(config: AcpConfig) -> Self {
        Self {
            config,
            run: None,
            events_tx: None,
            params: None,
            available_modes: Vec::new(),
            models: Vec::new(),
            current_model: None,
        }
    }

    fn running(&self) -> Result<&AcpRun, String> {
        self.run
            .as_ref()
            .ok_or_else(|| format!("{} is not running", self.config.dialect.provider_id()))
    }

    fn session_id(&self) -> Result<String, String> {
        self.running()?.shared.session().ok_or_else(|| {
            format!(
                "{} has no active session",
                self.config.dialect.provider_id()
            )
        })
    }

    fn resolve_program(&self, program: &Path) -> Result<PathBuf, String> {
        if program.is_absolute() || program.components().count() > 1 {
            return program
                .is_file()
                .then(|| program.to_path_buf())
                .ok_or_else(|| format!("{} does not exist", program.display()));
        }
        let name = program.to_string_lossy();
        resolve_in_path(&name, self.config.launch.path.as_deref())
            .ok_or_else(|| format!("{name} is not on PATH"))
    }

    fn spawn_run(
        &self,
        params: &StartParams,
        events: mpsc::Sender<BackendEvent>,
    ) -> Result<AcpRun, String> {
        let spec = self.config.dialect.launch(&self.config.launch, params)?;
        let binary = self.resolve_program(&spec.program)?;
        let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
        let mut process = Process::spawn_env(
            &binary,
            &args,
            &params.cwd,
            self.config.launch.path.as_ref(),
            |name| spec.env_remove.iter().any(|k| name.eq_ignore_ascii_case(k)),
            &spec.env_set,
        )
        .map_err(|error| format!("cannot start {}: {error}", binary.display()))?;
        let peer = self.config.dialect.provider_id();
        let (stdin, stdout, stderr) = process.take_stdio(peer)?;
        let rpc = Arc::new(Rpc::new(stdin, peer, true));
        let shared = Arc::new(Shared::default());
        let tail = StderrTail::default();
        let (frames, inbound) = mpsc::channel(256);
        let (control_tx, control_rx) = mpsc::channel(32);
        let translator = AcpTranslator::new(
            shared.clone(),
            rpc.clone(),
            self.config.dialect.clone(),
            events,
            tail.clone(),
            self.config.timeouts.wakeup_idle,
        );
        let tasks = vec![
            tokio::spawn(process::read_frames(peer, stdout, frames)),
            tokio::spawn(process::read_stderr(peer, stderr, tail.clone())),
            tokio::spawn(translator.run(inbound, control_rx)),
        ];
        Ok(AcpRun {
            process: Some(process),
            rpc,
            shared,
            tail,
            control: control_tx,
            tasks,
        })
    }

    async fn handshake(&mut self, params: &StartParams) -> Result<Started, String> {
        let init = self.initialize().await?;
        self.config.dialect.check_initialize(&init)?;
        let cwd = params.cwd.display().to_string();
        let (session_id, setup, resumed) = self
            .open_acp_session(&init, &cwd, params.resume.as_deref())
            .await?;
        let provider = self.config.dialect.provider_id();
        let (model, models) = self
            .resolve_models(provider, &setup, params.model.as_deref())
            .await;
        self.available_modes = setup
            .modes
            .as_ref()
            .map(|m| {
                m.available_modes
                    .iter()
                    .map(|entry| entry.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        let current_mode = setup.modes.as_ref().map(|m| m.current_mode_id.as_str());
        if let Some(requested_mode) = params.mode.as_deref() {
            self.apply_session_mode(&session_id, requested_mode, current_mode)
                .await?;
        }
        let final_model = self
            .maybe_apply_initial_model(&session_id, model, params.model.as_deref())
            .await?;
        self.models = models.clone();
        self.current_model = Some(final_model.clone());
        if models.len() > 1
            && let Some(tx) = self.events_tx.as_ref().and_then(mpsc::WeakSender::upgrade)
        {
            let _ = tx.send(BackendEvent::Models(models)).await;
        }
        Ok(Started {
            external_id: session_id,
            resumed,
            model: final_model,
            thinking: ThinkingLevel::Off,
        })
    }

    async fn initialize(&self) -> Result<InitializeInfo, String> {
        let run = self.running()?;
        let peer = self.config.dialect.provider_id();
        let timeout = self.config.timeouts.handshake;
        let (_, rx) = run
            .rpc
            .send("initialize", wire::initialize_params())
            .await
            .map_err(|e| e.describe(peer, "initialize"))?;
        let reply = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| init_timeout_error(peer, timeout, run.shared.saw_frame()))?;
        let raw = match reply {
            Ok(res) => res.map_err(|err| self.format_rpc_error("initialize", &err))?,
            Err(_) => {
                tokio::task::yield_now().await;
                return Err(format!(
                    "{peer} exited before it answered initialize{}",
                    run.tail.detail()
                ));
            }
        };
        let parsed: InitializeWire =
            serde_json::from_value(raw).map_err(|e| format!("{peer} initialize response: {e}"))?;
        let info = parsed.into_info();
        if info.protocol_version != ACP_PROTOCOL_VERSION {
            return Err(format!(
                "unsupported ACP protocol version: {}",
                info.protocol_version
            ));
        }
        Ok(info)
    }

    async fn open_acp_session(
        &self,
        init: &InitializeInfo,
        cwd: &str,
        resume: Option<&str>,
    ) -> Result<(String, SessionSetupWire, bool), String> {
        if let Some(target_id) = resume {
            match self.try_reopen(init, cwd, target_id).await {
                Ok(Some(setup)) => return Ok((target_id.to_owned(), setup, true)),
                Ok(None) => {}
                Err(fatal) => return Err(fatal),
            }
        }
        let (id, setup) = self.call_session_new(cwd).await?;
        Ok((id, setup, false))
    }

    async fn try_reopen(
        &self,
        init: &InitializeInfo,
        cwd: &str,
        session_id: &str,
    ) -> Result<Option<SessionSetupWire>, String> {
        let methods = reopen_methods(init);
        if methods.is_empty() {
            return Ok(None);
        }
        let run = self.running()?;
        run.shared.set_session(session_id.to_owned());
        for (idx, method) in methods.iter().enumerate() {
            match self.call_reopen_method(run, method, cwd, session_id).await {
                Ok(raw) => {
                    return parse_setup(self.config.dialect.provider_id(), method, raw).map(Some);
                }
                Err(CallError::Rpc(err)) if err.code == -32601 && idx + 1 < methods.len() => {}
                Err(err) => return self.classify_reopen_error(method, err),
            }
        }
        Ok(None)
    }

    async fn call_reopen_method(
        &self,
        run: &AcpRun,
        method: &str,
        cwd: &str,
        session_id: &str,
    ) -> Result<Value, CallError> {
        let params = wire::session_params(cwd, Some(session_id));
        let rpc_id = run.rpc.next_id();
        if method == "session/load" {
            run.shared.begin_load(rpc_id);
        }
        let result = run
            .rpc
            .call_with_id(rpc_id, method, params, self.config.timeouts.session)
            .await;
        if result.is_err() {
            run.shared.clear_load_if(rpc_id);
        }
        result
    }

    fn classify_reopen_error(
        &self,
        method: &str,
        error: CallError,
    ) -> Result<Option<SessionSetupWire>, String> {
        let peer = self.config.dialect.provider_id();
        match error {
            CallError::Timeout | CallError::Closed(_) => Err(error.describe(peer, method)),
            CallError::Rpc(err) => {
                if is_session_unusable(&err) && !self.is_auth_failure(&err) {
                    debug(&format!("{method} rejected saved session: {}", err.message));
                    return Ok(None);
                }
                if err.code == -32002 {
                    debug(&format!("{method} rejected saved session: {}", err.message));
                    return Ok(None);
                }
                Err(self.format_rpc_error(method, &err))
            }
        }
    }

    async fn call_session_new(&self, cwd: &str) -> Result<(String, SessionSetupWire), String> {
        let run = self.running()?;
        let peer = self.config.dialect.provider_id();
        let raw = run
            .rpc
            .call(
                "session/new",
                wire::session_params(cwd, None),
                self.config.timeouts.session,
            )
            .await
            .map_err(|err| match err {
                CallError::Rpc(rpc_err) => self.format_rpc_error("session/new", &rpc_err),
                other => other.describe(peer, "session/new"),
            })?;
        let setup = parse_setup(peer, "session/new", raw)?;
        let session_id = setup
            .session_id
            .clone()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| format!("{peer} session/new returned no sessionId"))?;
        run.shared.set_session(session_id.clone());
        Ok((session_id, setup))
    }

    async fn apply_session_mode(
        &self,
        session_id: &str,
        tiechui_mode: &str,
        current_mode: Option<&str>,
    ) -> Result<(), String> {
        let peer = self.config.dialect.provider_id();
        let Some(target) = self
            .config
            .dialect
            .session_mode(tiechui_mode, &self.available_modes)
        else {
            if !self.available_modes.is_empty()
                && !self
                    .config
                    .dialect
                    .mode_requires_restart(None, tiechui_mode)
            {
                return Err(format!(
                    "{peer} does not support permission mode {tiechui_mode}"
                ));
            }
            return Ok(());
        };
        if current_mode == Some(target.as_str()) {
            return Ok(());
        }
        let run = self.running()?;
        run.rpc
            .call(
                "session/set_mode",
                json!({ "sessionId": session_id, "modeId": target }),
                self.config.timeouts.request,
            )
            .await
            .map(|_| ())
            .map_err(|e| e.describe(peer, "session/set_mode"))
    }

    async fn resolve_models(
        &self,
        provider: &str,
        setup: &SessionSetupWire,
        requested: Option<&str>,
    ) -> (ModelInfo, Vec<ModelInfo>) {
        let has_wire_models = setup
            .models
            .as_ref()
            .is_some_and(|state| !state.available_models.is_empty());
        if !has_wire_models {
            let fallback = self
                .config
                .dialect
                .fallback_models(&self.config.launch)
                .await;
            if let Some(first) = fallback.first() {
                let target = requested.unwrap_or(&first.id);
                let current = fallback
                    .iter()
                    .find(|m| m.id == target)
                    .cloned()
                    .unwrap_or_else(|| first.clone());
                return (current, fallback);
            }
        }
        items::extract_models(provider, setup, requested)
    }

    async fn maybe_apply_initial_model(
        &self,
        session_id: &str,
        default_model: ModelInfo,
        requested: Option<&str>,
    ) -> Result<ModelInfo, String> {
        let Some(wanted) = requested.filter(|m| !m.is_empty() && *m != default_model.id) else {
            return Ok(default_model);
        };
        self.call_set_model(session_id, wanted).await
    }

    async fn call_set_model(&self, session_id: &str, model_id: &str) -> Result<ModelInfo, String> {
        let run = self.running()?;
        let peer = self.config.dialect.provider_id();
        run.rpc
            .call(
                "session/set_model",
                json!({ "sessionId": session_id, "modelId": model_id }),
                self.config.timeouts.request,
            )
            .await
            .map_err(|err| match err {
                CallError::Rpc(rpc_err) => self.format_rpc_error("session/set_model", &rpc_err),
                other => other.describe(peer, "session/set_model"),
            })?;
        let found = self.models.iter().find(|m| m.id == model_id).cloned();
        Ok(found.unwrap_or_else(|| items::model_info_of(peer, model_id, 0)))
    }

    fn format_rpc_error(&self, what: &str, err: &RpcError) -> String {
        let detail = self
            .config
            .dialect
            .format_error(err.code, &err.message, err.data.as_ref());
        if detail != err.message {
            detail
        } else {
            format!("{} {what}: {detail}", self.config.dialect.provider_id())
        }
    }

    fn is_auth_failure(&self, err: &RpcError) -> bool {
        if err.code == 401
            || self
                .config
                .dialect
                .is_auth_error(err.code, &err.message, err.data.as_ref())
        {
            return true;
        }
        let combined = combined_lower(err);
        GENERIC_AUTH_PHRASES
            .iter()
            .any(|needle| combined.contains(needle))
    }

    /// Restarts the underlying ACP child process with an updated permission `mode` and resumes
    /// the current session in place without replacing the event stream.
    pub async fn restart_and_resume(&mut self, mode: Option<String>) -> Result<(), String> {
        let session_id = self.session_id()?;
        let keep_open = self
            .events_tx
            .as_ref()
            .and_then(mpsc::WeakSender::upgrade)
            .ok_or("ACP event channel is closed")?;
        let mut next_params = self.params.clone().ok_or("ACP backend was never started")?;
        next_params.mode = mode;
        next_params.resume = Some(session_id);
        if let Some(current) = &self.current_model {
            next_params.model = Some(current.id.clone());
        }
        self.shutdown_run().await;
        self.run = Some(self.spawn_run(&next_params, keep_open.clone())?);
        let result = self.handshake(&next_params).await;
        match result {
            Ok(_) => {
                drop(keep_open);
                self.params = Some(next_params);
                Ok(())
            }
            Err(error) => {
                self.shutdown_run().await;
                drop(keep_open);
                Err(error)
            }
        }
    }

    async fn seal_derived(&mut self, emit_idle: bool) {
        let Some(run) = &self.run else {
            return;
        };
        let (tx, rx) = oneshot::channel();
        if run
            .control
            .send(Control::SealDerived {
                emit_idle,
                done: tx,
            })
            .await
            .is_ok()
        {
            let _ = tokio::time::timeout(Duration::from_secs(2), rx).await;
        }
    }

    async fn shutdown_run(&mut self) {
        if let Some(mut run) = self.run.take() {
            for task in &run.tasks {
                task.abort();
            }
            for task in run.tasks.drain(..) {
                let _ = task.await;
            }
            if let Some(process) = run.process.take() {
                process.close().await;
            }
        }
    }

    async fn switch_permission_mode(&mut self, mode: &str) -> Result<CommandAction, String> {
        if !matches!(mode, "rules" | "auto" | "yolo") {
            return Err(format!("unknown permission mode {mode:?}"));
        }
        let current = self.params.as_ref().and_then(|p| p.mode.as_deref());
        if self.config.dialect.mode_requires_restart(current, mode) {
            self.restart_for_mode(mode).await?;
        } else {
            let session_id = self.session_id()?;
            self.apply_session_mode(&session_id, mode, None).await?;
            if let Some(params) = &mut self.params {
                params.mode = Some(mode.to_owned());
            }
        }
        Ok(CommandAction::PermissionMode(mode.to_owned()))
    }

    async fn restart_for_mode(&mut self, mode: &str) -> Result<(), String> {
        let run = self.running()?;
        if run.shared.has_active_prompt() || !run.shared.approvals_empty() {
            return Err("cannot switch permission mode while a turn or approval is active".into());
        }
        self.seal_derived(true).await;
        self.restart_and_resume(Some(mode.to_owned())).await
    }
}

fn reopen_methods(init: &InitializeInfo) -> Vec<&'static str> {
    let mut methods = Vec::new();
    if init.resume_session {
        methods.push("session/resume");
    }
    if init.load_session {
        methods.push("session/load");
    }
    methods
}

fn parse_setup(peer: &str, method: &str, raw: Value) -> Result<SessionSetupWire, String> {
    serde_json::from_value(raw).map_err(|e| format!("{peer} {method} response: {e}"))
}

fn combined_lower(err: &RpcError) -> String {
    format!(
        "{} {}",
        err.message.to_ascii_lowercase(),
        err.data
            .as_ref()
            .map(Value::to_string)
            .unwrap_or_default()
            .to_ascii_lowercase()
    )
}

fn is_session_unusable(err: &RpcError) -> bool {
    if matches!(err.code, -32002 | -32601) {
        return true;
    }
    let text = combined_lower(err);
    if !text.contains("session") && !text.contains("conversation") {
        return false;
    }
    [
        "not found",
        "no such",
        "does not exist",
        "unknown session",
        "unknown conversation",
        "invalid session",
        "session expired",
        "corrupt",
        "unusable",
        "cannot load",
        "cannot resume",
        "could not load",
        "could not resume",
    ]
    .iter()
    .any(|phrase| text.contains(phrase))
}

#[async_trait]
impl Backend for AcpBackend {
    async fn start(
        &mut self,
        params: StartParams,
    ) -> Result<(Started, mpsc::Receiver<BackendEvent>), String> {
        if self.run.is_some() {
            return Err(format!(
                "{} is already running",
                self.config.dialect.provider_id()
            ));
        }
        let (events, received) = mpsc::channel(1024);
        self.events_tx = Some(events.downgrade());
        self.run = Some(self.spawn_run(&params, events)?);
        match self.handshake(&params).await {
            Ok(started) => {
                self.params = Some(params);
                Ok((started, received))
            }
            Err(error) => {
                self.shutdown().await;
                Err(error)
            }
        }
    }

    async fn start_turn(&mut self, input: &[InputContent]) -> Result<(), String> {
        let session_id = self.session_id()?;
        let blocks = wire::user_prompt_blocks(input);
        if blocks.is_empty() {
            return Err("a turn requires non-empty input".into());
        }
        let run = self.running()?;
        let rpc_id = run.rpc.next_id();
        run.shared.begin_prompt(rpc_id);
        let result = run
            .rpc
            .write_request(
                rpc_id,
                "session/prompt",
                json!({ "sessionId": session_id, "prompt": blocks }),
            )
            .await;
        if let Err(error) = result {
            run.shared.clear_prompt_if(rpc_id);
            return Err(error);
        }
        Ok(())
    }

    async fn steer(&mut self, _input: &[InputContent]) -> Result<Steered, String> {
        Ok(Steered::Rejected(
            "ACP queues follow-up messages until the active turn completes".into(),
        ))
    }

    async fn interrupt(&mut self) -> Result<(), String> {
        let session_id = self.session_id()?;
        let (rpc, control, events, had_prompt, drained) = {
            let run = self.running()?;
            let had_prompt = run.shared.mark_cancel();
            let drained = run.shared.drain_approvals();
            (
                run.rpc.clone(),
                run.control.clone(),
                self.events_tx.as_ref().and_then(mpsc::WeakSender::upgrade),
                had_prompt,
                drained,
            )
        };
        for (id, pending) in drained {
            let _ = rpc
                .respond(
                    &pending.rpc_id,
                    pending.responder.into_response(&ApprovalDecision::Cancel),
                )
                .await;
            if let Some(tx) = &events {
                let _ = tx.send(BackendEvent::ApprovalWithdrawn { id }).await;
            }
        }
        rpc.notify_params("session/cancel", json!({ "sessionId": session_id }))
            .await?;
        if !had_prompt {
            let _ = control.send(Control::CancelDerived).await;
        }
        Ok(())
    }

    async fn answer_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
    ) -> Result<(), String> {
        let run = self.running()?;
        let Some(pending) = run.shared.take_approval(approval_id) else {
            return Ok(());
        };
        let payload = pending.responder.into_response(&decision);
        run.rpc.respond(&pending.rpc_id, payload).await
    }

    async fn run_command(&mut self, name: &str, arguments: &str) -> Result<CommandAction, String> {
        let clean = name.trim().trim_start_matches('/');
        if clean == "permissions" {
            return self.switch_permission_mode(arguments.trim()).await;
        }
        let probe = CommandInfo {
            name: clean.to_owned(),
            description: String::new(),
            argument_hint: None,
        };
        if !self.config.dialect.filter_command(&probe) {
            return Err(format!("command /{clean} is not available"));
        }
        let args = arguments.trim();
        let prompt = if args.is_empty() {
            format!("/{clean}")
        } else {
            format!("/{clean} {args}")
        };
        Ok(CommandAction::Prompt(prompt))
    }

    async fn set_model(&mut self, model: &str) -> Result<ModelInfo, String> {
        let session_id = self.session_id()?;
        let info = self.call_set_model(&session_id, model).await?;
        self.current_model = Some(info.clone());
        Ok(info)
    }

    async fn finish_derived(&mut self) {
        self.seal_derived(false).await;
    }

    async fn shutdown(&mut self) {
        self.events_tx = None;
        self.shutdown_run().await;
    }
}

fn init_timeout_error(peer: &str, timeout: Duration, saw_frame: bool) -> String {
    if saw_frame {
        format!(
            "{peer} timed out: no answer to initialize within {}",
            seconds(timeout)
        )
    } else {
        format!(
            "{peer} timed out: no output within {} of starting",
            seconds(timeout)
        )
    }
}
