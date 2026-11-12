//! Bounded dispatches of isolated actors, with parent-owned approvals and cancellation.
pub mod definitions;
mod output;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tiechui_plugin_api::{
    futures::{FutureExt, StreamExt},
    model::*,
    protocol::*,
    serde_json::{self, Value, json},
    *,
};

const ENTRY: &str = "tiechui.subagents.record";
const MAX_PARALLEL_TASKS: usize = 8;
const MAX_CONCURRENT_TASKS: usize = 8;

#[async_trait]
pub trait ProjectConfirmation: Send + Sync {
    async fn confirm(
        &self,
        paths: Vec<PathBuf>,
        cancel: CancellationToken,
    ) -> Result<bool, ToolError>;
}

pub struct SubagentsModule {
    pub home: PathBuf,
    pub trusted: bool,
    pub confirmation: Option<Arc<dyn ProjectConfirmation>>,
    pub children: Arc<dyn ChildRegistry>,
}

impl Module for SubagentsModule {
    fn id(&self) -> &str {
        "subagents"
    }
    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        let service = Arc::new(Service {
            home: self.home.clone(),
            trusted: self.trusted,
            confirmation: self.confirmation.clone(),
            children: self.children.clone(),
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TASKS)),
            runner: Mutex::new(None),
            state: Mutex::new(State::default()),
        });
        registry.register_resources(service.clone());
        registry.register_actor_client(service.clone());
        registry.register_context_provider(service.clone());
        registry.register_runtime(service.clone());
        registry.register_command(service.clone());
        registry.register_tool(Arc::new(DispatchTool {
            service,
            spec: spec(),
        }))
    }
}

#[derive(Default)]
struct State {
    session: String,
    records: BTreeMap<String, AgentDispatch>,
    active: BTreeMap<String, CancellationToken>,
}

struct Service {
    home: PathBuf,
    trusted: bool,
    confirmation: Option<Arc<dyn ProjectConfirmation>>,
    children: Arc<dyn ChildRegistry>,
    slots: Arc<tokio::sync::Semaphore>,
    runner: Mutex<Option<Arc<dyn ChildRunner>>>,
    state: Mutex<State>,
}

impl ActorClient for Service {
    fn bind(&self, runner: Arc<dyn ChildRunner>) {
        *self.runner.lock().unwrap_or_else(|e| e.into_inner()) = Some(runner);
    }
}

fn restore_records(state: &mut State, session_id: &str, entries: &[BranchEntry]) {
    for (_, value) in entries
        .iter()
        .filter_map(|e| e.custom.as_ref())
        .filter(|(kind, _)| kind == ENTRY)
    {
        let Ok(mut record) = serde_json::from_value::<AgentDispatch>(value.clone()) else {
            continue;
        };
        record.session_id = session_id.to_owned();
        for task in &mut record.tasks {
            if task.state.active() {
                task.state = AgentState::Cancelled;
                task.diagnostic = Some("Interrupted before record settlement".into());
            }
        }
        state.records.insert(record.id.clone(), record);
    }
}

impl RuntimeExtension for Service {
    fn event(&self, event: &RuntimeEvent, branch: &BranchView) -> Result<RuntimeOutput, ToolError> {
        let mut state = self.state.lock().map_err(|e| ToolError(e.to_string()))?;
        match event {
            RuntimeEvent::Restore { .. } | RuntimeEvent::Fork { .. } => {
                for cancel in state.active.values() {
                    cancel.cancel();
                }
                state.active.clear();
                state.records.clear();
                state.session = branch.session_id.clone();
                let entries = match event {
                    RuntimeEvent::Fork { entries } => entries,
                    _ => &branch.entries,
                };
                restore_records(&mut state, &branch.session_id, entries);
            }
            RuntimeEvent::BeforeNavigate | RuntimeEvent::Shutdown | RuntimeEvent::Interrupt => {
                for cancel in state.active.values() {
                    cancel.cancel();
                }
            }
            _ => {}
        }
        Ok(RuntimeOutput {
            events: vec![(
                SUBAGENTS_EVENT.into(),
                json!(SubagentsView {
                    records: state.records.values().cloned().collect(),
                    open: false
                }),
            )],
            ..Default::default()
        })
    }
}

