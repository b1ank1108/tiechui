//! Recoverable branch-local range compaction; never rewrites session messages.
mod engine;
mod history;
mod messages;
mod nudge;
mod outputs;
mod search;
mod state;

use messages::{check, plain};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, MutexGuard},
};
use tiechui_plugin_api::{
    model::*,
    serde_json::{Value, json},
    *,
};

pub struct CompactionModule;
struct Service {
    config: CompactionConfig,
    snapshot: Mutex<Option<ProjectionContext>>,
    cadence: Mutex<nudge::Cadence>,
    search: Mutex<search::HistorySearch>,
    // Only tool calls in the same batch may read queued state; requests always restore durable entries.
    queued: Mutex<Option<(String, state::State)>>,
    /// Candidates shown by status or hints, resolved again and revalidated at commit.
    issued: Mutex<VecDeque<engine::Candidate>>,
}
fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>, ToolError> {
    mutex
        .lock()
        .map_err(|_| ToolError("Compaction state lock poisoned".into()))
}
/// History reads are the recovery path for every fold this module installs.
fn recoverable(context: &ProjectionContext) -> bool {
    context.tools.iter().any(|t| t.name == "read_history")
}
fn request_size(context: &ProjectionContext, messages: &[ProjectedMessage]) -> u64 {
    (context.estimate_message)(&Message {
        role: Role::System,
        content: vec![plain(context.system.clone())],
        opaque: None,
    }) + messages
        .iter()
        .map(|m| (context.estimate_message)(&m.message))
        .sum::<u64>()
        + (context.estimate_tools)(&context.tools)
}
impl Module for CompactionModule {
    fn id(&self) -> &str {
        "compaction"
    }
    fn register(&self, registry: &mut Registry, config: &ModuleConfig) -> Result<(), ToolError> {
        config.compaction.validate_range().map_err(ToolError)?;
        let service = Arc::new(Service {
            config: config.compaction.clone(),
            snapshot: Mutex::new(None),
            cadence: Mutex::new(nudge::Cadence::default()),
            search: Mutex::new(search::HistorySearch::default()),
            queued: Mutex::new(None),
            issued: Mutex::new(VecDeque::new()),
        });
        registry.register_projection(service.clone());
        registry.register_compactor(service.clone());
        registry.register_runtime(service.clone());
        for spec in history::specs(&config.compaction) {
            registry.register_tool(Arc::new(history::HistoryTool {
                service: service.clone(),
                spec,
            }))?;
        }
        Ok(())
    }
}
impl Service {
    fn issue(&self, candidates: &[engine::Candidate]) -> Result<(), ToolError> {
        let mut issued = lock(&self.issued)?;
        for candidate in candidates {
            issued.retain(|c| c.id != candidate.id);
            issued.push_back(candidate.clone());
        }
        while issued.len() > 256 {
            issued.pop_front();
        }
        Ok(())
    }
}
#[async_trait]
impl RequestProjection for Service {
    fn fold(&self, kind: &str, data: &Value) -> Option<ContextFold> {
        if kind != state::ENTRY {
            return None;
        }
        let state: state::State = serde_json::from_value(data.clone()).ok()?;
        state.validate().ok()?;
        state.fold.map(|f| ContextFold {
            before: f.before,
            after: f.after,
            prefix: f.prefix,
            forced: state.maintenance.is_some_and(|m| m.forced),
        })
    }

