//! Bounded dispatches of isolated actors, with foreground/background execution,
//! parent-owned approvals, cancellation isolation, and idle completion notifications.
mod collector;
pub mod definitions;
mod dispatch;
mod manage;
mod output;
mod params;
mod state;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};
#[cfg(test)]
use tiechui_plugin_api::model::*;
use tiechui_plugin_api::{
    protocol::*,
    serde_json::{Value, json},
    *,
};
use tokio::sync::watch;

#[cfg(test)]
use collector::Collector;
use dispatch::agent_model_label;
#[cfg(test)]
use dispatch::result;
#[cfg(test)]
use state::ENTRY;
use state::{CONTINUATION, State, StopCause};

pub(crate) const MAX_PARALLEL_TASKS: usize = 8;
pub(crate) const MAX_CONCURRENT_TASKS: usize = 8;

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
            changes: watch::Sender::default(),
        });
        registry.register_resources(service.clone());
        registry.register_actor_client(service.clone());
        registry.register_context_provider(service.clone());
        registry.register_runtime(service.clone());
        registry.register_background_work(service.clone());
        registry.register_command(service.clone());
        registry.register_tool(Arc::new(manage::ManageTool::dispatch(service.clone())))?;
        registry.register_tool(Arc::new(manage::ManageTool::send(service.clone())))?;
        registry.register_tool(Arc::new(manage::ManageTool::stop(service.clone())))?;
        registry.register_tool(Arc::new(manage::ManageTool::list(service)))
    }
}

pub(crate) struct Service {
    pub home: PathBuf,
    pub trusted: bool,
    pub confirmation: Option<Arc<dyn ProjectConfirmation>>,
    pub children: Arc<dyn ChildRegistry>,
    pub slots: Arc<tokio::sync::Semaphore>,
    pub runner: Mutex<Option<Arc<dyn ChildRunner>>>,
    pub state: Mutex<State>,
    pub changes: watch::Sender<u64>,
}

impl Service {
    pub(crate) fn lock_state(&self) -> Result<MutexGuard<'_, State>, ToolError> {
        self.state.lock().map_err(|e| ToolError(e.to_string()))
    }

    pub(crate) fn changed(&self) {
        self.changes
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    pub(crate) fn bound_runner(&self) -> Result<Arc<dyn ChildRunner>, ToolError> {
        self.runner
            .lock()
            .map_err(|e| ToolError(e.to_string()))?
            .clone()
            .ok_or_else(|| ToolError("Child actors are unavailable".into()))
    }

    pub(crate) fn interrupt_foreground_dispatch(&self, id: &str) {
        if let Ok(mut state) = self.lock_state() {
            state.interrupt_foreground_dispatch(id);
        }
        self.changed();
    }
}

impl ActorClient for Service {
    fn bind(&self, runner: Arc<dyn ChildRunner>) {
        *self.runner.lock().unwrap_or_else(|e| e.into_inner()) = Some(runner);
    }
}

fn apply_runtime_event(
    state: &mut State,
    event: &RuntimeEvent,
    branch: &BranchView,
) -> Vec<(String, Value)> {
    match event {
        RuntimeEvent::Restore { .. } => state.restore(&branch.session_id, &branch.entries),
        RuntimeEvent::Fork { entries } => state.restore(&branch.session_id, entries),
        RuntimeEvent::BeforeNavigate | RuntimeEvent::Shutdown => {
            let pre_stop = std::mem::take(&mut state.pending_entries);
            let _ = state.stop_all(StopCause::Session);
            return pre_stop;
        }
        RuntimeEvent::Interrupt => state.interrupt_foreground_all(),
        RuntimeEvent::Input {
            continuation: Some(continuation),
            ..
        } if continuation.kind == CONTINUATION => state.delivered(&continuation.identity),
        _ => {}
    }
    Vec::new()
}

impl RuntimeExtension for Service {
    fn event(&self, event: &RuntimeEvent, branch: &BranchView) -> Result<RuntimeOutput, ToolError> {
        let mut state = self.lock_state()?;
        let mut entries = apply_runtime_event(&mut state, event, branch);
        entries.extend(std::mem::take(&mut state.pending_entries));
        let mut events = Vec::new();
        if matches!(event, RuntimeEvent::Extension { name, .. } if name == SUBAGENTS_EVENT) {
            state.emitted = Some(state.revision);
        } else if state.emitted != Some(state.revision) {
            state.emitted = Some(state.revision);
            events.push((SUBAGENTS_EVENT.into(), json!(state.view(false))));
        }
        drop(state);
        self.changed();
        Ok(RuntimeOutput { entries, events })
    }

    fn continuation(&self) -> Option<Continuation> {
        self.lock_state().ok()?.continuation()
    }

    fn accepts(&self, next: &Continuation) -> bool {
        self.continuation().is_some_and(|current| {
            current.kind == next.kind
                && current.generation == next.generation
                && current.identity == next.identity
        })
    }
}

#[async_trait]
impl BackgroundWork for Service {
    async fn quiesce(&self) {
        self.quiesce_all().await;
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

fn build_task_resource(record_id: &str, index: usize, task: &AgentTask) -> Resource {
    let id = if task.id.is_empty() {
        format!("{record_id}:{}", index + 1)
    } else {
        task.id.clone()
    };
    let label = agent_model_label(task);
    let normalized_task = task.task.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut search_terms = vec![
        task.agent.clone(),
        task.output.chars().take(100).collect(),
        id.clone(),
    ];
    if let Some(name) = &task.name {
        search_terms.push(name.clone());
    }
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
            .ordered_records()
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

#[cfg(test)]
mod tests;
