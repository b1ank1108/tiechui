use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use tiechui_plugin_api::{CancellationToken, ToolError, model::*};

#[derive(Clone, Debug)]
pub struct Part {
    pub id: String,
    pub owner: usize,
    pub slots: Vec<usize>,
    pub role: Role,
    pub text: String,
    pub thinking: u64,
    pub tool: String,
    pub call: String,
    pub result: bool,
    pub opaque: bool,
}
impl Part {
    pub fn is_reasoning(&self) -> bool {
        self.id.ends_with("/thinking")
    }
    pub fn is_act(&self) -> bool {
        self.role == Role::Assistant
            && (self.id.ends_with("/text") || (!self.call.is_empty() && !self.result))
    }
    pub fn tokens(&self) -> u64 {
        tokens(&self.text) + self.thinking
    }
    /// Maintenance reads do not advance the recent window that protects business work.
    pub fn recovery(&self) -> bool {
        maintenance(&self.tool) && !crate::engine::carrier(&self.tool)
    }
}
#[derive(Clone)]
pub struct Projection {
    pub originals: Vec<Message>,
    pub parts: Vec<Part>,
}

/// Current and legacy history-maintenance tools, including a legacy search alias.
pub fn maintenance(tool: &str) -> bool {
    matches!(
        tool,
        "context_status"
            | "compact_history"
            | "search_history"
            | "read_history"
            | "bc_status"
            | "compress"
            | "decompress"
            | "search_context"
    )
}
pub fn check(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError("Operation aborted".into()))
    } else {
        Ok(())
    }
}
pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn tokens(text: &str) -> u64 {
    let mut cjk = 0;
    let mut other = 0;
    for c in text.chars() {
        if matches!(c, '\u{4e00}'..='\u{9fff}' | '\u{3040}'..='\u{30ff}' | '\u{ac00}'..='\u{d7af}')
        {
            cjk += 1;
        } else {
            other += c.len_utf16() as u64;
        }
    }
    cjk + other.div_ceil(4)
}
pub fn text(content: &[Content]) -> String {
    content
        .iter()
        .filter_map(|p| match p {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}
pub fn plain(text: impl Into<String>) -> Content {
    Content::Text {
        text: text.into(),
        opaque: None,
    }
}
pub fn project(messages: &[Message], cancel: &CancellationToken) -> Result<Projection, ToolError> {
    let mut seen = BTreeMap::<String, usize>::new();
    let calls: BTreeMap<_, _> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|p| match p {
            Content::ToolCall(c) => Some((c.id.clone(), c.name.clone())),
            _ => None,
        })
        .collect();
    let mut parts = vec![];
    for (owner, message) in messages.iter().enumerate() {
        check(cancel)?;
        let digest = hash(&serde_json::to_vec(message).map_err(|e| ToolError(e.to_string()))?);
        let count = seen.entry(digest.clone()).or_default();
        let id = format!("msg-{digest}-{count}");
        *count += 1;
        let opaque = message.role == Role::System
            || (message.role != Role::Assistant && message.opaque.is_some())
            || message.content.iter().any(|p| match message.role {
                Role::Assistant => !matches!(
                    p,
                    Content::Text { .. } | Content::Thinking { .. } | Content::ToolCall(_)
                ),
                Role::Tool => !matches!(p, Content::ToolResult(_)),
                _ => !matches!(p, Content::Text { opaque: None, .. }),
            });
        let base = Part {
            id,
            owner,
            slots: vec![],
            role: message.role,
            text: String::new(),
            thinking: 0,
            tool: String::new(),
            call: String::new(),
            result: false,
            opaque,
        };
        if message.role == Role::Assistant {
            let thinking: Vec<_> = message
                .content
                .iter()
                .enumerate()
                .filter_map(|(i, p)| matches!(p, Content::Thinking { .. }).then_some(i))
                .collect();
            if !thinking.is_empty() {
                let size = thinking
                    .iter()
                    .map(|i| match &message.content[*i] {
                        Content::Thinking { text, .. } => tokens(text),
                        _ => 0,
                    })
                    .sum();
                parts.push(Part {
                    id: format!("{}/thinking", base.id),
                    slots: thinking,
                    thinking: size,
                    ..base.clone()
                });
            }
            let slots: Vec<_> = message
                .content
                .iter()
                .enumerate()
                .filter_map(|(i, p)| matches!(p, Content::Text { .. }).then_some(i))
                .collect();
            if !slots.is_empty() || message.content.is_empty() {
                parts.push(Part {
                    id: format!("{}/text", base.id),
                    slots,
                    text: text(&message.content),
                    ..base.clone()
                });
            }
            for (i, part) in message.content.iter().enumerate() {
                if let Content::ToolCall(call) = part {
                    parts.push(Part {
                        id: format!("{}/call-{i}", base.id),
                        slots: vec![i],
                        text: call.arguments.to_string(),
                        call: call.id.clone(),
                        tool: call.name.clone(),
                        ..base.clone()
                    });
                }
            }
            if !parts.iter().any(|p| p.owner == owner) {
                parts.push(base);
            }
        } else {
            let result = message.content.iter().find_map(|p| {
                if let Content::ToolResult(r) = p {
                    Some(r)
                } else {
                    None
                }
            });
            let mut part = base;
            part.slots = (0..message.content.len()).collect();
            part.text = result.map_or_else(|| text(&message.content), |r| text(&r.content));
            if let Some(result) = result {
                part.result = true;
                part.call = result.call_id.clone();
                part.tool = calls.get(&result.call_id).cloned().unwrap_or_default();
                part.opaque |= result
                    .content
                    .iter()
                    .any(|p| !matches!(p, Content::Text { opaque: None, .. }));
            }
            parts.push(part);
        }
    }
    Ok(Projection {
        originals: messages.to_vec(),
        parts,
    })
}

pub fn signed(message: &Message) -> bool {
    message.opaque.is_some()
        || message.content.iter().any(|p| match p {
            Content::Text { opaque, .. } => opaque.is_some(),
            Content::ToolCall(c) => c.opaque.is_some(),
            _ => false,
        })
}

/// Withdraw incomplete pairs and reasoning runs to a fixed point after protection carving.
pub fn integrity(parts: &[Part], folded: &mut BTreeSet<String>) {
    let mut groups = vec![];
    let mut cursor = 0;
    while cursor < parts.len() {
        let start = cursor;
        while cursor < parts.len() && parts[cursor].is_reasoning() {
            cursor += 1;
        }
        let burst = cursor;
        while cursor < parts.len() && parts[cursor].is_act() {
            cursor += 1;
        }
        if cursor > burst {
            let mut group: Vec<_> = (start..cursor).collect();
            for part in &parts[burst..cursor] {
                if !part.call.is_empty()
                    && let Some(index) = parts.iter().position(|p| p.result && p.call == part.call)
                {
                    group.push(index);
                }
            }
            groups.push(group);
        }
        if cursor == start {
            cursor += 1;
        }
    }
    loop {
        let before = folded.len();
        for group in &groups {
            let reasoning_folds = group
                .iter()
                .any(|i| parts[*i].is_reasoning() && folded.contains(&parts[*i].id));
            let call_stays = group.iter().any(|i| {
                !parts[*i].call.is_empty() && !parts[*i].result && !folded.contains(&parts[*i].id)
            });
            if reasoning_folds && call_stays {
                for i in group {
                    folded.remove(&parts[*i].id);
                }
            }
        }
        for part in parts.iter().filter(|p| !p.call.is_empty() && !p.result) {
            if let Some(result) = parts.iter().find(|r| r.result && r.call == part.call)
                && folded.contains(&part.id) != folded.contains(&result.id)
            {
                folded.remove(&part.id);
                folded.remove(&result.id);
            }
        }
        if before == folded.len() {
            break;
        }
    }
}