fn format_candidate(entry: &CatalogModel) -> String {
    if !entry.reasoning {
        return format!("- {} (reasoning: off)", entry.model);
    }
    let levels = entry
        .thinking_levels
        .iter()
        .map(|&level| ChildThinking::Level(level).as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!("- {} (thinking: {levels})", entry.model)
}

fn format_catalog(catalog: &ModelCatalog) -> String {
    let candidates: Vec<_> = catalog.candidates().map(format_candidate).collect();
    let body = if candidates.is_empty() {
        "No other candidate models are configured; subagents run on the active session model."
            .to_owned()
    } else {
        format!(
            "Candidate models for `subagent` (`model` parameter):\n{}",
            candidates.join("\n")
        )
    };
    format!(
        "<subagent_models>\nActive session model: {}\n{body}\n</subagent_models>",
        catalog.active
    )
}

#[async_trait]
impl ContextProvider for Service {
    fn describe(&self, fragments: &[String]) -> Vec<PromptFragment> {
        fragments
            .iter()
            .map(|text| PromptFragment {
                label: "subagent_models".into(),
                category: "System prompt".into(),
                source: "Subagent model catalog".into(),
                text: text.clone(),
                references: vec![],
            })
            .collect()
    }

    async fn fragments(
        &self,
        _: &TurnInput,
        _: CancellationToken,
    ) -> Result<Vec<String>, ToolError> {
        let runner = self
            .runner
            .lock()
            .map_err(|e| ToolError(e.to_string()))?
            .clone();
        let Some(runner) = runner else {
            return Ok(vec![]);
        };
        Ok(vec![format_catalog(&runner.model_catalog())])
    }
}

fn spec() -> ToolSpec {
    let thinking = json!({
        "type": "string",
        "enum": ["off", "minimal", "low", "medium", "high", "xhigh", "max", "inherit"]
    });
    let task = json!({
        "type": "object",
        "required": ["task"],
        "properties": {
            "agent": {"type": "string"},
            "task": {"type": "string"},
            "cwd": {"type": "string"},
            "model": {"type": "string"},
            "thinking": thinking
        }
    });
    ToolSpec {
        name: "subagent".into(),
        description: format!(
            "Delegate isolated tasks to the default general-purpose subagent or a named user/project agent. Use exactly one of task, tasks (at most {MAX_PARALLEL_TASKS}), or chain (supports {{previous}}). Up to {MAX_CONCURRENT_TASKS} child tasks run concurrently across the session; extras queue. Project definitions require user trust or interactive confirmation."
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "agent": {"type": "string"},
                "task": {"type": "string"},
                "cwd": {"type": "string"},
                "model": {"type": "string"},
                "thinking": thinking,
                "tasks": {"type": "array", "items": task},
                "chain": {"type": "array", "items": task},
                "agentScope": {"type": "string", "enum": ["user", "project", "both"], "default": "user"}
            }
        }),
    }
}

#[derive(serde::Deserialize)]
struct RawTask {
    agent: Option<String>,
    task: String,
    cwd: Option<String>,
    model: Option<String>,
    thinking: Option<String>,
}

struct ValidatedTask {
    agent: Option<String>,
    task: String,
    cwd: Option<String>,
    model: Option<String>,
    thinking: Option<ChildThinking>,
}

struct PreparedTask {
    label: String,
    custom_agent: Option<definitions::Definition>,
    task: String,
    cwd: Option<String>,
    model: Option<String>,
    thinking: Option<ThinkingLevel>,
}

fn parse_mode(value: &Value) -> Result<(&'static str, Vec<RawTask>), ToolError> {
    let has = |key: &str| value.get(key).is_some_and(|v| !v.is_null());
    let single = ["task", "agent", "cwd", "model", "thinking"]
        .into_iter()
        .any(has);
    let modes = usize::from(single) + usize::from(has("tasks")) + usize::from(has("chain"));
    if modes != 1 {
        return Err(ToolError("Use exactly one of task, tasks, or chain".into()));
    }
    if let Some(tasks) = value.get("tasks").filter(|v| !v.is_null()) {
        let parsed = serde_json::from_value(tasks.clone()).map_err(|e| ToolError(e.to_string()))?;
        return Ok(("parallel", parsed));
    }
    if let Some(tasks) = value.get("chain").filter(|v| !v.is_null()) {
        let parsed = serde_json::from_value(tasks.clone()).map_err(|e| ToolError(e.to_string()))?;
        return Ok(("chain", parsed));
    }
    let parsed: RawTask =
        serde_json::from_value(value.clone()).map_err(|e| ToolError(e.to_string()))?;
    Ok(("single", vec![parsed]))
}

