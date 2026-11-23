//! How an external session lives in the `SessionStore`: normalized messages and usage as the
//! native runtime writes them, plus a few custom entries for what messages cannot hold.
//!
//! - header `runtime`: the runtime that runs the session, fixed when the file is created, so a
//!   listing and a reopen know it without reading entries;
//! - `runtime.thread` entry: the external thread id. It is an entry, not a header field, because
//!   it can change (a resume the runtime refuses starts a new thread) and the file is
//!   append-only; the latest one on the branch wins;
//! - `runtime.item` entry: an item messages cannot express (a sub-agent's item or status);
//! - `runtime.derived_turn` entry: opens a derived turn.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tiechui_model::{Content, Message, ReasoningLevel, Role, Usage};
use tiechui_protocol::{
    DerivedReason, DerivedTurnMarker, InputContent, Item, ItemId, KnownItem, ThinkingLevel,
    TokenUsage, Turn, TurnId, TurnStatus,
};
use tiechui_session::{Entry, EntryKind, SessionStore};

pub const THREAD_ENTRY: &str = "runtime.thread";
pub const ITEM_ENTRY: &str = "runtime.item";
pub const DERIVED_ENTRY: &str = "runtime.derived_turn";

#[derive(Serialize, Deserialize)]
struct ThreadData {
    id: String,
}

#[derive(Serialize, Deserialize)]
struct DerivedData {
    derived_from: TurnId,
    reason: DerivedReason,
}

pub fn thread_entry(external_id: &str) -> EntryKind {
    EntryKind::Custom {
        kind: THREAD_ENTRY.into(),
        data: json!({ "id": external_id }),
    }
}

pub fn item_entry(item: &Item) -> EntryKind {
    EntryKind::Custom {
        kind: ITEM_ENTRY.into(),
        data: serde_json::to_value(item).unwrap_or(Value::Null),
    }
}

pub fn derived_entry(derived_from: &TurnId, reason: DerivedReason) -> EntryKind {
    EntryKind::Custom {
        kind: DERIVED_ENTRY.into(),
        data: json!({ "derived_from": derived_from, "reason": reason }),
    }
}

pub fn user_entry(content: &[InputContent]) -> EntryKind {
    EntryKind::Message {
        message: Message {
            role: Role::User,
            content: content
                .iter()
                .map(|part| match part {
                    InputContent::Text { text } => Content::Text {
                        text: text.clone(),
                        opaque: None,
                    },
                    InputContent::Image { mime_type, data } => Content::Image {
                        mime_type: mime_type.clone(),
                        data: data.clone(),
                    },
                })
                .collect(),
            opaque: None,
        },
        usage: None,
        model: None,
        stop_reason: None,
        reasoning: None,
        details: None,
    }
}

pub fn usage_entry(usage: &TokenUsage, model: tiechui_session::ModelRef) -> EntryKind {
    EntryKind::Usage {
        usage: usage_of(usage),
        model: Some(model),
        request_id: None,
    }
}

pub fn usage_of(usage: &TokenUsage) -> Usage {
    Usage {
        partial: false,
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        reasoning: usage.reasoning,
        cache_write_1h: usage.cache_write_1h,
        cost_nano_usd: usage.cost_nano_usd,
    }
}

pub fn tokens_of(usage: &Usage) -> TokenUsage {
    TokenUsage {
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        reasoning: usage.reasoning,
        cache_write_1h: usage.cache_write_1h,
        cost_nano_usd: usage.cost_nano_usd,
    }
}

pub fn item_message(item: &KnownItem, model: &tiechui_session::ModelRef) -> Option<EntryKind> {
    let assistant = |content| EntryKind::Message {
        message: Message {
            role: Role::Assistant,
            content: vec![content],
            opaque: None,
        },
        usage: None,
        model: Some(model.clone()),
        stop_reason: None,
        reasoning: None,
        details: None,
    };
    match item {
        KnownItem::AssistantMessage { text, .. } => Some(assistant(Content::Text {
            text: text.clone(),
            opaque: None,
        })),
        KnownItem::Thinking { text, .. } => Some(assistant(Content::Thinking {
            text: text.clone(),
            redacted: false,
            opaque: None,
        })),
        KnownItem::ToolCall {
            id,
            name,
            arguments,
        } => Some(assistant(Content::ToolCall(tiechui_model::ToolCall {
            id: id.0.clone(),
            name: name.clone(),
            arguments: arguments.clone(),
            opaque: None,
        }))),
        KnownItem::ToolResult {
            call_id,
            output,
            is_error,
            ..
        } => Some(EntryKind::Message {
            message: Message {
                role: Role::Tool,
                content: vec![Content::ToolResult(tiechui_model::ToolResult {
                    call_id: call_id.0.clone(),
                    content: vec![Content::Text {
                        text: output_text(output),
                        opaque: None,
                    }],
                    is_error: *is_error,
                })],
                opaque: None,
            },
            usage: None,
            model: None,
            stop_reason: None,
            reasoning: None,
            details: Some(output.clone()),
        }),
        KnownItem::UserMessage { .. } | KnownItem::Compaction { .. } => None,
    }
}

