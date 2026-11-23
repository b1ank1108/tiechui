use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tiechui_plugin_api::{
    model::{ToolCall, ToolSpec},
    protocol::*,
    serde_json::{Value, json},
    *,
};

use crate::{
    Service,
    dispatch::wait_done,
    params::{dispatch_spec, list_spec, send_spec, stop_spec},
    state::{StopCause, Target},
};

const USAGE: &str = "Usage: /subagents [background|stop <id>]";

enum ManageKind {
    Dispatch,
    Send,
    List,
    Stop,
}

pub(crate) struct ManageTool {
    service: Arc<Service>,
    kind: ManageKind,
    spec: ToolSpec,
}

impl ManageTool {
    pub fn dispatch(service: Arc<Service>) -> Self {
        Self {
            service,
            kind: ManageKind::Dispatch,
            spec: dispatch_spec(),
        }
    }

    pub fn send(service: Arc<Service>) -> Self {
        Self {
            service,
            kind: ManageKind::Send,
            spec: send_spec(),
        }
    }

    pub fn list(service: Arc<Service>) -> Self {
        Self {
            service,
            kind: ManageKind::List,
            spec: list_spec(),
        }
    }

    pub fn stop(service: Arc<Service>) -> Self {
        Self {
            service,
            kind: ManageKind::Stop,
            spec: stop_spec(),
        }
    }

    fn run_list(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if call
            .arguments
            .as_object()
            .is_some_and(|obj| !obj.is_empty())
        {
            return Err(ToolError("subagent_list accepts no arguments".into()));
        }
        let state = self.service.lock_state()?;
        if state.session != ctx.scope.thread_id.0 {
            return Err(ToolError("Subagents belong to another session".into()));
        }
        let unique = collect_unique_tasks(&state);
        let mut lines = Vec::with_capacity(unique.len());
        let mut items = Vec::with_capacity(unique.len());
        for (dispatch_id, task) in &unique {
            lines.push(format_list_line(task));
            items.push(list_item_json(dispatch_id, task));
        }
        let text = if lines.is_empty() {
            "No subagents in this session.".into()
        } else {
            lines.join("\n")
        };
        Ok(ToolOutput {
            structured_content: Some(json!({ "agents": items })),
            ..ToolOutput::text(text)
        })
    }

    async fn run_stop(&self, call: &ToolCall, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let id = call.arguments["id"].as_str().unwrap_or_default().trim();
        if id.is_empty() {
            return Err(ToolError("`id` is required".into()));
        }
        let (target, waiters) = {
            let mut state = self.service.lock_state()?;
            if state.session != ctx.scope.thread_id.0 {
                return Err(ToolError("Subagents belong to another session".into()));
            }
            state.stop_target(id, StopCause::Request)?
        };
        self.service.changed();
        wait_done(waiters, Duration::from_secs(5)).await;
        self.service.flush_entries(ctx)?;
        let state = self.service.lock_state()?;
        Ok(stop_report(&state, id, &target))
    }
}

#[async_trait]
impl Tool for ManageTool {
    fn source(&self) -> Option<String> {
        Some("Built-in tools".into())
    }
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Shared
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        match self.kind {
            ManageKind::Dispatch => self.service.dispatch(call, ctx).await,
            ManageKind::Send => self.service.send(call, ctx).await,
            ManageKind::List => self.run_list(&call, &ctx),
            ManageKind::Stop => self.run_stop(&call, &ctx).await,
        }
    }
}

fn merge_list_task(prev: &AgentTask, latest: &AgentTask) -> AgentTask {
    let mut merged = latest.clone();
    merged.turns = prev.turns.saturating_add(latest.turns);
    if merged.model.is_empty() {
        merged.model = prev.model.clone();
    }
    merged
}

fn collect_unique_tasks(state: &crate::state::State) -> Vec<(String, AgentTask)> {
    let mut index_by_id: BTreeMap<String, usize> = BTreeMap::new();
    let mut items: Vec<(String, AgentTask)> = Vec::new();
    for record in state.ordered_records() {
        for task in &record.tasks {
            if !task.id.is_empty()
                && let Some(&idx) = index_by_id.get(&task.id)
            {
                let merged = merge_list_task(&items[idx].1, task);
                items[idx] = (record.id.clone(), merged);
            } else {
                if !task.id.is_empty() {
                    index_by_id.insert(task.id.clone(), items.len());
                }
                items.push((record.id.clone(), task.clone()));
            }
        }
    }
    items
}

fn one_line_summary(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}

fn format_list_line(task: &AgentTask) -> String {
    let mode = match (task.background, task.fork) {
        (true, true) => "background/fork",
        (true, false) => "background",
        (false, true) => "foreground/fork",
        (false, false) => "foreground",
    };
    let alias = task
        .name
        .as_deref()
        .map_or_else(String::new, |name| format!(" ({name})"));
    let model = if task.model.is_empty() {
        "-".to_owned()
    } else {
        task.model.clone()
    };
    format!(
        "{}{}  {}  {}  {:?}  {}  {}t  {}",
        task.id,
        alias,
        task.agent,
        mode,
        task.state,
        model,
        task.turns,
        one_line_summary(&task.task)
    )
}