fn validate_raw_task(raw: RawTask) -> Result<ValidatedTask, ToolError> {
    if raw.task.trim().is_empty() {
        return Err(ToolError("Tasks must not be empty".into()));
    }
    if raw
        .agent
        .as_deref()
        .is_some_and(|agent| agent.trim().is_empty())
    {
        return Err(ToolError(
            "Agent name must not be blank; omit `agent` to use the default subagent".into(),
        ));
    }
    let thinking = raw
        .thinking
        .as_deref()
        .map(parse_child_thinking)
        .transpose()?;
    Ok(ValidatedTask {
        agent: raw.agent,
        task: raw.task,
        cwd: raw.cwd,
        model: raw.model,
        thinking,
    })
}

fn parameters(value: &Value) -> Result<(&'static str, Vec<ValidatedTask>), ToolError> {
    let (mode, raw_tasks) = parse_mode(value)?;
    if raw_tasks.is_empty() {
        return Err(ToolError("Tasks must not be empty".into()));
    }
    if mode == "parallel" && raw_tasks.len() > MAX_PARALLEL_TASKS {
        return Err(ToolError(format!(
            "At most {MAX_PARALLEL_TASKS} parallel tasks are allowed"
        )));
    }
    let tasks = raw_tasks
        .into_iter()
        .map(validate_raw_task)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((mode, tasks))
}

fn unknown_agent_error(
    name: &str,
    agents: &BTreeMap<String, definitions::Definition>,
) -> ToolError {
    if agents.is_empty() {
        return ToolError(format!(
            "Unknown agent: {name}. No custom agents are defined in scope; omit `agent` to use the default subagent."
        ));
    }
    let available = agents.keys().cloned().collect::<Vec<_>>().join(", ");
    ToolError(format!(
        "Unknown agent: {name}. Available custom agents: {available} (or omit `agent` to use the default subagent)."
    ))
}

fn prepare_task(
    task: ValidatedTask,
    agents: &BTreeMap<String, definitions::Definition>,
    runner: &dyn ChildRunner,
) -> Result<PreparedTask, ToolError> {
    let (label, custom_agent) = match task.agent {
        None => ("default".to_owned(), None),
        Some(name) => {
            let def = agents
                .get(&name)
                .cloned()
                .ok_or_else(|| unknown_agent_error(&name, agents))?;
            (def.name.clone(), Some(def))
        }
    };
    let agent_model = custom_agent.as_ref().and_then(|def| def.model.as_deref());
    let agent_thinking = custom_agent.as_ref().and_then(|def| def.thinking);
    let model = match task.model.as_deref().or(agent_model) {
        None | Some("inherit") => None,
        Some(value) => Some(value.to_owned()),
    };
    let thinking = match task.thinking.or(agent_thinking) {
        None | Some(ChildThinking::Inherit) => None,
        Some(ChildThinking::Level(level)) => Some(level),
    };
    if let Some(m) = model.as_deref() {
        runner.validate_model(m)?;
    }
    Ok(PreparedTask {
        label,
        custom_agent,
        task: task.task,
        cwd: task.cwd,
        model,
        thinking,
    })
}

struct DispatchTool {
    service: Arc<Service>,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for DispatchTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Shared
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.service.dispatch(call, ctx).await
    }
}

struct TaskSlot<'a> {
    id: &'a str,
    index: usize,
    ctx: &'a ToolContext,
    collector: Arc<Mutex<Collector>>,
    child_cancel: CancellationToken,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

fn read_completed_output(buffer: &mut Collector) -> Option<(String, String)> {
    if buffer.error.is_some() || buffer.status != Some(TurnStatus::Completed) {
        return None;
    }
    match buffer
        .output
        .full()
        .and_then(|text| Ok((text, buffer.thinking.full()?)))
    {
        Ok(full) => Some(full),
        Err(error) => {
            buffer.error = Some(error.0);
            None
        }
    }
}

