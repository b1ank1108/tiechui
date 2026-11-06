use std::collections::{BTreeSet, HashSet};
use std::ops::Range;

use tiechui_model::{Content, Message, Role, StopReason};
use tiechui_plugin_api::{CompactionPreparation, RestoredContext, serde_json::Value};
use tiechui_providers::estimate;
use tiechui_session::{EntryKind, SessionStore};

pub(super) struct Prepared {
    pub(super) preparation: CompactionPreparation,
    /// Entry IDs of `messages`, then of `turn_prefix`.
    pub(super) sources: Vec<String>,
    pub(super) retained: Vec<Message>,
    /// Items restored by the checkpoint this one replaces.
    pub(super) restored: Vec<RestoredContext>,
}

pub(super) fn prepare(
    store: &SessionStore,
    excluded: &HashSet<String>,
    keep: u64,
    tokens_before: u64,
) -> Option<Prepared> {
    if store
        .entries()
        .iter()
        .find(|entry| Some(entry.id.as_str()) == store.current_leaf())
        .is_some_and(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
    {
        return None;
    }
    let mut entries = store.project(excluded);
    for (entry, messages) in &mut entries {
        if matches!(
            entry.kind,
            EntryKind::Message {
                stop_reason: Some(StopReason::Error | StopReason::Cancelled),
                ..
            }
        ) {
            messages.clear();
        }
        messages.retain(|message| message.role != Role::System);
    }
    let previous = entries.iter().position(|(entry, messages)| {
        matches!(entry.kind, EntryKind::Compaction { .. }) && !messages.is_empty()
    });
    let start = previous.map_or(0, |index| index + 1);
    let mut pending = HashSet::new();
    let mut cuts = Vec::new();
    for (index, (_, messages)) in entries.iter().enumerate().skip(start) {
        if pending.is_empty()
            && messages
                .iter()
                .any(|m| matches!(m.role, Role::User | Role::Assistant))
        {
            cuts.push(index);
        }
        for message in messages {
            for content in &message.content {
                match content {
                    Content::ToolCall(call) => {
                        pending.insert(call.id.clone());
                    }
                    Content::ToolResult(result) => {
                        pending.remove(&result.call_id);
                    }
                    _ => {}
                }
            }
        }
    }
    let mut cut = *cuts.first()?;
    let mut accumulated = 0u64;
    for index in (start..entries.len()).rev() {
        let tokens = estimate::messages(&entries[index].1);
        if tokens == 0 {
            continue;
        }
        accumulated = accumulated.saturating_add(tokens);
        if accumulated >= keep {
            cut = *cuts
                .iter()
                .find(|candidate| **candidate >= index)
                .or_else(|| cuts.last())?;
            let suffix = &entries[cut + 1..];
            if suffix.iter().all(|(_, messages)| messages.is_empty()) && suffix.iter().any(|(entry, _)| matches!(entry.kind, EntryKind::Message { ref message, .. } if message.role == Role::Assistant)) {
                cut += 1;
            }
            break;
        }
    }
    while cut > start
        && entries[cut - 1].1.is_empty()
        && !matches!(entries[cut - 1].0.kind, EntryKind::Compaction { .. })
    {
        cut -= 1;
    }
    let first_kept_entry_id = entries.get(cut)?.0.id.clone();
    let turn_start = if entries[cut].1.iter().any(|m| m.role == Role::User) {
        None
    } else {
        (start..=cut)
            .rev()
            .find(|index| entries[*index].1.iter().any(|m| m.role == Role::User))
    };
    let history_end = turn_start.unwrap_or(cut);
    let collect = |from, to| -> (Vec<String>, Vec<Message>) {
        entries[from..to]
            .iter()
            .filter(|(entry, _)| !matches!(entry.kind, EntryKind::Compaction { .. }))
            .flat_map(|(entry, messages)| {
                messages
                    .iter()
                    .map(|message| (entry.id.clone(), message.clone()))
            })
            .unzip()
    };
    let (mut sources, messages) = collect(start, history_end);
    let (prefix_sources, turn_prefix) =
        turn_start.map_or_else(Default::default, |from| collect(from, cut));
    if messages.is_empty() && turn_prefix.is_empty() {
        return None;
    }
    sources.extend(prefix_sources);
    let retained = collect(cut, entries.len()).1;
    let mut read = BTreeSet::new();
    let mut modified = BTreeSet::new();
    let mut restored = Vec::new();
    let previous_summary = previous.and_then(|index| {
        let EntryKind::Compaction {
            summary,
            details,
            from_extension,
            ..
        } = &entries[index].0.kind
        else {
            return None;
        };
        if !from_extension {
            for (key, files) in [("read_files", &mut read), ("modified_files", &mut modified)] {
                files.extend(
                    details[key]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|v| v.as_str().map(str::to_owned)),
                );
            }
        }
        restored = details["restored"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(restored_item)
            .collect();
        Some(summary.clone())
    });
    for message in messages.iter().chain(&turn_prefix) {
        for content in &message.content {
            if let Content::ToolCall(call) = content
                && let Some(path) = call.arguments["path"].as_str()
            {
                match call.name.as_str() {
                    "read" => {
                        read.insert(path.to_owned());
                    }
                    "write" | "edit" => {
                        modified.insert(path.to_owned());
                    }
                    _ => {}
                }
            }
        }
    }
    read.retain(|path| !modified.contains(path));
    Some(Prepared {
        preparation: CompactionPreparation {
            first_kept_entry_id,
            messages,
            turn_prefix,
            previous_summary,
            tokens_before,
            read_files: read.into_iter().collect(),
            modified_files: modified.into_iter().collect(),
        },
        sources,
        retained,
        restored,
    })
}

fn restored_item(value: &Value) -> Option<RestoredContext> {
    let field = |key: &str| value[key].as_str().map(str::to_owned);
    Some(RestoredContext {
        source: field("source")?,
        key: field("key")?,
        version: field("version")?,
        text: field("text")?,
    })
}

/// API rounds: a user or assistant message opens one once every earlier tool call has a result,
/// so omitting whole rounds never separates a call from its result.
pub(super) fn rounds(messages: &[Message]) -> Vec<Range<usize>> {
    let mut starts = vec![0];
    let mut pending = HashSet::new();
    for (index, message) in messages.iter().enumerate() {
        if index > 0 && pending.is_empty() && matches!(message.role, Role::User | Role::Assistant) {
            starts.push(index);
        }
        for content in &message.content {
            match content {
                Content::ToolCall(call) => {
                    pending.insert(&call.id);
                }
                Content::ToolResult(result) => {
                    pending.remove(&result.call_id);
                }
                _ => {}
            }
        }
    }
    starts.push(messages.len());
    starts
        .windows(2)
        .map(|pair| pair[0]..pair[1])
        .filter(|range| !range.is_empty())
        .collect()
}

/// Serialized rounds with `omitted` rounds after the first `pinned` replaced by a marker.
pub(super) fn conversation(
    parts: &[String],
    pinned: usize,
    omitted: usize,
    count: usize,
) -> String {
    let marker = (omitted > 0).then(|| {
        format!("[{count} earlier messages omitted to fit this request; they remain in the session history]")
    });
    parts[..pinned]
        .iter()
        .map(String::as_str)
        .chain(marker.as_deref())
        .chain(parts[pinned + omitted..].iter().map(String::as_str))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn text(content: &[Content], separator: &str) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(separator)
}

pub(super) fn serialize(messages: &[Message]) -> String {
    let mut parts = Vec::new();
    for message in messages {
        match message.role {
            Role::User => {
                let text = text(&message.content, "");
                if !text.is_empty() {
                    parts.push(format!("[User]: {text}"));
                }
            }
            Role::Assistant => {
                if message
                    .content
                    .iter()
                    .any(|part| matches!(part, Content::Text { .. }))
                {
                    parts.push(format!("[Assistant]: {}", text(&message.content, "\n")));
                }
                let calls: Vec<_> = message
                    .content
                    .iter()
                    .filter_map(|part| {
                        let Content::ToolCall(call) = part else {
                            return None;
                        };
                        let args = call
                            .arguments
                            .as_object()
                            .map(|args| {
                                args.iter()
                                    .map(|(key, value)| format!("{key}={value}"))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            })
                            .unwrap_or_default();
                        Some(format!("{}({args})", call.name))
                    })
                    .collect();
                if !calls.is_empty() {
                    parts.push(format!("[Assistant tool calls]: {}", calls.join("; ")));
                }
            }
            Role::Tool => {
                for part in &message.content {
                    if let Content::ToolResult(result) = part {
                        let mut text = text(&result.content, "");
                        let chars: Vec<_> = text.encode_utf16().collect();
                        if chars.len() > 2000 {
                            text = format!(
                                "{}\n\n[... {} more characters truncated]",
                                String::from_utf16_lossy(&chars[..2000]),
                                chars.len() - 2000
                            );
                        }
                        if !text.is_empty() {
                            parts.push(format!("[Tool result]: {text}"));
                        }
                    }
                }
            }
            Role::System => {}
        }
    }
    parts.join("\n\n")
}

pub(super) fn prompt(
    conversation: &str,
    previous: Option<&str>,
    instructions: Option<&str>,
    split: bool,
) -> String {
    let mut prompt = if split {
        format!(
            "# Conversation\n{conversation}\n\n# Instructions\n{}",
            include_str!("turn.txt")
        )
    } else {
        let mut text = format!("<conversation>\n{conversation}\n</conversation>\n\n");
        if let Some(previous) = previous {
            text.push_str(&format!(
                "<previous-summary>\n{previous}\n</previous-summary>\n\n"
            ));
        }
        text.push_str(if previous.is_some() {
            include_str!("update.txt")
        } else {
            include_str!("summary.txt")
        });
        text
    };
    if let Some(instructions) = instructions.filter(|text| !text.is_empty()) {
        prompt.push_str(&format!("\n\nAdditional focus: {instructions}"));
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiechui_model::{ModelId, ToolCall, ToolResult};
    use tiechui_session::ModelRef;
    use tokio_util::sync::CancellationToken;

    fn append(store: &mut SessionStore, role: Role, content: Vec<Content>) -> String {
        store
            .append(
                EntryKind::Message {
                    message: Message {
                        role,
                        content,
                        opaque: None,
                    },
                    model: (role == Role::Assistant).then(|| ModelRef {
                        provider: "test".into(),
                        model: ModelId("test".into()),
                    }),
                    usage: None,
                    stop_reason: None,
                    reasoning: None,
                    details: None,
                },
                &CancellationToken::new(),
            )
            .unwrap()
            .id
            .clone()
    }
    fn text(text: &str) -> Content {
        Content::Text {
            text: text.into(),
            opaque: None,
        }
    }

    #[test]
    fn cuts_keep_tool_pairs_and_images_and_track_file_operations() {
        let temp = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut store =
            SessionStore::create(temp.path(), temp.path(), &CancellationToken::new()).unwrap();
        append(&mut store, Role::User, vec![text("old request")]);
        append(
            &mut store,
            Role::Assistant,
            vec![Content::ToolCall(ToolCall {
                id: "read".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path":"file.rs"}),
                opaque: None,
            })],
        );
        append(
            &mut store,
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call_id: "read".into(),
                content: vec![text("read result")],
                is_error: false,
            })],
        );
        append(&mut store, Role::Assistant, vec![text("done")]);
        let user = append(
            &mut store,
            Role::User,
            vec![
                text("new request"),
                Content::Image {
                    mime_type: "image/png".into(),
                    data: "AA==".into(),
                },
            ],
        );
        let call = append(
            &mut store,
            Role::Assistant,
            vec![Content::ToolCall(ToolCall {
                id: "edit".into(),
                name: "edit".into(),
                arguments: serde_json::json!({"path":"file.rs"}),
                opaque: None,
            })],
        );
        append(
            &mut store,
            Role::Tool,
            vec![Content::ToolResult(ToolResult {
                call_id: "edit".into(),
                content: vec![text(&"result ".repeat(100))],
                is_error: false,
            })],
        );
        let prepared = prepare(&store, &HashSet::new(), 10, 100)
            .unwrap()
            .preparation;
        assert_eq!(prepared.first_kept_entry_id, call);
        assert_eq!(prepared.read_files, ["file.rs"]);
        assert!(
            prepared
                .turn_prefix
                .iter()
                .any(|m| m.content.iter().any(|c| matches!(c, Content::Image { .. })))
        );
        let prepared = prepare(&store, &HashSet::new(), 300, 100)
            .unwrap()
            .preparation;
        assert_eq!(prepared.first_kept_entry_id, user);
        assert!(prepared.turn_prefix.is_empty());
        append(
            &mut store,
            Role::User,
            vec![text(&"another request ".repeat(100))],
        );
        let prepared = prepare(&store, &HashSet::new(), 10, 100)
            .unwrap()
            .preparation;
        assert!(prepared.read_files.is_empty());
        assert_eq!(prepared.modified_files, ["file.rs"]);
        store.append(EntryKind::Compaction {
            summary: "previous checkpoint".into(), first_kept_entry_id: Some(prepared.first_kept_entry_id),
            tokens_before: 100, usage: None, system_message: None, from_extension: false,
            details: serde_json::json!({"read_files":["prior.rs"], "modified_files": prepared.modified_files}),
        }, &CancellationToken::new()).unwrap();
        append(
            &mut store,
            Role::User,
            vec![text(&"latest input ".repeat(100))],
        );
        let prepared = prepare(&store, &HashSet::new(), 10, 100)
            .unwrap()
            .preparation;
        assert_eq!(
            prepared.previous_summary.as_deref(),
            Some("previous checkpoint")
        );
        assert_eq!(prepared.read_files, ["prior.rs"]);
        assert_eq!(prepared.modified_files, ["file.rs"]);
    }

    #[test]
    fn threshold_is_strict_with_zero_and_oversized_reserves() {
        let mut config = tiechui_plugin_api::CompactionConfig {
            enabled: true,
            reserve_tokens: 100,
            keep_recent_tokens: 0,
            ..Default::default()
        };
        assert!(!super::super::should_compact(900, 1000, &config));
        assert!(super::super::should_compact(901, 1000, &config));
        config.enabled = false;
        assert!(!super::super::should_compact(2000, 1000, &config));
        config.enabled = true;
        config.reserve_tokens = 0;
        assert!(!super::super::should_compact(1000, 1000, &config));
        config.reserve_tokens = 1001;
        assert!(super::super::should_compact(0, 1000, &config));
    }

    #[test]
    fn serialization_omits_thinking_images_and_truncates_tool_results() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    Content::Thinking {
                        text: "PRIVATE_THINKING".into(),
                        redacted: false,
                        opaque: None,
                    },
                    text("visible"),
                ],
                opaque: None,
            },
            Message {
                role: Role::Tool,
                content: vec![Content::ToolResult(ToolResult {
                    call_id: "x".into(),
                    content: vec![
                        text(&"x".repeat(2005)),
                        Content::Image {
                            mime_type: "image/png".into(),
                            data: "IMAGE_BYTES".into(),
                        },
                    ],
                    is_error: false,
                })],
                opaque: None,
            },
        ];
        let serialized = serialize(&messages);
        assert!(!serialized.contains("PRIVATE_THINKING"));
        assert!(!serialized.contains("IMAGE_BYTES"));
        assert!(serialized.contains("[... 5 more characters truncated]"));
        let prompt = prompt(&serialized, None, Some("focus"), true);
        assert!(prompt.starts_with("# Conversation\n"));
        assert!(prompt.contains("## Original Request"));
        assert!(prompt.ends_with("Additional focus: focus"));
    }
}
