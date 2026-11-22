use std::collections::{BTreeMap, BTreeSet};
use tiechui_plugin_api::{
    model::ToolSpec,
    protocol::ThinkingLevel,
    serde_json::{self, Value, json},
    *,
};

use crate::{MAX_CONCURRENT_TASKS, MAX_PARALLEL_TASKS, definitions};

pub(crate) fn dispatch_spec() -> ToolSpec {
    let thinking = json!({
        "type": "string",
        "enum": ["off", "minimal", "low", "medium", "high", "xhigh", "max", "inherit"]
    });
    let task = json!({
        "type": "object",
        "required": ["task"],
        "properties": {
            "agent": {"type": "string"},
            "name": {"type": "string"},
            "background": {"type": "boolean"},
            "task": {"type": "string"},
            "cwd": {"type": "string"},
            "model": {"type": "string"},
            "thinking": thinking
        }
    });
    ToolSpec {
        name: "subagent".into(),
        description: format!(
            "Delegate isolated tasks to the default general-purpose subagent or a named user/project agent. Use exactly one of task, tasks (at most {MAX_PARALLEL_TASKS}), or chain (supports {{previous}}). Set background to true to launch asynchronously and receive a <task-notification> when complete (do not poll). Up to {MAX_CONCURRENT_TASKS} child tasks run concurrently across the session; extras queue. Project definitions require user trust or interactive confirmation."
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "agent": {"type": "string"},
                "name": {"type": "string"},
                "background": {"type": "boolean"},
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

pub(crate) fn stop_spec() -> ToolSpec {
    ToolSpec {
        name: "subagent_stop".into(),
        description: "Stop a running or queued subagent task or dispatch by agent_id, name, or dispatch ID. Stopped subagents do not send a completion notification.".into(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id"],
            "properties": {
                "id": {"type": "string"}
            }
        }),
    }
}

pub(crate) fn list_spec() -> ToolSpec {
    ToolSpec {
        name: "subagent_list".into(),
        description: "List all subagent tasks in the current session with their agent_id, name, agent, mode, state, model, turns, and task summary. Do not poll subagent_list in a loop waiting for background subagents; wait for <task-notification>.".into(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {}
        }),
    }
}

#[derive(serde::Deserialize)]
pub(crate) struct RawTask {
    pub agent: Option<String>,
    pub name: Option<String>,
    pub background: Option<bool>,
    pub task: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
}

pub(crate) struct ValidatedTask {
    pub agent: Option<String>,
    pub name: Option<String>,
    pub background: Option<bool>,
    pub task: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<ChildThinking>,
}

#[derive(Clone)]
pub(crate) struct PreparedTask {
    pub id: String,
    pub name: Option<String>,
    pub background: bool,
    pub fork: bool,
    pub label: String,
    pub custom_agent: Option<definitions::Definition>,
    pub task: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<ThinkingLevel>,
}

pub(crate) fn resolve_background(
    item: Option<bool>,
    top_level: Option<bool>,
    agent: Option<bool>,
    fork: bool,
) -> bool {
    item.or(top_level).or(agent).unwrap_or(fork)
}

fn has_value(value: &Value, key: &str) -> bool {
    value.get(key).is_some_and(|v| !v.is_null())
}

fn parse_top_background(value: &Value) -> Result<Option<bool>, ToolError> {
    let Some(raw) = value.get("background").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    raw.as_bool()
        .map(Some)
        .ok_or_else(|| ToolError("`background` must be a boolean".into()))
}

fn parse_mode(value: &Value) -> Result<(&'static str, Option<bool>, Vec<RawTask>), ToolError> {
    let top_bg = parse_top_background(value)?;
    let single = ["task", "agent", "name", "cwd", "model", "thinking"]
        .into_iter()
        .any(|key| has_value(value, key));
    let modes = usize::from(single)
        + usize::from(has_value(value, "tasks"))
        + usize::from(has_value(value, "chain"));
    if modes != 1 {
        return Err(ToolError("Use exactly one of task, tasks, or chain".into()));
    }
    if let Some(tasks) = value.get("tasks").filter(|v| !v.is_null()) {
        let parsed = serde_json::from_value(tasks.clone()).map_err(|e| ToolError(e.to_string()))?;
        return Ok(("parallel", top_bg, parsed));
    }
    if let Some(tasks) = value.get("chain").filter(|v| !v.is_null()) {
        let parsed = serde_json::from_value(tasks.clone()).map_err(|e| ToolError(e.to_string()))?;
        return Ok(("chain", top_bg, parsed));
    }
    let parsed: RawTask =
        serde_json::from_value(value.clone()).map_err(|e| ToolError(e.to_string()))?;
    Ok(("single", top_bg, vec![parsed]))
}

fn validate_name(raw: Option<String>) -> Result<Option<String>, ToolError> {
    let Some(name) = raw else {
        return Ok(None);
    };
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ToolError("Subagent name must not be blank".into()));
    }
    Ok(Some(trimmed.to_owned()))
}