fn blank_agent_task(task: &PreparedTask) -> AgentTask {
    AgentTask {
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
        model: String::new(),
    }
}

fn resolve_child_cwd(parent_cwd: &Path, child: Option<&str>) -> Result<PathBuf, ToolError> {
    let cwd = std::fs::canonicalize(parent_cwd.join(child.unwrap_or(".")))?;
    let boundary = std::fs::canonicalize(parent_cwd)?;
    if !cwd.starts_with(&boundary) || !cwd.is_dir() {
        return Err(ToolError(
            "Child cwd must remain inside the parent workspace".into(),
        ));
    }
    Ok(cwd)
}

impl Service {
    fn publish(&self, id: &str, ctx: &ToolContext) -> Result<(), ToolError> {
        let state = self.state.lock().map_err(|e| ToolError(e.to_string()))?;
        if state.session != ctx.scope.thread_id.0 {
            return Err(ToolError("Child dispatch belongs to an old session".into()));
        }
        if let Some(record) = state.records.get(id) {
            (ctx.updates)(tools::ToolUpdate {
                call_id: id.into(),
                epoch: ctx.epoch,
                output: result(record),
            });
        }
        Ok(())
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

    fn record_start(
        &self,
        id: &str,
        mode: &str,
        tasks: &[PreparedTask],
        ctx: &ToolContext,
        cancel: &CancellationToken,
    ) -> Result<(), ToolError> {
        let record = AgentDispatch {
            id: id.to_owned(),
            session_id: ctx.scope.thread_id.0.clone(),
            mode: mode.into(),
            tasks: tasks.iter().map(blank_agent_task).collect(),
        };
        {
            let mut state = self.state.lock().map_err(|e| ToolError(e.to_string()))?;
            if state.session != ctx.scope.thread_id.0 {
                return Err(ToolError("Session changed before dispatch".into()));
            }
            state.records.insert(id.to_owned(), record.clone());
            state.active.insert(id.to_owned(), cancel.clone());
        }
        (ctx.append_entry)(tools::CustomSessionEntry {
            kind: ENTRY.into(),
            data: json!(record),
        })?;
        self.publish(id, ctx)
    }

    fn record_finish(
        &self,
        id: &str,
        ctx: &ToolContext,
        cancel: &CancellationToken,
    ) -> Result<AgentDispatch, ToolError> {
        let record = {
            let mut state = self.state.lock().map_err(|e| ToolError(e.to_string()))?;
            if state.session != ctx.scope.thread_id.0 {
                return Err(ToolError("Session changed during dispatch".into()));
            }
            state.active.remove(id);
            let record = state
                .records
                .get_mut(id)
                .ok_or_else(|| ToolError("Dispatch disappeared".into()))?;
            for task in &mut record.tasks {
                if task.state.active() {
                    task.state = if cancel.is_cancelled() {
                        AgentState::Cancelled
                    } else {
                        AgentState::Skipped
                    };
                }
            }
            record.clone()
        };
        (ctx.append_entry)(tools::CustomSessionEntry {
            kind: ENTRY.into(),
            data: json!(record),
        })?;
        self.publish(id, ctx)?;
        Ok(record)
    }

    fn bound_runner(&self) -> Result<Arc<dyn ChildRunner>, ToolError> {
        self.runner
            .lock()
            .map_err(|e| ToolError(e.to_string()))?
            .clone()
            .ok_or_else(|| ToolError("Child actors are unavailable".into()))
    }

    async fn run_batch(
        self: &Arc<Self>,
        id: &str,
        mode: &str,
        tasks: Vec<PreparedTask>,
        runner: Arc<dyn ChildRunner>,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) {
        if mode == "parallel" {
            let mut jobs =
                futures::stream::iter(tasks.into_iter().enumerate().map(|(index, task)| {
                    self.run_task(id, index, task, runner.clone(), ctx, cancel.clone())
                }))
                .buffer_unordered(MAX_PARALLEL_TASKS);
            while jobs.next().await.is_some() {}
            return;
        }
        let mut previous = String::new();
        for (index, mut task) in tasks.into_iter().enumerate() {
            task.task = task.task.replace("{previous}", &previous);
            match self
                .run_task(id, index, task, runner.clone(), ctx, cancel.clone())
                .await
            {
                Ok(output) => previous = output,
                Err(_) => break,
            }
        }
    }

    async fn dispatch(
        self: &Arc<Self>,
        call: ToolCall,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let (mode, raw_tasks) = parameters(&call.arguments)?;
        let scope = call.arguments["agentScope"].as_str().unwrap_or("user");
        let agents = definitions::discover(&self.home, &ctx.cwd, scope, &ctx.cancel)?;
        let runner = self.bound_runner()?;
        let tasks = raw_tasks
            .into_iter()
            .map(|task| prepare_task(task, &agents, runner.as_ref()))
            .collect::<Result<Vec<_>, _>>()?;
        self.confirm_project_agents(&tasks, &ctx.cancel).await?;
        let cancel = ctx.cancel.child_token();
        self.record_start(&call.id, mode, &tasks, &ctx, &cancel)?;
        self.run_batch(&call.id, mode, tasks, runner, &ctx, cancel.clone())
            .await;
        let record = self.record_finish(&call.id, &ctx, &cancel)?;
        Ok(result(&record))
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

    fn mark_running(
        &self,
        id: &str,
        index: usize,
        prompt: &str,
        ctx: &ToolContext,
    ) -> Result<(), ToolError> {
        {
            let mut state = self.state.lock().map_err(|e| ToolError(e.to_string()))?;
            if state.session != ctx.scope.thread_id.0 {
                return Err(ToolError("Session changed".into()));
            }
            if let Some(record) = state.records.get_mut(id) {
                record.tasks[index].state = AgentState::Running;
                record.tasks[index].task = prompt.to_owned();
            }
        }
        self.publish(id, ctx)
    }

    fn make_event_sink(
        self: &Arc<Self>,
        id: &str,
        index: usize,
        ctx: &ToolContext,
        collector: Arc<Mutex<Collector>>,
        stop: CancellationToken,
    ) -> Arc<dyn Fn(Event) + Send + Sync> {
        let service = self.clone();
        let id = id.to_owned();
        let ctx = ctx.clone();
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
                && buffer.published.elapsed() < std::time::Duration::from_millis(50)
            {
                return;
            }
            buffer.published = std::time::Instant::now();
            if let Ok(mut state) = service.state.lock()
                && state.session == ctx.scope.thread_id.0
                && let Some(record) = state.records.get_mut(&id)
            {
                buffer.update(&mut record.tasks[index]);
            }
            drop(buffer);
            let _ = service.publish(&id, &ctx);
        })
    }

