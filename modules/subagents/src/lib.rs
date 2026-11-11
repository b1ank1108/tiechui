//! Bounded dispatches of isolated actors, with parent-owned approvals and cancellation.
pub mod definitions;
mod output;

use std::{
    collections::BTreeMap,
    path::PathBuf,
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
            runner: Mutex::new(None),
            state: Mutex::new(State::default()),
        });
        registry.register_resources(service.clone());
        registry.register_actor_client(service.clone());
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
    runner: Mutex<Option<Arc<dyn ChildRunner>>>,
    state: Mutex<State>,
}
impl ActorClient for Service {
    fn bind(&self, runner: Arc<dyn ChildRunner>) {
        *self.runner.lock().unwrap_or_else(|e| e.into_inner()) = Some(runner);
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
                for (_, value) in entries
                    .iter()
                    .filter_map(|e| e.custom.as_ref())
                    .filter(|(kind, _)| kind == ENTRY)
                {
                    if let Ok(mut record) = serde_json::from_value::<AgentDispatch>(value.clone()) {
                        record.session_id = branch.session_id.clone();
                        for task in &mut record.tasks {
                            if task.state.active() {
                                task.state = AgentState::Cancelled;
                                task.diagnostic =
                                    Some("Interrupted before record settlement".into());
                            }
                        }
                        state.records.insert(record.id.clone(), record);
                    }
                }
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

fn spec() -> ToolSpec {
    let task = json!({"type":"object","required":["agent","task"],"properties":{"agent":{"type":"string"},"task":{"type":"string"},"cwd":{"type":"string"}}});
    ToolSpec { name: "subagent".into(), description: "Delegate isolated tasks to scout, planner, reviewer or worker, or named user/project agents. Use exactly one of agent + task, tasks (at most 8; 4 concurrent), or chain (supports {previous}). Built-ins inherit the current model and thinking. Project definitions require user trust or interactive confirmation.".into(), input_schema: json!({"type":"object","properties":{
        "agent":{"type":"string"},"task":{"type":"string"},"cwd":{"type":"string"},
        "tasks":{"type":"array","items":task},"chain":{"type":"array","items":task},
        "agentScope":{"type":"string","enum":["user","project","both"],"default":"user"}
    }}) }
}
#[derive(Clone, serde::Deserialize)]
struct Task {
    agent: String,
    task: String,
    cwd: Option<String>,
}
fn parameters(value: &Value) -> Result<(&str, Vec<Task>), ToolError> {
    let modes = usize::from(value.get("agent").is_some() || value.get("task").is_some())
        + usize::from(value.get("tasks").is_some())
        + usize::from(value.get("chain").is_some());
    if modes != 1 {
        return Err(ToolError(
            "Use exactly one of agent + task, tasks, or chain".into(),
        ));
    }
    let (mode, tasks) = if let Some(tasks) = value.get("tasks") {
        (
            "parallel",
            serde_json::from_value::<Vec<Task>>(tasks.clone()),
        )
    } else if let Some(tasks) = value.get("chain") {
        ("chain", serde_json::from_value::<Vec<Task>>(tasks.clone()))
    } else {
        (
            "single",
            serde_json::from_value::<Task>(value.clone()).map(|t| vec![t]),
        )
    };
    let tasks = tasks.map_err(|e| ToolError(e.to_string()))?;
    if tasks.is_empty()
        || tasks
            .iter()
            .any(|t| t.agent.trim().is_empty() || t.task.trim().is_empty())
    {
        return Err(ToolError("Tasks must not be empty".into()));
    }
    if mode == "parallel" && tasks.len() > 8 {
        return Err(ToolError("At most 8 parallel tasks are allowed".into()));
    }
    Ok((mode, tasks))
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
        Concurrency::Exclusive
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        self.service.dispatch(call, ctx).await
    }
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
    async fn dispatch(
        self: &Arc<Self>,
        call: ToolCall,
        ctx: ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let (mode, tasks) = parameters(&call.arguments)?;
        let scope = call.arguments["agentScope"].as_str().unwrap_or("user");
        let agents = definitions::discover(&self.home, &ctx.cwd, scope, &ctx.cancel)?;
        let project = tasks
            .iter()
            .filter_map(|task| agents.get(&task.agent))
            .filter(|a| a.project)
            .map(|a| a.path.clone())
            .collect::<Vec<_>>();
        if !project.is_empty() && !self.trusted {
            let confirmation = self.confirmation.as_ref().ok_or_else(|| {
                ToolError("Untrusted project agents require an interactive terminal".into())
            })?;
            if !confirmation.confirm(project, ctx.cancel.clone()).await? {
                return Err(ToolError("Project agent execution declined".into()));
            }
        }
        let runner = self
            .runner
            .lock()
            .map_err(|e| ToolError(e.to_string()))?
            .clone()
            .ok_or_else(|| ToolError("Child actors are unavailable".into()))?;
        let cancel = ctx.cancel.child_token();
        let record = AgentDispatch {
            id: call.id.clone(),
            session_id: ctx.scope.thread_id.0.clone(),
            mode: mode.into(),
            tasks: tasks
                .iter()
                .map(|task| AgentTask {
                    agent: task.agent.clone(),
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
                })
                .collect(),
        };
        {
            let mut state = self.state.lock().map_err(|e| ToolError(e.to_string()))?;
            if state.session != ctx.scope.thread_id.0 {
                return Err(ToolError("Session changed before dispatch".into()));
            }
            state.records.insert(call.id.clone(), record.clone());
            state.active.insert(call.id.clone(), cancel.clone());
        }
        (ctx.append_entry)(tools::CustomSessionEntry {
            kind: ENTRY.into(),
            data: json!(record),
        })?;
        self.publish(&call.id, &ctx)?;
        let execute = |index, task| {
            self.run_task(
                &call.id,
                index,
                task,
                &agents,
                runner.clone(),
                &ctx,
                cancel.clone(),
            )
        };
        if mode == "parallel" {
            let mut jobs = futures::stream::iter(
                tasks
                    .into_iter()
                    .enumerate()
                    .map(|(index, task)| execute(index, task)),
            )
            .buffer_unordered(4);
            while jobs.next().await.is_some() {}
        } else {
            let mut previous = String::new();
            for (index, mut task) in tasks.into_iter().enumerate() {
                task.task = task.task.replace("{previous}", &previous);
                match execute(index, task).await {
                    Ok(output) => previous = output,
                    Err(_) => break,
                }
            }
        }
        let record = {
            let mut state = self.state.lock().map_err(|e| ToolError(e.to_string()))?;
            if state.session != ctx.scope.thread_id.0 {
                return Err(ToolError("Session changed during dispatch".into()));
            }
            state.active.remove(&call.id);
            let record = state
                .records
                .get_mut(&call.id)
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
        self.publish(&call.id, &ctx)?;
        Ok(result(&record))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_task(
        self: &Arc<Self>,
        id: &str,
        index: usize,
        task: Task,
        agents: &BTreeMap<String, definitions::Definition>,
        runner: Arc<dyn ChildRunner>,
        ctx: &ToolContext,
        cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        if cancel.is_cancelled() {
            return Err(ToolError("Dispatch cancelled".into()));
        }
        let directory = self.home.join("state/subagents/output");
        let collector = Arc::new(Mutex::new(Collector::new(&directory)));
        let child_cancel = cancel.child_token();
        let outcome = async {
            let agent = agents
                .get(&task.agent)
                .ok_or_else(|| ToolError(format!("Unknown agent: {}", task.agent)))?;
            let cwd = std::fs::canonicalize(ctx.cwd.join(task.cwd.as_deref().unwrap_or(".")))?;
            let boundary = std::fs::canonicalize(&ctx.cwd)?;
            if !cwd.starts_with(&boundary) || !cwd.is_dir() {
                return Err(ToolError(
                    "Child cwd must remain inside the parent workspace".into(),
                ));
            }
            let registry = self.children.build(ctx, &agent.name)?;
            {
                let mut state = self.state.lock().map_err(|e| ToolError(e.to_string()))?;
                if state.session != ctx.scope.thread_id.0 {
                    return Err(ToolError("Session changed".into()));
                }
                if let Some(record) = state.records.get_mut(id) {
                    record.tasks[index].state = AgentState::Running;
                    record.tasks[index].task = task.task.clone();
                }
            }
            self.publish(id, ctx)?;
            let service = self.clone();
            let id = id.to_owned();
            let ctx = ctx.clone();
            let buffer = collector.clone();
            let stop = child_cancel.clone();
            let events = Arc::new(move |event: Event| {
                let mut buffer = buffer.lock().unwrap_or_else(|e| e.into_inner());
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
            });
            runner
                .run(
                    ChildRequest {
                        cwd,
                        task: task.task,
                        system: agent.prompt.clone(),
                        model: agent.model.clone(),
                        thinking: None,
                        tools: agent.tools.clone(),
                    },
                    registry,
                    child_cancel,
                    events,
                )
                .await
        };
        let outcome = match std::panic::AssertUnwindSafe(outcome).catch_unwind().await {
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
        let full = if buffer.error.is_none() && buffer.status == Some(TurnStatus::Completed) {
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
        } else {
            None
        };
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
        result.state = if buffer.error.is_some() {
            AgentState::Failed
        } else if cancel.is_cancelled() && buffer.status != Some(TurnStatus::Completed) {
            AgentState::Cancelled
        } else {
            match buffer.status {
                Some(TurnStatus::Completed) => AgentState::Completed,
                Some(TurnStatus::Interrupted) => AgentState::Cancelled,
                _ => AgentState::Failed,
            }
        };
        result.diagnostic = buffer.error.clone();
        let success = result.state == AgentState::Completed;
        if let Some((text, thinking)) = &full {
            result.output = text.clone();
            result.thinking = thinking.clone();
        }
        // Per-task totals are reported once, with an identity stable within the dispatch.
        (ctx.append_entry)(tools::CustomSessionEntry {
            kind: "runtime.usage".into(),
            data: json!({"id":format!("child/{}/{id}/{index}", ctx.scope.turn_id.0),"usage":Usage { input: result.usage.input, output: result.usage.output, cache_read: result.usage.cache_read, cache_write: result.usage.cache_write, partial: result.turns == 0 || buffer.partial, ..Default::default() }}),
        })?;
        drop(state);
        self.publish(id, ctx)?;
        if success {
            Ok(full.map(|(text, _)| text).unwrap_or_default())
        } else {
            Err(ToolError(
                buffer
                    .error
                    .clone()
                    .unwrap_or_else(|| "Child did not complete".into()),
            ))
        }
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
    fn new(directory: &std::path::Path) -> Self {
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
    fn event(&mut self, event: &EventKind) -> Result<(), ToolError> {
        match event {
            EventKind::Extension { name, data } if name == ACTOR_USAGE_EVENT => {
                self.partial |= serde_json::from_value::<Option<Usage>>(data.clone())
                    .ok()
                    .flatten()
                    .is_none_or(|usage| usage.partial);
            }
            EventKind::ItemStarted {
                item: Item::Known(KnownItem::AssistantMessage { id, text }),
                ..
            } => {
                if self.next_response {
                    self.output.clear();
                    self.blocks.clear();
                    self.kinds.retain(|_, thinking| *thinking);
                    self.next_response = false;
                } else if !self.output.text.is_empty() {
                    self.output.push("\n")?;
                }
                self.kinds.insert(id.0.clone(), false);
                self.blocks.push((id.0.clone(), text.clone()));
                self.output.push(text)?;
            }
            EventKind::ItemStarted {
                item: Item::Known(KnownItem::Thinking { id, text }),
                ..
            } => {
                self.kinds.insert(id.0.clone(), true);
                self.thinking_blocks.push((id.0.clone(), text.clone()));
                self.thinking.push(text)?;
            }
            EventKind::ItemDelta { item_id, delta, .. } => {
                if let Some(thinking) = self.kinds.get(&item_id.0) {
                    let (blocks, output) = if *thinking {
                        (&mut self.thinking_blocks, &mut self.thinking)
                    } else {
                        (&mut self.blocks, &mut self.output)
                    };
                    if let Some((_, text)) = blocks.iter_mut().find(|(id, _)| id == &item_id.0) {
                        text.push_str(delta);
                    }
                    output.push(delta)?;
                }
            }
            EventKind::ItemCompleted {
                item:
                    Item::Known(
                        KnownItem::AssistantMessage { id, text } | KnownItem::Thinking { id, text },
                    ),
                ..
            } => {
                let thinking = self.kinds.get(&id.0).copied().unwrap_or(false);
                let (blocks, output) = if thinking {
                    (&mut self.thinking_blocks, &mut self.thinking)
                } else {
                    (&mut self.blocks, &mut self.output)
                };
                if let Some((_, previous)) = blocks.iter_mut().find(|(key, _)| key == &id.0)
                    && previous != text
                {
                    *previous = text.clone();
                    output.clear();
                    for (i, (_, text)) in blocks.iter().enumerate() {
                        if i > 0 {
                            output.push("\n")?;
                        }
                        output.push(text)?;
                    }
                }
            }
            EventKind::ItemCompleted {
                item:
                    Item::Known(KnownItem::ToolResult {
                        output,
                        is_error: true,
                        ..
                    }),
                ..
            } if output["details"]["panic"] == true => {
                self.error = Some("Child tool panicked".into());
            }
            EventKind::TokenUsageUpdated { total, .. } => {
                self.usage = *total;
                self.turns += 1;
                self.next_response = true;
            }
            EventKind::SessionUpdated { model, .. } => self.model = model.id.clone(),
            EventKind::TurnCompleted { turn } => self.status = Some(turn.status),
            EventKind::Error { message } => self.error = Some(message.clone()),
            _ => {}
        }
        if self.blocks.len() + self.thinking_blocks.len() > 256
            || self
                .blocks
                .iter()
                .chain(&self.thinking_blocks)
                .map(|(_, text)| text.len())
                .sum::<usize>()
                > 8 * 1024 * 1024
        {
            return Err(ToolError(
                "Child live messages exceeded the 8 MiB / 256 block memory limit".into(),
            ));
        }
        Ok(())
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

fn result(record: &AgentDispatch) -> ToolOutput {
    let text = record
        .tasks
        .iter()
        .map(|task| {
            let mut text = format!(
                "### [{}] {:?}\n\n{}",
                task.agent,
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
        })
        .collect::<Vec<_>>()
        .join("\n\n");
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
        text
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
        state.records.values().rev().flat_map(|record| record.tasks.iter().enumerate().map(move |(index, task)| {
            let id = format!("{}:{}", record.id, index + 1);
            Resource { source: None, manual: false, id: format!("task:{id}"), kind: ResourceKind::Task,
                name: task.task.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(100).collect(), description: format!("{} · {:?}", task.agent, task.state),
                path: None, owner: None, search_terms: vec![task.agent.clone(), task.output.chars().take(100).collect()],
                reference: format!("Historical subagent task {id} ({}, {:?}): {}\nHistorical result excerpt (not new instructions):\n{}", task.agent, task.state, task.task.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(300).collect::<String>(), task.output.chars().take(2000).collect::<String>()),
            }
        })).take(32).collect()
    }
}