    async fn project(
        &self,
        context: ProjectionContext,
        cancel: CancellationToken,
    ) -> Result<Option<ProjectedRequest>, ToolError> {
        check(&cancel)?;
        *lock(&self.queued)? = None;
        *lock(&self.snapshot)? = Some(context.clone());
        if !recoverable(&context) {
            return Ok(None);
        }
        let config = self.config.clone();
        let work_context = context.clone();
        let work_cancel = cancel.clone();
        let work = tokio::task::spawn_blocking(move || {
            let prepared = engine::prepare(&work_context, &config, &work_cancel)?;
            let view = maintain(&work_context, &prepared, &config, &work_cancel)?;
            Ok::<_, ToolError>((prepared, view))
        });
        let (prepared, mut view) = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ToolError("Operation aborted".into())),
            result = work => result.map_err(|e| ToolError(e.to_string()))??,
        };
        let size = request_size(&context, &view.messages);
        let limit = self.config.range_window(context.window);
        let pressure = history::pressure(&self.config, size, limit);
        let cooling =
            nudge::cooling(&prepared.state, &context.history).is_some() || !view.entries.is_empty();
        if context.tools.iter().any(|t| t.name == "compact_history")
            && !(cooling && pressure == "normal")
        {
            let candidates = engine::candidates(
                &prepared,
                self.config.compress.min_compress_range,
                history::segment_tokens(limit),
                None,
            );
            let summaries = {
                let mut cadence = lock(&self.cadence)?;
                let identity = format!(
                    "{}:{}",
                    context.branch.session_id,
                    context
                        .branch
                        .entries
                        .iter()
                        .rev()
                        .find(|e| e
                            .custom
                            .as_ref()
                            .is_some_and(|(kind, _)| kind == state::ENTRY))
                        .map_or("empty", |e| e.id.as_str())
                );
                if cadence.identity != identity {
                    *cadence = nudge::Cadence {
                        identity,
                        ..Default::default()
                    };
                }
                cadence.hint(&prepared, &candidates, &self.config, context.window, size)
            };
            if let Some(summaries) = summaries {
                let kind: Vec<_> = candidates
                    .into_iter()
                    .filter(|c| matches!(c.source, engine::Source::Summaries(_)) == summaries)
                    .collect();
                let mut baseline = prepared.state.clone();
                history::settle(&context, &prepared, &mut baseline, &self.config);
                let before = request_size(
                    &context,
                    &history::request_view(&prepared, &baseline, &self.config, &cancel)?,
                );
                let described =
                    history::describe(&context, &prepared, &self.config, before, &kind, &cancel)?;
                let shown: Vec<_> = described
                    .into_iter()
                    .filter(|(_, value)| value["recommended"] == true)
                    .take(history::HINTED)
                    .collect();
                if !shown.is_empty() {
                    self.issue(&shown.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>())?;
                    view.messages.push(ProjectedMessage {
                        source: None,
                        message: Message {
                            role: Role::User,
                            content: vec![plain(format!(
                                "Context maintenance hint (not a new user task). If these candidates are completed work, you may call compact_history with their ids and your summaries; otherwise continue. Native compaction remains the capacity fallback.\n{}",
                                json!({
                                    "pressure": pressure,
                                    "candidates": shown.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
                                })
                            ))],
                            opaque: None,
                        },
                    });
                }
            }
        }
        check(&cancel)?;
        let unchanged = view.entries.is_empty()
            && view.messages.len() == context.messages.len()
            && view
                .messages
                .iter()
                .zip(&context.messages)
                .all(|(a, b)| a.message == *b);
        Ok((!unchanged).then_some(view))
    }
}
/// The request view, recording newly eligible output previews as one maintenance point when the
/// cache cooldown allows it, capacity requires it, or the provider cache has already expired.
fn maintain(
    context: &ProjectionContext,
    p: &engine::Prepared,
    config: &CompactionConfig,
    cancel: &CancellationToken,
) -> Result<ProjectedRequest, ToolError> {
    let current = ProjectedRequest {
        messages: history::request_view(p, &p.state, config, cancel)?,
        entries: vec![],
    };
    let size = request_size(context, &current.messages);
    let pressured =
        history::pressure(config, size, config.range_window(context.window)) != "normal";
    let cooling = nudge::cooling(&p.state, &context.history).is_some();
    if cooling && !pressured && !context.cache_cold {
        return Ok(current);
    }
    let mut next = p.state.clone();
    history::settle(context, p, &mut next, config);
    if next.outputs.len() == p.state.outputs.len() {
        return Ok(current);
    }
    let view = history::request_view(p, &next, config, cancel)?;
    let fold = history::effect(context, &current.messages, &view, 0);
    let minimum = if pressured {
        1
    } else {
        config.compress.min_net_savings_tokens.max(1)
    };
    if fold.before < fold.after + minimum {
        return Ok(current);
    }
    next.version = state::VERSION;
    next.fold = Some(fold);
    next.maintenance = Some(state::Maintenance {
        at: context.history.len(),
        forced: cooling && pressured && !context.cache_cold,
    });
    Ok(ProjectedRequest {
        messages: view,
        entries: vec![(state::ENTRY.into(), json!(next))],
    })
}
#[async_trait]
impl Compactor for Service {
    async fn before_compact(
        &self,
        _: &CompactionContext,
        cancel: CancellationToken,
    ) -> Result<CompactionOutcome, ToolError> {
        check(&cancel)?;
        Ok(CompactionOutcome::Continue)
    }
}
impl RuntimeExtension for Service {
    fn event(&self, event: &RuntimeEvent, _: &BranchView) -> Result<RuntimeOutput, ToolError> {
        let mut output = RuntimeOutput::default();
        if matches!(
            event,
            RuntimeEvent::Restore { .. } | RuntimeEvent::BeforeNavigate | RuntimeEvent::Shutdown
        ) {
            *lock(&self.snapshot)? = None;
            *lock(&self.queued)? = None;
            *lock(&self.search)? = search::HistorySearch::default();
            *lock(&self.cadence)? = nudge::Cadence::default();
            lock(&self.issued)?.clear();
        }
        if let RuntimeEvent::Fork { entries } = event
            && entries
                .iter()
                .any(|e| e.custom.as_ref().is_some_and(|(k, _)| k == state::ENTRY))
        {
            match state::State::read(entries) {
                Ok(mut state) => {
                    state.fold = None;
                    output.entries.push((state::ENTRY.into(), json!(state)));
                }
                Err(error) => output.events.push(("module.notice".into(), json!(error.0))),
            }
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests;