    async fn execute_child(
        self: &Arc<Self>,
        task: PreparedTask,
        runner: Arc<dyn ChildRunner>,
        slot: TaskSlot<'_>,
    ) -> Result<(), ToolError> {
        let cwd = resolve_child_cwd(&slot.ctx.cwd, task.cwd.as_deref())?;
        let (registry, system, tools) = match &task.custom_agent {
            Some(agent) => (
                self.children.build(slot.ctx, &agent.name)?,
                agent.prompt.clone(),
                agent.tools.clone(),
            ),
            None => (self.children.build_default(slot.ctx)?, String::new(), None),
        };
        self.mark_running(slot.id, slot.index, &task.task, slot.ctx)?;
        let events = self.make_event_sink(
            slot.id,
            slot.index,
            slot.ctx,
            slot.collector,
            slot.child_cancel.clone(),
        );
        runner
            .run(
                ChildRequest {
                    cwd,
                    task: task.task,
                    system,
                    model: task.model,
                    thinking: task.thinking,
                    tools,
                },
                registry,
                slot.child_cancel,
                events,
            )
            .await
    }

    fn settle_task(
        &self,
        id: &str,
        index: usize,
        ctx: &ToolContext,
        cancel: &CancellationToken,
        buffer: &mut Collector,
    ) -> Result<String, ToolError> {
        let full = read_completed_output(buffer);
        let usage = {
            let mut state = self.state.lock().map_err(|e| ToolError(e.to_string()))?;
            if state.session != ctx.scope.thread_id.0 {
                return Err(ToolError("Session changed".into()));
            }
            let record = state
                .records
                .get_mut(id)
                .ok_or_else(|| ToolError("Dispatch disappeared".into()))?;
            let result = &mut record.tasks[index];
            buffer.update(result);
            result.state = resolve_task_state(buffer, cancel);
            result.diagnostic = buffer.error.clone();
            if let Some((text, thinking)) = &full {
                result.output = text.clone();
                result.thinking = thinking.clone();
            }
            let partial = result.turns == 0 || buffer.partial;
            (
                result.state == AgentState::Completed,
                Usage {
                    input: result.usage.input,
                    output: result.usage.output,
                    cache_read: result.usage.cache_read,
                    cache_write: result.usage.cache_write,
                    partial,
                    ..Default::default()
                },
            )
        };
        // Per-task totals are reported once, with an identity stable within the dispatch.
        (ctx.append_entry)(tools::CustomSessionEntry {
            kind: "runtime.usage".into(),
            data: json!({"id":format!("child/{}/{id}/{index}", ctx.scope.turn_id.0),"usage":usage.1}),
        })?;
        self.publish(id, ctx)?;
        if usage.0 {
            return Ok(full.map(|(text, _)| text).unwrap_or_default());
        }
        Err(ToolError(
            buffer
                .error
                .clone()
                .unwrap_or_else(|| "Child did not complete".into()),
        ))
    }