fn output_text(output: &Value) -> String {
    match output {
        Value::String(text) => text.clone(),
        Value::Object(map) => map
            .get("content")
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

pub fn external_id(store: &SessionStore) -> Option<String> {
    on_branch(store)
        .iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            EntryKind::Custom { kind, data } if kind == THREAD_ENTRY => {
                serde_json::from_value::<ThreadData>(data.clone())
                    .ok()
                    .map(|data| data.id)
            }
            _ => None,
        })
}

pub fn stored_model(store: &SessionStore, runtime: &str) -> Option<String> {
    on_branch(store)
        .iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            EntryKind::ModelChange { model } if model.provider == runtime => {
                Some(model.model.0.clone())
            }
            _ => None,
        })
}

pub fn thinking_entry(level: ThinkingLevel) -> EntryKind {
    EntryKind::ThinkingChange {
        level: match level {
            ThinkingLevel::Off => None,
            ThinkingLevel::Minimal => Some(ReasoningLevel::Minimal),
            ThinkingLevel::Low => Some(ReasoningLevel::Low),
            ThinkingLevel::Medium => Some(ReasoningLevel::Medium),
            ThinkingLevel::High => Some(ReasoningLevel::High),
            ThinkingLevel::Xhigh => Some(ReasoningLevel::Xhigh),
            ThinkingLevel::Max => Some(ReasoningLevel::Max),
        },
    }
}

pub fn stored_thinking(store: &SessionStore) -> Option<ThinkingLevel> {
    on_branch(store)
        .iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            EntryKind::ThinkingChange { level } => Some(match level {
                None => ThinkingLevel::Off,
                Some(ReasoningLevel::Minimal) => ThinkingLevel::Minimal,
                Some(ReasoningLevel::Low) => ThinkingLevel::Low,
                Some(ReasoningLevel::Medium) => ThinkingLevel::Medium,
                Some(ReasoningLevel::High) => ThinkingLevel::High,
                Some(ReasoningLevel::Xhigh) => ThinkingLevel::Xhigh,
                Some(ReasoningLevel::Max) => ThinkingLevel::Max,
            }),
            _ => None,
        })
}

pub fn usage(store: &SessionStore) -> (Option<TokenUsage>, TokenUsage) {
    let mut context = None;
    let mut total = TokenUsage::default();
    for entry in on_branch(store) {
        if let EntryKind::Usage { usage, .. } = &entry.kind {
            let usage = tokens_of(usage);
            context = Some(usage);
            total.input += usage.input;
            total.output += usage.output;
            total.cache_read += usage.cache_read;
            total.cache_write += usage.cache_write;
            total.reasoning += usage.reasoning;
            total.cache_write_1h += usage.cache_write_1h;
            total.cost_nano_usd = match (total.cost_nano_usd, usage.cost_nano_usd) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
            };
        }
    }
    (context, total)
}

fn on_branch(store: &SessionStore) -> Vec<&Entry> {
    store
        .path_to_root(store.current_leaf().unwrap_or(&store.header().id))
        .unwrap_or_default()
}

pub fn marker_item(turn_entry: &str, derived_from: &TurnId, reason: DerivedReason) -> Item {
    DerivedTurnMarker {
        id: marker_id(turn_entry),
        derived_from: derived_from.clone(),
        reason,
    }
    .into_item()
}

pub fn marker_id(turn_entry: &str) -> ItemId {
    ItemId(format!("{turn_entry}-derived"))
}

pub fn user_item_id(entry: &str) -> ItemId {
    ItemId(format!("{entry}-0"))
}

