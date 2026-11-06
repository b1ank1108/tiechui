use std::collections::HashSet;

use tiechui_model::{Message, Role, StopReason, ToolSpec, Usage};
use tiechui_providers::estimate;
use tiechui_session::{EntryKind, SessionStore};

pub(super) fn input(
    store: &SessionStore,
    excluded: &HashSet<String>,
    messages: &[Message],
    tools: &[ToolSpec],
) -> Result<u64, String> {
    let path = store
        .path_to_root(store.current_leaf().unwrap_or(&store.header().id))
        .map_err(|error| error.to_string())?;
    let mut pending_usage = None;
    let mut anchor = None;
    for (index, entry) in path.iter().enumerate() {
        match &entry.kind {
            EntryKind::Compaction { .. } => {
                // A new summary changes the prefix described by earlier usage.
                anchor = None;
                pending_usage = None;
            }
            EntryKind::Usage { usage, .. } => pending_usage = Some(*usage),
            EntryKind::Message {
                message,
                stop_reason,
                usage,
                ..
            } => {
                // Legacy entries stored snapshots immediately before the assistant.
                let usage = usage.or(pending_usage.take()).unwrap_or_default();
                if message.role == Role::Assistant
                    && !excluded.contains(&entry.id)
                    && !matches!(stop_reason, Some(StopReason::Error | StopReason::Cancelled))
                    && usage != Usage::default()
                {
                    anchor = Some((index, usage));
                }
            }
            _ => {}
        }
    }
    let Some((index, usage)) = anchor else {
        return Ok(estimate::input(messages, tools));
    };
    let trailing = path[index + 1..]
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::Message {
                message,
                stop_reason,
                ..
            } if !excluded.contains(&entry.id)
                && !matches!(stop_reason, Some(StopReason::Error | StopReason::Cancelled)) =>
            {
                Some(message)
            }
            _ => None,
        });
    Ok(usage
        .input
        .saturating_add(usage.output)
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_write)
        .saturating_add(estimate::messages(trailing)))
}