    async fn run_task(
        self: &Arc<Self>,
        id: &str,
        index: usize,
        task: PreparedTask,
        runner: Arc<dyn ChildRunner>,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        let permit = self.acquire_slot(&cancel).await?;
        let directory = self.home.join("state/subagents/output");
        let collector = Arc::new(Mutex::new(Collector::new(&directory)));
        let slot = TaskSlot {
            id,
            index,
            ctx,
            collector: collector.clone(),
            child_cancel: cancel.child_token(),
            _permit: permit,
        };
        let fut = self.execute_child(task, runner, slot);
        let outcome = match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
            Ok(result) => result,
            Err(_) => Err(ToolError("Child execution panicked; task isolated".into())),
        };
        let mut buffer = collector.lock().map_err(|e| ToolError(e.to_string()))?;
        if let Err(error) = outcome {
            if cancel.is_cancelled() && buffer.status.is_none() && buffer.error.is_none() {
                buffer.status = Some(TurnStatus::Interrupted);
            } else {
                buffer.error = Some(error.0);
            }
        }
        self.settle_task(id, index, ctx, &cancel, &mut buffer)
    }
}

fn resolve_task_state(buffer: &Collector, cancel: &CancellationToken) -> AgentState {
    if buffer.error.is_some() {
        return AgentState::Failed;
    }
    if cancel.is_cancelled() && buffer.status != Some(TurnStatus::Completed) {
        return AgentState::Cancelled;
    }
    match buffer.status {
        Some(TurnStatus::Completed) => AgentState::Completed,
        Some(TurnStatus::Interrupted) => AgentState::Cancelled,
        _ => AgentState::Failed,
    }
}

struct Collector {
    published: std::time::Instant,
    partial: bool,
    output: output::Output,
    thinking: output::Output,
    kinds: BTreeMap<String, bool>,
    blocks: Vec<(String, String)>,
    thinking_blocks: Vec<(String, String)>,
    status: Option<TurnStatus>,
    error: Option<String>,
    usage: TokenUsage,
    turns: u64,
    model: String,
    next_response: bool,
}

impl Collector {
    fn new(directory: &Path) -> Self {
        Self {
            published: std::time::Instant::now(),
            partial: false,
            output: output::Output::new(directory),
            thinking: output::Output::new(directory),
            kinds: BTreeMap::new(),
            blocks: vec![],
            thinking_blocks: vec![],
            status: None,
            error: None,
            usage: TokenUsage::default(),
            turns: 0,
            model: String::new(),
            next_response: false,
        }
    }