pub fn turns(store: &SessionStore) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for entry in on_branch(store) {
        match &entry.kind {
            EntryKind::Custom { kind, data } if kind == DERIVED_ENTRY => {
                push_derived(&mut turns, entry, data);
            }
            EntryKind::Custom { kind, data } if kind == ITEM_ENTRY => {
                push_runtime_item(&mut turns, entry, data);
            }
            EntryKind::Message {
                message, details, ..
            } => push_message(&mut turns, entry, message, details.as_ref()),
            _ => {}
        }
    }
    turns
}

fn push_derived(turns: &mut Vec<Turn>, entry: &Entry, data: &Value) {
    let Ok(derived) = serde_json::from_value::<DerivedData>(data.clone()) else {
        return;
    };
    turns.push(new_turn(
        &entry.id,
        entry.timestamp,
        vec![marker_item(
            &entry.id,
            &derived.derived_from,
            derived.reason,
        )],
    ));
}

fn push_runtime_item(turns: &mut [Turn], entry: &Entry, data: &Value) {
    if let (Some(turn), Ok(item)) = (
        turns.last_mut(),
        serde_json::from_value::<Item>(data.clone()),
    ) {
        upsert(turn, item);
        turn.ended_at = Some(entry.timestamp);
    }
}

fn push_message(turns: &mut Vec<Turn>, entry: &Entry, message: &Message, details: Option<&Value>) {
    let mut items = Vec::new();
    if message.role == Role::User {
        turns.push(new_turn(&entry.id, entry.timestamp, vec![]));
        items.push(Item::Known(KnownItem::UserMessage {
            id: user_item_id(&entry.id),
            content: input_content(&message.content),
        }));
    } else {
        for (index, content) in message.content.iter().enumerate() {
            let id = ItemId(format!("{}-{index}", entry.id));
            items.extend(content_item(message.role, content, id, details));
        }
    }
    if turns.is_empty() {
        turns.push(new_turn(&entry.id, entry.timestamp, vec![]));
    }
    if let Some(turn) = turns.last_mut() {
        turn.items.extend(items);
        turn.ended_at = Some(entry.timestamp);
    }
}

fn input_content(parts: &[Content]) -> Vec<InputContent> {
    parts
        .iter()
        .filter_map(|part| match part {
            Content::Text { text, .. } => Some(InputContent::Text { text: text.clone() }),
            Content::Image { mime_type, data } => Some(InputContent::Image {
                mime_type: mime_type.clone(),
                data: data.clone(),
            }),
            _ => None,
        })
        .collect()
}

fn new_turn(id: &str, at: u64, items: Vec<Item>) -> Turn {
    Turn {
        id: TurnId(id.to_owned()),
        status: TurnStatus::Completed,
        items,
        started_at: at,
        ended_at: Some(at),
    }
}

pub fn upsert(turn: &mut Turn, item: Item) {
    let id = item_id(&item);
    match id.and_then(|id| {
        turn.items
            .iter()
            .position(|kept| item_id(kept).as_ref() == Some(&id))
    }) {
        Some(index) => turn.items[index] = item,
        None => turn.items.push(item),
    }
}

pub fn item_id(item: &Item) -> Option<ItemId> {
    match item {
        Item::Known(
            KnownItem::Compaction { id, .. }
            | KnownItem::UserMessage { id, .. }
            | KnownItem::AssistantMessage { id, .. }
            | KnownItem::Thinking { id, .. }
            | KnownItem::ToolCall { id, .. }
            | KnownItem::ToolResult { id, .. },
        ) => Some(id.clone()),
        Item::Custom { payload, .. } => payload
            .get("id")
            .and_then(Value::as_str)
            .map(|id| ItemId(id.into())),
    }
}

fn content_item(
    role: Role,
    content: &Content,
    id: ItemId,
    details: Option<&Value>,
) -> Option<Item> {
    let known = match (role, content) {
        (Role::Assistant, Content::Text { text, .. }) => KnownItem::AssistantMessage {
            id,
            text: text.clone(),
        },
        (Role::Assistant, Content::Thinking { text, .. }) => KnownItem::Thinking {
            id,
            text: text.clone(),
        },
        (Role::Assistant, Content::ToolCall(call)) => KnownItem::ToolCall {
            id: ItemId(call.id.clone()),
            name: call.name.clone(),
            arguments: call.arguments.clone(),
        },
        (Role::Tool, Content::ToolResult(result)) => KnownItem::ToolResult {
            id,
            call_id: ItemId(result.call_id.clone()),
            output: details.cloned().unwrap_or_else(
                || json!({ "content": result.content, "is_error": result.is_error }),
            ),
            is_error: result.is_error,
        },
        _ => return None,
    };
    Some(Item::Known(known))
}
