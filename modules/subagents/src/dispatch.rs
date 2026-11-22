use std::{
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tiechui_plugin_api::{
    futures::{FutureExt, StreamExt},
    model::{ToolCall, Usage},
    protocol::*,
    serde_json::{self, Value, json},
    *,
};
use tokio::sync::{mpsc, watch};

use crate::{
    MAX_PARALLEL_TASKS, Service,
    collector::{Collector, read_completed_output, resolve_task_state},
    definitions, output,
    params::{
        PreparedTask, SendParams, parameters, parse_send_params, prepare_tasks, resolve_thinking,
    },
    state::{ActiveDispatch, ActiveTask, StopCause, now_ms},
};

struct DoneSignal(watch::Sender<bool>);

impl Drop for DoneSignal {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

struct ForegroundGuard<'a> {
    service: &'a Service,
    id: String,
    armed: bool,
}

impl Drop for ForegroundGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.service.interrupt_foreground_dispatch(&self.id);
        }
    }
}

struct TaskRun {
    dispatch_id: String,
    call_id: String,
    session_gen: u64,
    index: usize,
    task: PreparedTask,
    ctx: ToolContext,
    cancel: CancellationToken,
    steer_rx: Option<mpsc::UnboundedReceiver<String>>,
    done_tx: watch::Sender<bool>,
}

struct BatchSpec {
    dispatch_id: String,
    call_id: String,
    mode: String,
    session_gen: u64,
    ctx: ToolContext,
    cancel: CancellationToken,
}

struct StartedDispatch {
    dispatch_id: String,
    session_gen: u64,
    all_background: bool,
    record: AgentDispatch,
    done: watch::Receiver<bool>,
}

enum WaitStep {
    SessionChanged(watch::Receiver<bool>),
    Promoted(AgentDispatch),
    Done(AgentDispatch),
    Running,
}

struct PersistedChildMeta {
    cwd: PathBuf,
    model: Option<String>,
    thinking: Option<ThinkingLevel>,
}

struct ResumeSeed {
    id: String,
    name: Option<String>,
    agent: String,
    model: String,
    fork: bool,
}

enum SteerAttempt {
    Queued(ToolOutput),
    WaitThenResume(watch::Receiver<bool>),
    Resume(ResumeSeed),
}

fn blank_agent_task(task: &PreparedTask, started_at: i64) -> AgentTask {
    let model = if task.resumed {
        task.model.clone().unwrap_or_default()
    } else {
        String::new()
    };
    AgentTask {
        id: task.id.clone(),
        name: task.name.clone(),
        background: task.background,
        fork: task.fork,
        started_at,
        agent: task.label.clone(),
        task: task.task.clone(),
        state: AgentState::Waiting,
        output: String::new(),
        thinking: String::new(),
        diagnostic: None,
        output_path: None,
        thinking_path: None,
        usage: TokenUsage::default(),
        turns: 0,
        model,
    }
}

fn verify_workspace_cwd(parent_cwd: &Path, candidate: &Path) -> Result<PathBuf, ToolError> {
    let cwd = std::fs::canonicalize(candidate)?;
    let boundary = std::fs::canonicalize(parent_cwd)?;
    if !cwd.starts_with(&boundary) || !cwd.is_dir() {
        return Err(ToolError(
            "Child cwd must remain inside the parent workspace".into(),
        ));
    }
    Ok(cwd)
}

fn resolve_child_cwd(parent_cwd: &Path, child: Option<&str>) -> Result<PathBuf, ToolError> {
    verify_workspace_cwd(parent_cwd, &parent_cwd.join(child.unwrap_or(".")))
}

fn validate_safe_segment(raw: &str, label: &str) -> Result<(), ToolError> {
    let mut components = Path::new(raw).components();
    let valid = !raw.is_empty()
        && !raw.contains(['/', '\\', '\0'])
        && matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none();
    if !valid {
        return Err(ToolError(format!("Invalid {label} '{raw}'")));
    }
    Ok(())
}

pub(crate) fn child_session_path(
    home: &Path,
    session_id: &str,
    agent_id: &str,
) -> Result<Option<PathBuf>, ToolError> {
    if session_id.is_empty() || agent_id.is_empty() {
        return Ok(None);
    }
    validate_safe_segment(session_id, "session identifier")?;
    validate_safe_segment(agent_id, "subagent identifier")?;
    Ok(Some(
        home.join("state/subagents/sessions")
            .join(session_id)
            .join(format!("{agent_id}.jsonl")),
    ))
}

fn wrap_fork_directive(task: &str) -> String {
    format!(
        "<fork_boilerplate>\n\
You are a forked subagent worker. You are NOT the main session agent.\n\
Rules:\n\
1. Do NOT call subagent, subagent_send, subagent_stop, or subagent_list. Execute your task directly using workspace tools.\n\
2. Do NOT ask the user questions or converse; stay strictly within the assigned directive scope.\n\
3. Report concise factual findings and any files modified, then finish.\n\
</fork_boilerplate>\n\n\
Directive: {task}"
    )
}