    fn start_assistant(&mut self, id: &str, text: &str) -> Result<(), ToolError> {
        if self.next_response {
            self.output.clear();
            self.blocks.clear();
            self.kinds.retain(|_, thinking| *thinking);
            self.next_response = false;
        } else if !self.output.text.is_empty() {
            self.output.push("\n")?;
        }
        self.kinds.insert(id.to_owned(), false);
        self.blocks.push((id.to_owned(), text.to_owned()));
        self.output.push(text)
    }

    fn push_delta(&mut self, item_id: &str, delta: &str) -> Result<(), ToolError> {
        let Some(&thinking) = self.kinds.get(item_id) else {
            return Ok(());
        };
        let (blocks, output) = if thinking {
            (&mut self.thinking_blocks, &mut self.thinking)
        } else {
            (&mut self.blocks, &mut self.output)
        };
        if let Some((_, text)) = blocks.iter_mut().find(|(id, _)| id == item_id) {
            text.push_str(delta);
        }
        output.push(delta)
    }

    fn complete_text(&mut self, id: &str, text: &str) -> Result<(), ToolError> {
        let thinking = self.kinds.get(id).copied().unwrap_or(false);
        let (blocks, output) = if thinking {
            (&mut self.thinking_blocks, &mut self.thinking)
        } else {
            (&mut self.blocks, &mut self.output)
        };
        let Some((_, previous)) = blocks.iter_mut().find(|(key, _)| key == id) else {
            return Ok(());
        };
        if previous == text {
            return Ok(());
        }
        *previous = text.to_owned();
        output.clear();
        for (i, (_, block_text)) in blocks.iter().enumerate() {
            if i > 0 {
                output.push("\n")?;
            }
            output.push(block_text)?;
        }
        Ok(())
    }

