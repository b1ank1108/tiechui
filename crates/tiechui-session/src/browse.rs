//! Read-only views for choosing a session, as the resume picker reads them: summaries to
//! list and search, and the current branch as conversation parts. Files are opened
//! without the writer lock and never written.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tiechui_model::{Content, Message, Role};
use tokio_util::sync::CancellationToken;

use crate::storage::list_all;
use crate::{EntryKind, Header, Meta, SessionError, SessionStore, check_cancel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub path: PathBuf,
    pub header: Header,
    /// Unix milliseconds of the latest user or assistant message.
    pub modified: u64,
    pub first_message: String,
    /// Every user and assistant text, for search.
    pub text: String,
    pub meta: Meta,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    User(String),
    Assistant(String),
    /// A tool the assistant called.
    Call {
        name: String,
        arguments: Value,
    },
    /// What a tool returned.
    Output(String),
    /// A compaction's summary of the messages it replaced.
    Compaction(String),
    Notice(String),
}

/// Sessions with a conversation in `cwd`'s directory or, given none, in every directory.
/// Files that cannot be read are diagnostics; cancellation is an error.
pub fn summaries(
    home: &Path,
    cwd: Option<&Path>,
    cancel: &CancellationToken,
) -> Result<(Vec<Summary>, Vec<SessionError>), SessionError> {
    let list = match cwd {
        Some(cwd) => crate::list(home, cwd, cancel)?,
        None => list_all(home, cancel)?,
    };
    let mut summaries = Vec::new();
    let mut diagnostics = list.diagnostics;
    for session in list.sessions {
        check_cancel(&session.path, cancel)?;
        match summary(&session.path, cancel) {
            Ok(Some(summary)) => summaries.push(summary),
            Ok(None) => {}
            Err(error) => diagnostics.push(error),
        }
    }
    check_cancel(home, cancel)?;
    Ok((summaries, diagnostics))
}

fn summary(path: &Path, cancel: &CancellationToken) -> Result<Option<Summary>, SessionError> {
    let store = SessionStore::open_read_only(path, cancel)?;
    let mut modified = None;
    let mut first_message = None;
    let mut texts = Vec::new();
    for entry in store.entries() {
        if let EntryKind::Message { message, .. } = &entry.kind
            && matches!(message.role, Role::User | Role::Assistant)
        {
            modified = modified.max(Some(entry.timestamp));
            let text = text(message);
            if text.is_empty() {
                continue;
            }
            if message.role == Role::User && first_message.is_none() {
                first_message = Some(text.clone());
            }
            texts.push(text);
        }
    }
    // Sessions that were opened and left are not worth resuming.
    let Some(modified) = modified else {
        return Ok(None);
    };
    Ok(Some(Summary {
        path: store.path().to_owned(),
        header: store.header().clone(),
        modified,
        first_message: first_message.unwrap_or_default(),
        text: texts.join(" "),
        meta: crate::meta(path)?,
    }))
}

fn text(message: &Message) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The current branch from the root, including messages a compaction replaced and the
/// compaction's summary. Thinking and provider data are not conversation.
pub fn preview(path: &Path, cancel: &CancellationToken) -> Result<Vec<Part>, SessionError> {
    let store = SessionStore::open_read_only(path, cancel)?;
    let leaf = store.current_leaf().unwrap_or(&store.header().id);
    let mut parts = Vec::new();
    for entry in store.path_to_root(leaf)? {
        match &entry.kind {
            EntryKind::Message { message, .. } => {
                let part = match message.role {
                    Role::System => continue,
                    Role::User => Part::User,
                    Role::Assistant => Part::Assistant,
                    Role::Tool => Part::Output,
                };
                let text = contents(&message.content);
                if !text.is_empty() {
                    parts.push(part(text));
                }
                parts.extend(message.content.iter().filter_map(|content| match content {
                    Content::ToolCall(call) => Some(Part::Call {
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    }),
                    _ => None,
                }));
            }
            EntryKind::Compaction { summary, .. } if !summary.is_empty() => {
                parts.push(Part::Compaction(summary.clone()));
            }
            EntryKind::CustomMessage {
                hidden: false,
                text,
                ..
            } if !text.is_empty() => parts.push(Part::Notice(text.clone())),
            _ => {}
        }
    }
    Ok(parts)
}

fn contents(parts: &[Content]) -> String {
    parts
        .iter()
        .filter_map(|content| match content {
            Content::Text { text, .. } => Some(text.clone()),
            Content::Image { mime_type, .. } => Some(format!("[image: {mime_type}; data omitted]")),
            Content::ToolResult(result) => {
                Some(contents(&result.content)).filter(|text| !text.is_empty())
            }
            Content::ToolCall(_) | Content::Thinking { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}