fn list_item_json(dispatch_id: &str, task: &AgentTask) -> Value {
    json!({
        "id": task.id,
        "agent_id": task.id,
        "dispatch_id": dispatch_id,
        "name": task.name,
        "agent": task.agent,
        "background": task.background,
        "fork": task.fork,
        "state": task.state,
        "model": task.model,
        "turns": task.turns,
        "summary": one_line_summary(&task.task),
        "output_path": task.output_path,
    })
}

fn stop_report(state: &crate::state::State, requested: &str, target: &Target) -> ToolOutput {
    let tasks: Vec<&AgentTask> = match target {
        Target::Task {
            dispatch_id, index, ..
        } => state
            .records
            .get(dispatch_id)
            .and_then(|r| r.tasks.get(*index))
            .into_iter()
            .collect(),
        Target::Dispatch(id) => state
            .records
            .get(id)
            .map(|r| r.tasks.iter().collect())
            .unwrap_or_default(),
    };
    let still_active = tasks.iter().any(|t| t.state.active());
    let text = match tasks.as_slice() {
        [single] if !still_active => format!("Subagent {} {:?}.", single.id, single.state),
        [single] => format!("Stopping subagent {}.", single.id),
        _ if !still_active => format!("Stopped {requested}."),
        _ => format!("Stopping {requested}."),
    };
    ToolOutput {
        structured_content: Some(json!({
            "id": requested,
            "agents": tasks.iter().map(|t| crate::dispatch::task_json(t)).collect::<Vec<_>>(),
        })),
        ..ToolOutput::text(text)
    }
}

impl Service {
    fn execute_command_action(&self, arguments: &str) -> Result<String, ToolError> {
        let mut words = arguments.split_whitespace();
        let (action, id, extra) = (words.next(), words.next(), words.next());
        if extra.is_some() {
            return Err(ToolError(USAGE.into()));
        }
        match (action, id) {
            (Some("background"), Some(id)) => {
                let mut state = self.lock_state()?;
                state.promote_target(id)?;
                drop(state);
                self.changed();
                Ok(format!("{id} moved to the background."))
            }
            (Some("stop"), Some(id)) => {
                let mut state = self.lock_state()?;
                let (_, waiters) = state.stop_target(id, StopCause::Request)?;
                drop(state);
                self.changed();
                Ok(if waiters.is_empty() {
                    format!("{id} is already settled.")
                } else {
                    format!("Stopping {id}.")
                })
            }
            _ => Err(ToolError(USAGE.into())),
        }
    }

    fn apply_input_action(&self, data: &Value) -> Result<(), ToolError> {
        let mut state = self.lock_state()?;
        if data["background_all"].as_bool() == Some(true) {
            state.promote_all();
        } else if let Some(target) = data["background"].as_str() {
            state.promote_target(target)?;
        } else if let Some(target) = data["stop"].as_str().or_else(|| data["cancel"].as_str()) {
            state.stop_target(target, StopCause::Request)?;
        }
        drop(state);
        self.changed();
        Ok(())
    }

    async fn emit_view(&self, session: &CommandSession, open: bool) -> Result<(), ToolError> {
        let view = {
            let mut state = self.lock_state()?;
            state.emitted = Some(state.revision);
            state.view(open)
        };
        session
            .host
            .apply(SessionEffect::Emit {
                kind: SUBAGENTS_EVENT.into(),
                data: json!(view),
            })
            .await
    }
}

#[async_trait]
impl Command for Service {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "subagents".into(),
            description:
                "View the main session and child tasks; move a foreground subagent to the background or stop it"
                    .into(),
            argument_hint: Some("[background|stop <id>]".into()),
        }
    }
    fn background(&self) -> bool {
        true
    }
    fn allows_busy(&self, _: &str) -> bool {
        true
    }
    async fn run(&self, arguments: &str, _: CancellationToken) -> Result<String, ToolError> {
        if arguments.trim().is_empty() {
            return Err(ToolError(
                "Subagent viewer requires an interactive terminal".into(),
            ));
        }
        self.execute_command_action(arguments)
    }
    async fn session_run(
        &self,
        arguments: &str,
        session: CommandSession,
        _: CancellationToken,
    ) -> Result<String, ToolError> {
        if arguments.trim().is_empty() {
            self.emit_view(&session, true).await?;
            return Ok(String::new());
        }
        let message = self.execute_command_action(arguments)?;
        self.emit_view(&session, false).await?;
        Ok(message)
    }
    async fn input(
        &self,
        data: Value,
        session: CommandSession,
        _: CancellationToken,
    ) -> Result<String, ToolError> {
        if self.lock_state()?.session != session.branch.session_id {
            return Ok(String::new());
        }
        self.apply_input_action(&data)?;
        self.emit_view(&session, false).await?;
        Ok(String::new())
    }
}