fn fork_resumed_task(prev: ResumeSeed, cwd: PathBuf, params: SendParams) -> PreparedTask {
    PreparedTask {
        id: prev.id,
        name: prev.name,
        background: params.background,
        fork: true,
        resumed: true,
        label: "fork".into(),
        custom_agent: None,
        task: params.message,
        cwd: None,
        resolved_cwd: Some(cwd),
        model: (!prev.model.is_empty()).then_some(prev.model),
        thinking: None,
    }
}

fn resolve_task_cwd(run: &TaskRun) -> Result<PathBuf, ToolError> {
    match &run.task.resolved_cwd {
        Some(resolved) => verify_workspace_cwd(&run.ctx.cwd, resolved),
        None => resolve_child_cwd(&run.ctx.cwd, run.task.cwd.as_deref()),
    }
}

fn resolve_child_prompt(task: &PreparedTask) -> (ChildMode, String, Option<String>) {
    if !task.fork {
        return (ChildMode::Standard, task.task.clone(), task.model.clone());
    }
    let prompt = if task.resumed {
        task.task.clone()
    } else {
        wrap_fork_directive(&task.task)
    };
    (ChildMode::Fork, prompt, None)
}

fn open_persisted_file(path: &Path, task_id: &str) -> Result<std::fs::File, ToolError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolError(format!(
                "No persisted session found for subagent '{task_id}'"
            )));
        }
        Err(err) => return Err(err.into()),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(ToolError(format!(
            "Invalid persisted session file for subagent '{task_id}'"
        )));
    }
    Ok(std::fs::File::open(path)?)
}

fn parse_header_cwd(line: &str, task_id: &str, parent_cwd: &Path) -> Result<PathBuf, ToolError> {
    let header: Value = serde_json::from_str(line)
        .map_err(|e| ToolError(format!("Invalid session header for '{task_id}': {e}")))?;
    let valid_id = header["id"].as_str().is_some_and(|s| !s.is_empty());
    let Some(raw_cwd) = header["cwd"]
        .as_str()
        .filter(|_| header["format_version"] == 1 && valid_id)
    else {
        return Err(ToolError(format!("Invalid session header for '{task_id}'")));
    };
    let cwd = Path::new(raw_cwd);
    if !cwd.is_absolute() {
        return Err(ToolError(format!(
            "Invalid session cwd for subagent '{task_id}'"
        )));
    }
    verify_workspace_cwd(parent_cwd, cwd)
}

fn apply_persisted_entry(
    entry: &Value,
    model: &mut Option<String>,
    thinking: &mut Option<ThinkingLevel>,
) {
    match entry["kind"].as_str() {
        Some("model_change") => {
            if let Some(m) = entry["model"]["model"].as_str() {
                *model = Some(m.to_owned());
            }
        }
        Some("thinking_change") => {
            if entry["level"].is_null() {
                *thinking = Some(ThinkingLevel::Off);
            } else if let Some(ChildThinking::Level(level)) = entry["level"]
                .as_str()
                .and_then(|s| parse_child_thinking(s).ok())
            {
                *thinking = Some(level);
            }
        }
        _ => {}
    }
}

fn inspect_persisted_child(
    path: &Path,
    task_id: &str,
    parent_cwd: &Path,
) -> Result<PersistedChildMeta, ToolError> {
    let file = open_persisted_file(path, task_id)?;
    let mut first = String::new();
    let mut head_reader = BufReader::new(&file).take(1024 * 1024);
    head_reader.read_line(&mut first)?;
    let cwd = parse_header_cwd(&first, task_id, parent_cwd)?;
    let mut reader = head_reader.into_inner();
    let mut model = None;
    let mut thinking = None;
    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 {
        if let Ok(entry) = serde_json::from_str::<Value>(&line) {
            apply_persisted_entry(&entry, &mut model, &mut thinking);
        }
        line.clear();
    }
    Ok(PersistedChildMeta {
        cwd,
        model,
        thinking,
    })
}

fn queued_steer_output(task_id: &str, task_name: &Option<String>) -> ToolOutput {
    let label = match task_name {
        Some(name) => format!("{task_id} ({name})"),
        None => task_id.to_owned(),
    };
    ToolOutput {
        structured_content: Some(json!({
            "status": "queued",
            "agent_id": task_id,
            "name": task_name,
        })),
        ..ToolOutput::text(format!(
            "Queued message for subagent {label}; it will be delivered at the next tool or turn boundary."
        ))
    }
}

fn lookup_resume_agent(
    home: &Path,
    cwd: &Path,
    agent_name: &str,
    cancel: &CancellationToken,
) -> Result<Option<definitions::Definition>, ToolError> {
    if agent_name == "default" {
        return Ok(None);
    }
    let catalog = definitions::discover_catalog(home, cwd, "both", cancel)?;
    if let Some((path, reason)) = catalog.invalid.get(agent_name) {
        return Err(ToolError(format!(
            "Invalid agent definition '{agent_name}' in {}: {reason}",
            path.display()
        )));
    }
    Ok(catalog.agents.get(agent_name).cloned())
}