    fn complete_item(&mut self, item: &Item) -> Result<bool, ToolError> {
        match item {
            Item::Known(
                KnownItem::AssistantMessage { id, text } | KnownItem::Thinking { id, text },
            ) => {
                self.complete_text(&id.0, text)?;
            }
            Item::Known(KnownItem::ToolResult {
                output,
                is_error: true,
                ..
            }) if output["details"]["panic"] == true => {
                self.error = Some("Child tool panicked".into());
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn item_event(&mut self, event: &EventKind) -> Result<bool, ToolError> {
        match event {
            EventKind::ItemStarted {
                item: Item::Known(KnownItem::AssistantMessage { id, text }),
                ..
            } => self.start_assistant(&id.0, text)?,
            EventKind::ItemStarted {
                item: Item::Known(KnownItem::Thinking { id, text }),
                ..
            } => {
                self.kinds.insert(id.0.clone(), true);
                self.thinking_blocks.push((id.0.clone(), text.clone()));
                self.thinking.push(text)?;
            }
            EventKind::ItemDelta { item_id, delta, .. } => self.push_delta(&item_id.0, delta)?,
            EventKind::ItemCompleted { item, .. } => return self.complete_item(item),
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn check_limits(&self) -> Result<(), ToolError> {
        let total_bytes: usize = self
            .blocks
            .iter()
            .chain(&self.thinking_blocks)
            .map(|(_, text)| text.len())
            .sum();
        if self.blocks.len() + self.thinking_blocks.len() > 256 || total_bytes > 8 * 1024 * 1024 {
            return Err(ToolError(
                "Child live messages exceeded the 8 MiB / 256 block memory limit".into(),
            ));
        }
        Ok(())
    }

    fn event(&mut self, event: &EventKind) -> Result<(), ToolError> {
        if !self.item_event(event)? {
            match event {
                EventKind::Extension { name, data } if name == ACTOR_USAGE_EVENT => {
                    self.partial |= serde_json::from_value::<Option<Usage>>(data.clone())
                        .ok()
                        .flatten()
                        .is_none_or(|usage| usage.partial);
                }
                EventKind::TokenUsageUpdated { total, .. } => {
                    self.usage = *total;
                    self.turns += 1;
                    self.next_response = true;
                }
                EventKind::SessionUpdated { model, .. } => {
                    self.model = format!("{}/{}", model.provider, model.id);
                }
                EventKind::TurnCompleted { turn } => self.status = Some(turn.status),
                EventKind::Error { message } => self.error = Some(message.clone()),
                _ => {}
            }
        }
        self.check_limits()
    }

    fn update(&self, task: &mut AgentTask) {
        task.output = self.output.text.clone();
        task.thinking = self.thinking.text.clone();
        task.output_path = self.output.path.as_ref().map(|p| p.display().to_string());
        task.thinking_path = self.thinking.path.as_ref().map(|p| p.display().to_string());
        task.usage = self.usage;
        task.turns = self.turns;
        task.model = self.model.clone();
    }
}

fn task_label(task: &AgentTask) -> String {
    if task.model.is_empty() {
        task.agent.clone()
    } else {
        format!("{} · {}", task.agent, task.model)
    }
}

fn format_task_summary(task: &AgentTask) -> String {
    let mut text = format!(
        "### [{}] {:?}\n\n{}",
        task_label(task),
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

fn result(record: &AgentDispatch) -> ToolOutput {
    let text = if record.mode != "parallel"
        && record
            .tasks
            .iter()
            .all(|t| t.state == AgentState::Completed)
    {
        record
            .tasks
            .last()
            .map(|task| task.output.clone())
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| "(no output)".into())
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
        is_error: record
            .tasks
            .iter()
            .any(|t| matches!(t.state, AgentState::Failed | AgentState::Cancelled)),
        ..ToolOutput::text(text)
    }
}

#[async_trait]
impl Command for Service {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "subagents".into(),
            description: "View the main session and child tasks; Ctrl+X stops a dispatch".into(),
            argument_hint: None,
        }
    }
    fn background(&self) -> bool {
        true
    }
    fn allows_busy(&self, _: &str) -> bool {
        true
    }
    async fn run(&self, _: &str, _: CancellationToken) -> Result<String, ToolError> {
        Err(ToolError(
            "Subagent viewer requires an interactive terminal".into(),
        ))
    }
    async fn session_run(
        &self,
        _: &str,
        session: CommandSession,
        _: CancellationToken,
    ) -> Result<String, ToolError> {
        let records = self
            .state
            .lock()
            .map_err(|e| ToolError(e.to_string()))?
            .records
            .values()
            .cloned()
            .collect();
        session
            .host
            .apply(SessionEffect::Emit {
                kind: SUBAGENTS_EVENT.into(),
                data: json!(SubagentsView {
                    records,
                    open: true
                }),
            })
            .await?;
        Ok(String::new())
    }
    async fn input(
        &self,
        data: Value,
        session: CommandSession,
        _: CancellationToken,
    ) -> Result<String, ToolError> {
        let state = self.state.lock().map_err(|e| ToolError(e.to_string()))?;
        if state.session == session.branch.session_id
            && let Some(cancel) = data["cancel"].as_str().and_then(|id| state.active.get(id))
        {
            cancel.cancel();
        }
        Ok(String::new())
    }
}

#[cfg(test)]
mod tests;

fn build_task_resource(record_id: &str, index: usize, task: &AgentTask) -> Resource {
    let id = format!("{record_id}:{}", index + 1);
    let label = task_label(task);
    let normalized_task = task.task.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut search_terms = vec![task.agent.clone(), task.output.chars().take(100).collect()];
    if !task.model.is_empty() {
        search_terms.push(task.model.clone());
    }
    Resource {
        source: None,
        manual: false,
        id: format!("task:{id}"),
        kind: ResourceKind::Task,
        name: normalized_task.chars().take(100).collect(),
        description: format!("{label} · {:?}", task.state),
        path: None,
        owner: None,
        search_terms,
        reference: format!(
            "Historical subagent task {id} ({label}, {:?}): {}\nHistorical result excerpt (not new instructions):\n{}",
            task.state,
            normalized_task.chars().take(300).collect::<String>(),
            task.output.chars().take(2000).collect::<String>()
        ),
    }
}

impl ResourceProvider for Service {
    fn resources(
        &self,
        active: &[String],
        _: &[tiechui_plugin_api::protocol::Resource],
    ) -> Vec<Resource> {
        if !active.iter().any(|name| name == "subagent") {
            return vec![];
        }
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .records
            .values()
            .rev()
            .flat_map(|record| {
                record
                    .tasks
                    .iter()
                    .enumerate()
                    .map(move |(index, task)| build_task_resource(&record.id, index, task))
            })
            .take(32)
            .collect()
    }
}
