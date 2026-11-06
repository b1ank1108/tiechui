//! Passive context inspection: no model calls, session writes, or replay mutations.

use std::sync::Arc;
use tiechui_plugin_api::{
    CancellationToken, Command, CommandResult, Module, ModuleConfig, Registry, SessionView,
    ToolError, async_trait, model::Role, protocol::*,
};

pub struct ContextModule;
impl Module for ContextModule {
    fn id(&self) -> &str {
        "context"
    }
    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        registry.register_command(Arc::new(ContextCommand));
        Ok(())
    }
}

struct ContextCommand;
#[async_trait]
impl Command for ContextCommand {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "context".into(),
            description: "Inspect a read-only context snapshot".into(),
            argument_hint: Some("[system|tools|injections|messages|cache]".into()),
        }
    }
    fn needs_session_view(&self) -> bool {
        true
    }
    async fn run(&self, _: &str, _: CancellationToken) -> Result<String, ToolError> {
        Err(ToolError(
            "Context inspection requires a session view".into(),
        ))
    }
    async fn inspect(
        &self,
        arguments: &str,
        session: SessionView,
        cancel: CancellationToken,
    ) -> Result<CommandResult, ToolError> {
        let view = match arguments.trim().to_ascii_lowercase().as_str() {
            "" => ContextView::Overview,
            "system" => ContextView::System,
            "tools" => ContextView::Tools,
            "injections" => ContextView::Injections,
            "messages" => ContextView::Messages,
            "cache" => ContextView::Cache,
            _ => {
                return Err(ToolError(
                    "Usage: /context [system|tools|injections|messages|cache]".into(),
                ));
            }
        };
        let snapshot = snapshot(session, &cancel)?;
        let text = if view == ContextView::Cache {
            cache_lines(&snapshot.cache).join("\n")
        } else {
            overview(&snapshot).join("\n")
        };
        Ok(CommandResult::Panel {
            snapshot: Box::new(snapshot),
            view,
            text,
        })
    }
}

pub fn snapshot(
    session: SessionView,
    cancel: &CancellationToken,
) -> Result<ContextSnapshot, ToolError> {
    if cancel.is_cancelled() {
        return Err(ToolError("Operation aborted".into()));
    }
    let mut snapshot = ContextSnapshot {
        model: session.model, window: session.window, input_tokens: session.input_tokens,
        system: session.system, tools: session.tools, injections: session.references,
        provider_usage: session.provider_usage,
        notices: vec![
            "Category sizes use the shared UTF-16 / 3.5 estimate, including image allowances; provider serialization can differ.".into(),
            "Session context estimate uses the actor's usage anchor and trailing history, as the output clamp does. Provider usage describes a past response, not the current complete request.".into(),
            "Snapshots refresh only on reopening. Injections are references, never additional allocations.".into(),
        ], ..ContextSnapshot::default()
    };
    for item in &snapshot.system {
        if item.category != "System prompt" {
            snapshot.injections.push(ContextItem {
                source: format!("Included in System estimate · {}", item.source),
                ..item.clone()
            });
        }
    }
    for mut message in session.messages {
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        message.item.category = match message.role {
            Role::Assistant => "Assistant",
            Role::Tool => "Tool results",
            _ => "User / injected messages",
        }
        .into();
        snapshot.messages.push(message.item);
    }
    if let Some(request) = session.request {
        for (prefix, source, messages) in [
            (
                "Request-only",
                "Last observed request; extension attribution unavailable",
                request.added,
            ),
            (
                "Removed / replaced",
                "Branch contribution absent from the last observed request",
                request.removed,
            ),
        ] {
            for message in messages {
                snapshot.injections.push(ContextItem {
                    label: format!("{prefix} {}", message.item.label),
                    category: prefix.into(),
                    source: source.into(),
                    ..message.item
                });
            }
        }
        snapshot.notices.push("Request differences were captured before provider serialization against the branch at request time; later provider rewrites are outside this observation.".into());
    } else {
        snapshot.notices.push("No request captured for this branch yet. Request-only injections appear after a real turn.".into());
    }
    snapshot.total = snapshot
        .system
        .iter()
        .chain(&snapshot.tools)
        .chain(&snapshot.messages)
        .map(|item| item.tokens)
        .sum();
    snapshot.cache.omitted = session.omitted_responses;
    snapshot.cache.unavailable = session.unavailable_responses;
    snapshot.cache.folds = session.folds as u64;
    snapshot.cache.reclaimed = session.reclaimed;
    snapshot.cache.forced = session.forced_folds as u64;
    let mut active: Option<tiechui_plugin_api::ContextFold> = None;
    let mut cold = 0;
    let mut previous: Option<(String, u64)> = None;
    for response in session.responses {
        if cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let usage = response.usage;
        let input = usage
            .input
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write);
        let miss = input.saturating_sub(usage.cache_read);
        if response.reset
            || previous
                .as_ref()
                .is_some_and(|(model, _)| model != &response.model)
        {
            previous = None;
            active = None;
        }
        let fold = if previous.is_some() {
            response.fold
        } else {
            None
        };
        if let Some(fold) = &fold {
            active = Some(fold.clone());
            cold = 0;
        }
        let fresh = previous
            .as_ref()
            .filter(|(model, _)| !response.reset && model == &response.model)
            .map_or(0, |(_, baseline)| {
                miss.min(
                    input.saturating_sub(
                        baseline.saturating_sub(
                            fold.as_ref()
                                .map_or(0, |f| f.before.saturating_sub(f.after)),
                        ),
                    ),
                )
            });
        let compression = active
            .as_ref()
            .filter(|_| previous.is_some() && cold < 4)
            .map_or(0, |f| {
                (miss - fresh).min(input.saturating_sub(f.prefix).saturating_sub(fresh))
            });
        snapshot.cache.responses.push(CacheResponse {
            compression,
            input,
            cached: usage.cache_read,
            fresh,
            unattributed: miss - fresh - compression,
        });
        previous = Some((response.model, input));
        cold += 1;
        if usage.cache_read as f64 / input.max(1) as f64 >= 0.85 || cold >= 4 {
            active = None;
        }
    }
    while snapshot.cache.responses.len() > 128 {
        snapshot.cache.responses.remove(0);
        snapshot.cache.omitted += 1;
    }
    Ok(snapshot)
}