fn validate_raw_task(raw: RawTask) -> Result<ValidatedTask, ToolError> {
    if raw.task.trim().is_empty() {
        return Err(ToolError("Tasks must not be empty".into()));
    }
    if raw.agent.as_deref().is_some_and(|a| a.trim().is_empty()) {
        return Err(ToolError(
            "Agent name must not be blank; omit `agent` to use the default subagent".into(),
        ));
    }
    let name = validate_name(raw.name)?;
    let thinking = raw
        .thinking
        .as_deref()
        .map(parse_child_thinking)
        .transpose()?;
    Ok(ValidatedTask {
        agent: raw.agent,
        name,
        background: raw.background,
        task: raw.task,
        cwd: raw.cwd,
        model: raw.model,
        thinking,
    })
}

fn ensure_unique_batch_names(tasks: &[ValidatedTask]) -> Result<(), ToolError> {
    let mut seen = BTreeSet::new();
    for name in tasks.iter().filter_map(|task| task.name.as_deref()) {
        if !seen.insert(name) {
            return Err(ToolError(format!(
                "Duplicate subagent name '{name}' in the same dispatch"
            )));
        }
    }
    Ok(())
}

pub(crate) fn parameters(
    value: &Value,
) -> Result<(&'static str, Option<bool>, Vec<ValidatedTask>), ToolError> {
    let (mode, top_bg, raw_tasks) = parse_mode(value)?;
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
    ensure_unique_batch_names(&tasks)?;
    Ok((mode, top_bg, tasks))
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

fn lookup_agent<'a>(
    name: &str,
    catalog: &'a definitions::Catalog,
) -> Result<&'a definitions::Definition, ToolError> {
    if let Some(def) = catalog.agents.get(name) {
        return Ok(def);
    }
    if let Some((path, reason)) = catalog.invalid.get(name) {
        return Err(ToolError(format!(
            "Invalid agent definition '{name}' in {}: {reason}",
            path.display()
        )));
    }
    Err(unknown_agent_error(name, &catalog.agents))
}

fn resolve_model(task_model: Option<&str>, agent_model: Option<&str>) -> Option<String> {
    match task_model.or(agent_model) {
        None | Some("inherit") => None,
        Some(value) => Some(value.to_owned()),
    }
}

fn resolve_thinking(
    task_thinking: Option<ChildThinking>,
    agent_thinking: Option<ChildThinking>,
) -> Option<ThinkingLevel> {
    match task_thinking.or(agent_thinking) {
        None | Some(ChildThinking::Inherit) => None,
        Some(ChildThinking::Level(level)) => Some(level),
    }
}

fn prepare_task(
    task: ValidatedTask,
    top_bg: Option<bool>,
    catalog: &definitions::Catalog,
    runner: &dyn ChildRunner,
) -> Result<PreparedTask, ToolError> {
    let (label, custom_agent) = match task.agent {
        None => ("default".to_owned(), None),
        Some(name) => {
            let def = lookup_agent(&name, catalog)?.clone();
            (def.name.clone(), Some(def))
        }
    };
    let agent_model = custom_agent.as_ref().and_then(|def| def.model.as_deref());
    let agent_thinking = custom_agent.as_ref().and_then(|def| def.thinking);
    let agent_bg = custom_agent.as_ref().and_then(|def| def.background);
    let model = resolve_model(task.model.as_deref(), agent_model);
    let thinking = resolve_thinking(task.thinking, agent_thinking);
    let background = resolve_background(task.background, top_bg, agent_bg, false);
    if let Some(m) = model.as_deref() {
        runner.validate_model(m)?;
    }
    Ok(PreparedTask {
        id: String::new(),
        name: task.name,
        background,
        fork: false,
        label,
        custom_agent,
        task: task.task,
        cwd: task.cwd,
        model,
        thinking,
    })
}

pub(crate) fn prepare_tasks(
    mode: &str,
    top_bg: Option<bool>,
    tasks: Vec<ValidatedTask>,
    catalog: &definitions::Catalog,
    runner: &dyn ChildRunner,
) -> Result<Vec<PreparedTask>, ToolError> {
    let prepared = tasks
        .into_iter()
        .map(|task| prepare_task(task, top_bg, catalog, runner))
        .collect::<Result<Vec<_>, _>>()?;
    if mode == "chain"
        && prepared
            .windows(2)
            .any(|pair| pair[0].background != pair[1].background)
    {
        return Err(ToolError(
            "Chain steps cannot mix foreground and background execution".into(),
        ));
    }
    Ok(prepared)
}