pub(crate) fn agent_model_label(task: &AgentTask) -> String {
    if task.model.is_empty() {
        task.agent.clone()
    } else {
        format!("{} · {}", task.agent, task.model)
    }
}

fn summary_header_label(task: &AgentTask) -> String {
    let base = agent_model_label(task);
    if task.id.is_empty() {
        base
    } else {
        format!("{} · {base}", task.id)
    }
}

fn format_task_summary(task: &AgentTask) -> String {
    let mut text = format!(
        "### [{}] {:?}\n\n{}",
        summary_header_label(task),
        task.state,
        output::prefix(&task.output, output::PREVIEW_BYTES)
    );
    if let Some(error) = &task.diagnostic {
        text.push_str(&format!("\n{error}"));
    }
    if let Some(path) = &task.output_path {
        text.push_str(&format!("\nFull output: {path} (retrieve with read)"));
    }
    text
}

pub(crate) fn task_json(task: &AgentTask) -> Value {
    json!({
        "agent_id": task.id,
        "id": task.id,
        "name": task.name,
        "agent": task.agent,
        "background": task.background,
        "fork": task.fork,
        "state": task.state,
        "model": task.model,
        "turns": task.turns,
        "output_path": task.output_path,
    })
}

pub(crate) fn result(record: &AgentDispatch) -> ToolOutput {
    let text = if record.mode != "parallel"
        && record
            .tasks
            .iter()
            .all(|t| t.state == AgentState::Completed)
    {
        let last = record.tasks.last();
        let body = last
            .map(|task| task.output.as_str())
            .filter(|out| !out.is_empty())
            .unwrap_or("(no output)");
        match last.filter(|task| !task.id.is_empty()) {
            Some(task) => format!("{body}\n\n[agent_id: {}]", task.id),
            None => body.to_owned(),
        }
    } else {
        record
            .tasks
            .iter()
            .map(format_task_summary)
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    ToolOutput {
        details: Some(json!(record)),
        structured_content: Some(json!({
            "dispatch_id": record.id,
            "mode": record.mode,
            "background": record.background,
            "agents": record.tasks.iter().map(task_json).collect::<Vec<_>>(),
        })),
        is_error: record
            .tasks
            .iter()
            .any(|t| matches!(t.state, AgentState::Failed | AgentState::Cancelled)),
        ..ToolOutput::text(text)
    }
}

fn describe_async_agent(task: &AgentTask) -> String {
    match &task.name {
        Some(name) => format!(
            "{} (name: {name}, agent: {}, {:?})",
            task.id, task.agent, task.state
        ),
        None => format!("{} (agent: {}, {:?})", task.id, task.agent, task.state),
    }
}

fn async_output(record: &AgentDispatch, output_dir: &Path, promoted: bool) -> ToolOutput {
    let agents_list = record
        .tasks
        .iter()
        .map(describe_async_agent)
        .collect::<Vec<_>>()
        .join(", ");
    let lead = if promoted {
        format!("Subagent dispatch moved to the background: {agents_list}.")
    } else {
        format!("Subagent launched in the background: {agents_list}.")
    };
    let text = format!(
        "{lead}\nOutput archive directory: {}\nYou will be notified automatically via <task-notification> when complete. Do not poll or sleep waiting for it.",
        output_dir.display()
    );
    ToolOutput {
        details: Some(json!(record)),
        structured_content: Some(json!({
            "status": "async_launched",
            "promoted": promoted,
            "dispatch_id": record.id,
            "background": true,
            "output_dir": output_dir.display().to_string(),
            "agents": record.tasks.iter().map(task_json).collect::<Vec<_>>(),
        })),
        ..ToolOutput::text(text)
    }
}

pub(crate) async fn wait_done(mut receivers: Vec<watch::Receiver<bool>>, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    for rx in &mut receivers {
        while !*rx.borrow_and_update() {
            let changed = tokio::select! {
                res = rx.changed() => res.is_ok(),
                _ = tokio::time::sleep_until(deadline) => false,
            };
            if !changed {
                break;
            }
        }
    }
}

fn has_active_tasks(record: &AgentDispatch, background: bool) -> bool {
    record
        .tasks
        .iter()
        .any(|t| t.background == background && t.state.active())
}

fn detach_mixed_background(state: &mut crate::state::State, id: &str) {
    if let Some(rec) = state.records.get_mut(id)
        && !rec.background
    {
        rec.background = true;
        state.push_record_entry(id);
        state.revision = state.revision.wrapping_add(1);
    }
    if let Some(disp) = state.dispatches.get_mut(id) {
        disp.detached = true;
    }
}

fn classify_record_progress(
    state: &mut crate::state::State,
    id: &str,
    record: AgentDispatch,
) -> WaitStep {
    if has_active_tasks(&record, false) {
        return WaitStep::Running;
    }
    let bg_active = has_active_tasks(&record, true);
    if bg_active && record.tasks.iter().all(|t| t.background) {
        if let Some(disp) = state.dispatches.get_mut(id) {
            disp.detached = true;
        }
        return WaitStep::Promoted(record);
    }
    if bg_active {
        detach_mixed_background(state, id);
    }
    state.mark_dispatch_seen(id);
    WaitStep::Done(state.records.get(id).cloned().unwrap_or(record))
}

fn take_dispatch_task_cancels(
    state: &mut crate::state::State,
    id: &str,
) -> std::collections::BTreeMap<String, bool> {
    let ids: Vec<String> = state
        .records
        .get(id)
        .map(|r| r.tasks.iter().map(|t| t.id.clone()).collect())
        .unwrap_or_default();
    ids.into_iter()
        .map(|tid| {
            let cancelled = state
                .tasks
                .remove(&tid)
                .is_some_and(|entry| entry.cancel.is_cancelled());
            (tid, cancelled)
        })
        .collect()
}

fn finalize_unsettled_tasks(
    record: &mut AgentDispatch,
    cancels: &std::collections::BTreeMap<String, bool>,
    dispatch_cancelled: bool,
) {
    for task in &mut record.tasks {
        if task.state.active() {
            let cancelled = dispatch_cancelled || cancels.get(&task.id).copied().unwrap_or(false);
            task.state = if cancelled {
                AgentState::Cancelled
            } else {
                AgentState::Skipped
            };
        }
    }
}

impl Service {
    pub(crate) fn flush_entries(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        let entries = {
            let mut state = self.lock_state()?;
            if state.session != ctx.scope.thread_id.0 {
                return Ok(());
            }
            std::mem::take(&mut state.pending_entries)
        };
        for (kind, data) in entries {
            (ctx.append_entry)(tools::CustomSessionEntry { kind, data })?;
        }
        Ok(())
    }

    fn publish_foreground(
        &self,
        dispatch_id: &str,
        call_id: &str,
        session_gen: u64,
        ctx: &ToolContext,
    ) {
        let snapshot = self.lock_state().ok().and_then(|state| {
            if state.session_gen != session_gen || state.session != ctx.scope.thread_id.0 {
                return None;
            }
            let detached = state
                .dispatches
                .get(dispatch_id)
                .is_some_and(|d| d.detached);
            let record = state.records.get(dispatch_id)?;
            (!detached && !record.background).then(|| result(record))
        });
        if let Some(output) = snapshot {
            (ctx.updates)(tools::ToolUpdate {
                call_id: call_id.into(),
                epoch: ctx.epoch,
                output,
            });
        }
    }

    async fn confirm_project_agents(
        &self,
        tasks: &[PreparedTask],
        cancel: &CancellationToken,
    ) -> Result<(), ToolError> {
        if self.trusted {
            return Ok(());
        }
        let mut project = Vec::new();
        for agent in tasks
            .iter()
            .filter_map(|task| task.custom_agent.as_ref())
            .filter(|agent| agent.project)
        {
            if !project.contains(&agent.path) {
                project.push(agent.path.clone());
            }
        }
        if project.is_empty() {
            return Ok(());
        }
        let confirmation = self.confirmation.as_ref().ok_or_else(|| {
            ToolError("Untrusted project agents require an interactive terminal".into())
        })?;
        if !confirmation.confirm(project, cancel.clone()).await? {
            return Err(ToolError("Project agent execution declined".into()));
        }
        Ok(())
    }

    fn start_dispatch(
        self: &Arc<Self>,
        id: &str,
        mode: &str,
        mut tasks: Vec<PreparedTask>,
        runner: Arc<dyn ChildRunner>,
        ctx: &ToolContext,
    ) -> Result<StartedDispatch, ToolError> {
        let mut state = self.lock_state()?;
        if state.session != ctx.scope.thread_id.0 {
            return Err(ToolError("Session changed before dispatch".into()));
        }
        state.ensure_available_names(&tasks)?;
        for task in &mut tasks {
            if task.id.is_empty() {
                task.id = state.allocate_id();
            } else if state.tasks.contains_key(&task.id) {
                return Err(ToolError(format!(
                    "Subagent '{}' is already active; retry subagent_send to queue steering",
                    task.id
                )));
            }
        }
        let dispatch_id = if state.records.contains_key(id) {
            format!("{id}#{}", state.order.len() + 1)
        } else {
            id.to_owned()
        };
        let started_at = now_ms();
        let all_background = tasks.iter().all(|t| t.background);
        let record = AgentDispatch {
            id: dispatch_id.clone(),
            session_id: ctx.scope.thread_id.0.clone(),
            mode: mode.into(),
            background: all_background,
            started_at,
            tasks: tasks
                .iter()
                .map(|t| blank_agent_task(t, started_at))
                .collect(),
        };
        let session_gen = state.session_gen;
        state.order.push(dispatch_id.clone());
        state.records.insert(dispatch_id.clone(), record.clone());
        state.push_record_entry(&dispatch_id);
        let dispatch_cancel = CancellationToken::new();
        let (disp_done_tx, disp_done_rx) = watch::channel(false);
        state.dispatches.insert(
            dispatch_id.clone(),
            ActiveDispatch {
                mode: mode.into(),
                cancel: dispatch_cancel.clone(),
                done: disp_done_rx.clone(),
                detached: all_background,
                stop: None,
                notified: false,
            },
        );
        let runs = register_task_runs(
            &mut state,
            (&dispatch_id, id),
            mode,
            session_gen,
            tasks,
            ctx,
            &dispatch_cancel,
        );
        state.revision = state.revision.wrapping_add(1);
        drop(state);

        let service = self.clone();
        let batch = BatchSpec {
            dispatch_id: dispatch_id.clone(),
            call_id: id.to_owned(),
            mode: mode.to_owned(),
            session_gen,
            ctx: ctx.clone(),
            cancel: dispatch_cancel,
        };
        tokio::spawn(async move {
            let _done = DoneSignal(disp_done_tx);
            service.run_batch(batch, runs, runner).await;
        });
        self.changed();
        Ok(StartedDispatch {
            dispatch_id,
            session_gen,
            all_background,
            record,
            done: disp_done_rx,
        })
    }

    fn inspect_progress(&self, id: &str, session_gen: u64) -> Result<WaitStep, ToolError> {
        let mut state = self.lock_state()?;
        if state.session_gen != session_gen {
            let done = state
                .dispatches
                .get(id)
                .map(|d| d.done.clone())
                .unwrap_or_else(|| watch::channel(true).1);
            return Ok(WaitStep::SessionChanged(done));
        }
        let record = state
            .records
            .get(id)
            .cloned()
            .ok_or_else(|| ToolError("Dispatch disappeared".into()))?;
        Ok(classify_record_progress(&mut state, id, record))
    }

    async fn prepare_dispatch(
        &self,
        arguments: &Value,
        ctx: &ToolContext,
    ) -> Result<(&'static str, Vec<PreparedTask>, Arc<dyn ChildRunner>), ToolError> {
        if !ctx.scope.thread_id.0.is_empty() {
            validate_safe_segment(&ctx.scope.thread_id.0, "session identifier")?;
        }
        let (mode, top_bg, raw_tasks) = parameters(arguments)?;
        let scope = arguments["agentScope"].as_str().unwrap_or("user");
        let catalog = definitions::discover_catalog(&self.home, &ctx.cwd, scope, &ctx.cancel)?;
        let runner = self.bound_runner()?;
        let tasks = prepare_tasks(mode, top_bg, raw_tasks, &catalog, runner.as_ref())?;
        self.confirm_project_agents(&tasks, &ctx.cancel).await?;
        Ok((mode, tasks, runner))
    }

    async fn launch_and_finish(
        self: &Arc<Self>,
        call_id: &str,
        mode: &str,
        tasks: Vec<PreparedTask>,
        runner: Arc<dyn ChildRunner>,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError("Dispatch cancelled".into()));
        }
        let mut changes = self.changes.subscribe();
        let output_dir = self.home.join("state/subagents/output");
        let started = self.start_dispatch(call_id, mode, tasks, runner, ctx)?;
        self.flush_entries(ctx)?;
        if started.all_background {
            return Ok(async_output(&started.record, &output_dir, false));
        }
        self.publish_foreground(&started.dispatch_id, call_id, started.session_gen, ctx);
        self.wait_foreground(
            (&started.dispatch_id, call_id),
            started.session_gen,
            started.done,
            &mut changes,
            ctx,
        )
        .await
    }

    pub(crate) async fn dispatch(
        self: &Arc<Self>,
        call: ToolCall,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let (mode, tasks, runner) = self.prepare_dispatch(&call.arguments, &ctx).await?;
        self.launch_and_finish(&call.id, mode, tasks, runner, &ctx)
            .await
    }

    fn try_steer_active(
        &self,
        params: &SendParams,
        ctx: &ToolContext,
    ) -> Result<SteerAttempt, ToolError> {
        let state = self.lock_state()?;
        if state.session != ctx.scope.thread_id.0 {
            return Err(ToolError("Subagents belong to another session".into()));
        }
        let prev = state.resolve_send_task(&params.to)?;
        if let Some(active) = state.tasks.get(&prev.id) {
            if active.steer.send(params.message.clone()).is_ok() {
                return Ok(SteerAttempt::Queued(queued_steer_output(
                    &prev.id, &prev.name,
                )));
            }
            return Ok(SteerAttempt::WaitThenResume(active.done.clone()));
        }
        let resumed_name = prev
            .name
            .as_deref()
            .filter(|name| state.name_resolves_to(name, &prev.id))
            .map(str::to_owned);
        Ok(SteerAttempt::Resume(ResumeSeed {
            id: prev.id,
            name: resumed_name,
            agent: prev.agent,
            model: prev.model,
            fork: prev.fork,
        }))
    }

    async fn load_persisted_child(
        &self,
        task_id: &str,
        ctx: &ToolContext,
    ) -> Result<PersistedChildMeta, ToolError> {
        let store_path = child_session_path(&self.home, &ctx.scope.thread_id.0, task_id)?
            .ok_or_else(|| {
                ToolError(format!(
                    "No persisted session found for subagent '{task_id}'"
                ))
            })?;
        let parent_cwd = ctx.cwd.clone();
        let owned_id = task_id.to_owned();
        tokio::task::spawn_blocking(move || {
            inspect_persisted_child(&store_path, &owned_id, &parent_cwd)
        })
        .await
        .map_err(|e| ToolError(e.to_string()))?
    }

    fn build_standard_resumed_task(
        &self,
        prev: ResumeSeed,
        persisted: PersistedChildMeta,
        params: SendParams,
        runner: &dyn ChildRunner,
        ctx: &ToolContext,
    ) -> Result<PreparedTask, ToolError> {
        let custom_agent = lookup_resume_agent(&self.home, &ctx.cwd, &prev.agent, &ctx.cancel)?;
        let model = (!prev.model.is_empty())
            .then_some(prev.model)
            .or(persisted.model);
        if let Some(m) = model.as_deref() {
            runner.validate_model(m)?;
        }
        let thinking = persisted
            .thinking
            .or_else(|| resolve_thinking(None, custom_agent.as_ref().and_then(|d| d.thinking)));
        Ok(PreparedTask {
            id: prev.id,
            name: prev.name,
            background: params.background,
            fork: false,
            resumed: true,
            label: prev.agent,
            custom_agent,
            task: params.message,
            cwd: None,
            resolved_cwd: Some(persisted.cwd),
            model,
            thinking,
        })
    }

    async fn prepare_resume(
        &self,
        prev: ResumeSeed,
        params: SendParams,
        ctx: &ToolContext,
    ) -> Result<(PreparedTask, Arc<dyn ChildRunner>), ToolError> {
        let persisted = self.load_persisted_child(&prev.id, ctx).await?;
        let runner = self.bound_runner()?;
        if prev.fork {
            return Ok((fork_resumed_task(prev, persisted.cwd, params), runner));
        }
        let prepared =
            self.build_standard_resumed_task(prev, persisted, params, runner.as_ref(), ctx)?;
        self.confirm_project_agents(std::slice::from_ref(&prepared), &ctx.cancel)
            .await?;
        Ok((prepared, runner))
    }

    pub(crate) async fn send(
        self: &Arc<Self>,
        call: ToolCall,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let params = parse_send_params(&call.arguments)?;
        let prev = match self.try_steer_active(&params, &ctx)? {
            SteerAttempt::Queued(out) => return Ok(out),
            SteerAttempt::Resume(prev) => prev,
            SteerAttempt::WaitThenResume(done) => {
                wait_done(vec![done], Duration::from_secs(5)).await;
                match self.try_steer_active(&params, &ctx)? {
                    SteerAttempt::Queued(out) => return Ok(out),
                    SteerAttempt::Resume(prev) => prev,
                    SteerAttempt::WaitThenResume(_) => {
                        return Err(ToolError(
                            "Subagent is still settling; retry subagent_send".into(),
                        ));
                    }
                }
            }
        };
        let (prepared, runner) = self.prepare_resume(prev, params, &ctx).await?;
        self.launch_and_finish(&call.id, "single", vec![prepared], runner, &ctx)
            .await
    }

    async fn wait_foreground(
        &self,
        (id, call_id): (&str, &str),
        session_gen: u64,
        done: watch::Receiver<bool>,
        changes: &mut watch::Receiver<u64>,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let mut guard = ForegroundGuard {
            service: self,
            id: id.to_owned(),
            armed: true,
        };
        let mut interrupted = false;
        let output_dir = self.home.join("state/subagents/output");
        loop {
            match self.inspect_progress(id, session_gen)? {
                WaitStep::SessionChanged(rx) => {
                    guard.armed = false;
                    wait_done(vec![rx, done], Duration::from_secs(5)).await;
                    return Err(ToolError("Session changed during dispatch".into()));
                }
                WaitStep::Promoted(record) => {
                    guard.armed = false;
                    self.flush_entries(ctx)?;
                    return Ok(async_output(&record, &output_dir, true));
                }
                WaitStep::Done(record) => {
                    guard.armed = false;
                    self.flush_entries(ctx)?;
                    self.publish_foreground(id, call_id, session_gen, ctx);
                    return Ok(result(&record));
                }
                WaitStep::Running => {}
            }
            tokio::select! {
                res = changes.changed() => {
                    if res.is_err() {
                        break;
                    }
                }
                () = ctx.cancel.cancelled(), if !interrupted => {
                    interrupted = true;
                    self.interrupt_foreground_dispatch(id);
                }
            }
        }
        Err(ToolError("Subagent channel closed".into()))
    }

    async fn run_batch(
        self: &Arc<Self>,
        batch: BatchSpec,
        runs: Vec<TaskRun>,
        runner: Arc<dyn ChildRunner>,
    ) {
        if batch.mode == "parallel" {
            let mut jobs = futures::stream::iter(
                runs.into_iter()
                    .map(|run| self.run_task(run, runner.clone())),
            )
            .buffer_unordered(MAX_PARALLEL_TASKS);
            while jobs.next().await.is_some() {}
        } else {
            let mut previous = String::new();
            for mut run in runs {
                run.task.task = run.task.task.replace("{previous}", &previous);
                match self.run_task(run, runner.clone()).await {
                    Ok(out) => previous = out,
                    Err(_) => break,
                }
            }
        }
        self.finish_dispatch(&batch);
    }

    fn finish_dispatch(&self, batch: &BatchSpec) {
        let Ok(mut state) = self.lock_state() else {
            return;
        };
        if state.session_gen != batch.session_gen {
            return;
        }
        let id = batch.dispatch_id.as_str();
        let cancels = take_dispatch_task_cancels(&mut state, id);
        if let Some(record) = state.records.get_mut(id) {
            finalize_unsettled_tasks(record, &cancels, batch.cancel.is_cancelled());
        }
        state.queue_chain_notification(id);
        state.dispatches.remove(id);
        state.push_record_entry(id);
        state.revision = state.revision.wrapping_add(1);
        drop(state);
        self.publish_foreground(id, &batch.call_id, batch.session_gen, &batch.ctx);
        self.changed();
    }

    async fn acquire_slot(
        &self,
        cancel: &CancellationToken,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError("Dispatch cancelled".into()));
        }
        let permit = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(ToolError("Dispatch cancelled".into())),
            permit = self.slots.clone().acquire_owned() => {
                permit.map_err(|_| ToolError("Subagent semaphore closed".into()))?
            }
        };
        if cancel.is_cancelled() {
            return Err(ToolError("Dispatch cancelled".into()));
        }
        Ok(permit)
    }

    fn mark_running(&self, run: &TaskRun, prompt: &str) -> Result<(), ToolError> {
        {
            let mut state = self.lock_state()?;
            if state.session_gen != run.session_gen {
                return Err(ToolError("Session changed".into()));
            }
            if let Some(record) = state.records.get_mut(&run.dispatch_id) {
                let task = &mut record.tasks[run.index];
                task.state = AgentState::Running;
                task.started_at = now_ms();
                task.task = prompt.to_owned();
                state.revision = state.revision.wrapping_add(1);
            }
        }
        self.publish_foreground(&run.dispatch_id, &run.call_id, run.session_gen, &run.ctx);
        self.changed();
        Ok(())
    }

    fn make_event_sink(
        self: &Arc<Self>,
        run: &TaskRun,
        collector: Arc<Mutex<Collector>>,
        stop: CancellationToken,
    ) -> Arc<dyn Fn(Event) + Send + Sync> {
        let service = self.clone();
        let id = run.dispatch_id.clone();
        let call_id = run.call_id.clone();
        let index = run.index;
        let session_gen = run.session_gen;
        let ctx = run.ctx.clone();
        Arc::new(move |event: Event| {
            let mut buffer = collector.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(error) = buffer.event(&event.event) {
                buffer.error = Some(error.0);
                stop.cancel();
            }
            if buffer.error.is_some() {
                stop.cancel();
            }
            if matches!(event.event, EventKind::ItemDelta { .. })
                && buffer.published.elapsed() < Duration::from_millis(50)
            {
                return;
            }
            buffer.published = Instant::now();
            if let Ok(mut state) = service.lock_state()
                && state.session_gen == session_gen
                && let Some(record) = state.records.get_mut(&id)
            {
                buffer.update(&mut record.tasks[index]);
                state.revision = state.revision.wrapping_add(1);
            }
            drop(buffer);
            service.publish_foreground(&id, &call_id, session_gen, &ctx);
            service.changed();
        })
    }

    fn build_child_registry(
        &self,
        run: &TaskRun,
    ) -> Result<(Registry, String, Option<Vec<String>>), ToolError> {
        match &run.task.custom_agent {
            Some(agent) => Ok((
                self.children.build(&run.ctx, &agent.name)?,
                agent.prompt.clone(),
                agent.tools.clone(),
            )),
            None => Ok((self.children.build_default(&run.ctx)?, String::new(), None)),
        }
    }

    async fn execute_child(
        self: &Arc<Self>,
        run: &TaskRun,
        runner: Arc<dyn ChildRunner>,
        collector: Arc<Mutex<Collector>>,
        steer: Option<mpsc::UnboundedReceiver<String>>,
        child_cancel: CancellationToken,
    ) -> Result<(), ToolError> {
        let cwd = resolve_task_cwd(run)?;
        let store_path = child_session_path(&self.home, &run.ctx.scope.thread_id.0, &run.task.id)?;
        let (registry, system, tools) = self.build_child_registry(run)?;
        let (mode, prompt, model) = resolve_child_prompt(&run.task);
        self.mark_running(run, &run.task.task)?;
        let events = self.make_event_sink(run, collector, child_cancel.clone());
        runner
            .run(
                ChildRequest {
                    cwd,
                    task: prompt,
                    system,
                    model,
                    thinking: run.task.thinking,
                    tools,
                    mode,
                    store_path,
                    steer,
                },
                registry,
                child_cancel,
                events,
            )
            .await
    }

    fn settle_task(&self, run: &TaskRun, buffer: &mut Collector) -> Result<String, ToolError> {
        let full = read_completed_output(buffer);
        let completed = {
            let mut state = self.lock_state()?;
            if state.session_gen != run.session_gen {
                return Err(ToolError("Session changed".into()));
            }
            let record = state
                .records
                .get_mut(&run.dispatch_id)
                .ok_or_else(|| ToolError("Dispatch disappeared".into()))?;
            let task = &mut record.tasks[run.index];
            buffer.update(task);
            task.state = resolve_task_state(buffer, &run.cancel);
            task.diagnostic = buffer.error.clone();
            if let Some((text, thinking)) = &full {
                task.output = text.clone();
                task.thinking = thinking.clone();
            }
            let partial = task.turns == 0 || buffer.partial;
            let usage = Usage {
                input: task.usage.input,
                output: task.usage.output,
                cache_read: task.usage.cache_read,
                cache_write: task.usage.cache_write,
                reasoning: task.usage.reasoning,
                cache_write_1h: task.usage.cache_write_1h,
                cost_nano_usd: task.usage.cost_nano_usd,
                partial,
            };
            let task_id = task.id.clone();
            let ok = task.state == AgentState::Completed;
            state.pending_entries.push((
                "runtime.usage".into(),
                json!({
                    "id": format!("child/{}/{}/{}", run.ctx.scope.turn_id.0, run.dispatch_id, run.index),
                    "usage": usage,
                }),
            ));
            state.queue_task_notification(&run.dispatch_id, run.index);
            state.tasks.remove(&task_id);
            state.revision = state.revision.wrapping_add(1);
            ok
        };
        self.publish_foreground(&run.dispatch_id, &run.call_id, run.session_gen, &run.ctx);
        self.changed();
        if completed {
            return Ok(full.map(|(text, _)| text).unwrap_or_default());
        }
        Err(ToolError(
            buffer
                .error
                .clone()
                .unwrap_or_else(|| "Child did not complete".into()),
        ))
    }

    fn cancel_waiting_task(&self, run: &TaskRun) {
        if let Ok(mut state) = self.lock_state()
            && state.session_gen == run.session_gen
        {
            if let Some(record) = state.records.get_mut(&run.dispatch_id) {
                let task = &mut record.tasks[run.index];
                if task.state.active() && run.cancel.is_cancelled() {
                    task.state = AgentState::Cancelled;
                }
            }
            state.tasks.remove(&run.task.id);
            state.revision = state.revision.wrapping_add(1);
        }
        self.publish_foreground(&run.dispatch_id, &run.call_id, run.session_gen, &run.ctx);
        self.changed();
    }

    async fn run_task(
        self: &Arc<Self>,
        mut run: TaskRun,
        runner: Arc<dyn ChildRunner>,
    ) -> Result<String, ToolError> {
        let _done = DoneSignal(run.done_tx.clone());
        let permit = match self.acquire_slot(&run.cancel).await {
            Ok(permit) => permit,
            Err(err) => {
                self.cancel_waiting_task(&run);
                return Err(err);
            }
        };
        let directory = self.home.join("state/subagents/output");
        let collector = Arc::new(Mutex::new(Collector::new(&directory)));
        let child_cancel = run.cancel.child_token();
        let steer = run.steer_rx.take();
        let fut = self.execute_child(&run, runner, collector.clone(), steer, child_cancel);
        let outcome = match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
            Ok(res) => res,
            Err(_) => Err(ToolError("Child execution panicked; task isolated".into())),
        };
        drop(permit);
        let mut buffer = collector.lock().map_err(|e| ToolError(e.to_string()))?;
        if let Err(error) = outcome {
            if run.cancel.is_cancelled() && buffer.status.is_none() && buffer.error.is_none() {
                buffer.status = Some(TurnStatus::Interrupted);
            } else {
                buffer.error = Some(error.0);
            }
        }
        self.settle_task(&run, &mut buffer)
    }

    pub(crate) async fn quiesce_all(&self) {
        let waiters = match self.lock_state() {
            Ok(mut state) => state.stop_all(StopCause::Session),
            Err(_) => return,
        };
        self.changed();
        wait_done(waiters, Duration::from_secs(5)).await;
    }
}

fn register_task_runs(
    state: &mut crate::state::State,
    (dispatch_id, call_id): (&str, &str),
    mode: &str,
    session_gen: u64,
    tasks: Vec<PreparedTask>,
    ctx: &ToolContext,
    dispatch_cancel: &CancellationToken,
) -> Vec<TaskRun> {
    tasks
        .into_iter()
        .enumerate()
        .map(|(index, task)| {
            let cancel = if mode == "chain" {
                dispatch_cancel.child_token()
            } else {
                CancellationToken::new()
            };
            let (done_tx, done_rx) = watch::channel(false);
            let (steer_tx, steer_rx) = mpsc::unbounded_channel();
            state.tasks.insert(
                task.id.clone(),
                ActiveTask {
                    dispatch_id: dispatch_id.to_owned(),
                    cancel: cancel.clone(),
                    steer: steer_tx,
                    done: done_rx,
                    stop: None,
                    notified: false,
                },
            );
            TaskRun {
                dispatch_id: dispatch_id.to_owned(),
                call_id: call_id.to_owned(),
                session_gen,
                index,
                task,
                ctx: ctx.clone(),
                cancel,
                steer_rx: Some(steer_rx),
                done_tx,
            }
        })
        .collect()
}
