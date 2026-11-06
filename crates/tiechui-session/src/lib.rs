//! Append-only session trees. A writable store holds its file lock until dropped.

mod browse;
mod entry;
mod storage;

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};

use tiechui_model::{Content, Message, Role, StopReason};
use tokio_util::sync::CancellationToken;

pub use browse::{Part, PartKind, Summary, preview, summaries};
pub use entry::{Entry, EntryKind, FORMAT_VERSION, Header, ModelRef};
pub use storage::{SessionInfo, SessionList, list, resolve_cwd, session_dir};

#[derive(Debug)]
pub struct SessionError {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub message: String,
}

impl SessionError {
    fn new(path: &Path, message: impl fmt::Display) -> Self {
        Self {
            path: path.to_owned(),
            line: None,
            message: message.to_string(),
        }
    }

    fn at_line(path: &Path, line: usize, message: impl fmt::Display) -> Self {
        Self {
            path: path.to_owned(),
            line: Some(line),
            message: message.to_string(),
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path.display())?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
        }
        write!(f, ": {}", self.message)
    }
}

impl std::error::Error for SessionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: usize,
    pub message: String,
}

#[derive(Debug)]
pub struct SessionStore {
    path: PathBuf,
    file: File,
    writable: bool,
    header: Header,
    entries: Vec<Entry>,
    // Retain original lines for forks, including unknown fields and JSON spelling.
    lines: Vec<Vec<u8>>,
    by_id: HashMap<String, usize>,
    leaf: Option<String>,
    diagnostics: Vec<Diagnostic>,
    file_len: u64,
    valid_len: u64,
    needs_newline: bool,
    write_failed: bool,
}

impl SessionStore {
    /// Create under `<home>/sessions/<encoded-cwd>/`. Callers resolve TIECHUI_HOME.
    pub fn create(
        home: &Path,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<Self, SessionError> {
        Self::create_in(&session_dir(home, cwd)?, cwd, None, cancel)
    }

    /// Acquire the exclusive writer lock before loading. No repair occurs until append.
    pub fn open(path: &Path, cancel: &CancellationToken) -> Result<Self, SessionError> {
        Self::load(path, true, cancel)
    }

    /// Load a snapshot without locking or modifying the source, including a torn tail.
    pub fn open_read_only(path: &Path, cancel: &CancellationToken) -> Result<Self, SessionError> {
        Self::load(path, false, cancel)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn header(&self) -> &Header {
        &self.header
    }
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Rebuild from the locked file without releasing or reacquiring writer ownership.
    pub fn reload(&mut self, cancel: &CancellationToken) -> Result<(), SessionError> {
        use std::io::{Seek, SeekFrom};
        let mut file = self
            .file
            .try_clone()
            .map_err(|error| SessionError::new(&self.path, error))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|error| SessionError::new(&self.path, error))?;
        let snapshot = Self::load_file(self.path.clone(), file, self.writable, cancel)?;
        *self = snapshot;
        Ok(())
    }

    /// None denotes the header, before the first entry.
    pub fn current_leaf(&self) -> Option<&str> {
        self.leaf.as_deref()
    }

    /// Navigation is in memory. The next append persists the selected parent.
    pub fn branch_to(&mut self, entry_id: &str) -> Result<(), SessionError> {
        if entry_id == self.header.id {
            self.leaf = None;
        } else if self.by_id.contains_key(entry_id) {
            self.leaf = Some(entry_id.to_owned());
        } else {
            return Err(SessionError::new(
                &self.path,
                format!("unknown entry {entry_id}"),
            ));
        }
        Ok(())
    }

    /// Return root-to-entry order. The header is implicit; its path is empty.
    pub fn path_to_root(&self, entry_id: &str) -> Result<Vec<&Entry>, SessionError> {
        if entry_id == self.header.id {
            return Ok(Vec::new());
        }
        let mut path = Vec::new();
        let mut next = Some(entry_id);
        while let Some(id) = next {
            let index = self
                .by_id
                .get(id)
                .ok_or_else(|| SessionError::new(&self.path, format!("unknown entry {id}")))?;
            let entry = &self.entries[*index];
            path.push(entry);
            next = entry.parent_id.as_deref();
        }
        path.reverse();
        Ok(path)
    }

    /// Neutral messages on the active branch, cut at the latest compaction.
    pub fn build_context(&self) -> Vec<Message> {
        self.build_context_excluding(&HashSet::new())
    }

    /// Omit audit-only message entries without rewriting the append-only history.
    pub fn build_context_excluding(&self, excluded: &HashSet<String>) -> Vec<Message> {
        self.build_context_with_origins(excluded)
            .into_iter()
            .map(|(message, _, _)| message)
            .collect()
    }

    /// `build_context_excluding`, with the model each message came from, when recorded.
    pub fn build_context_with_origins(
        &self,
        excluded: &HashSet<String>,
    ) -> Vec<(Message, Option<ModelRef>, Option<StopReason>)> {
        self.project(excluded)
            .into_iter()
            .flat_map(|(entry, messages)| {
                let (model, reason) = match &entry.kind {
                    EntryKind::Message {
                        model, stop_reason, ..
                    } => (model.clone(), *stop_reason),
                    _ => (None, None),
                };
                messages
                    .into_iter()
                    .map(move |message| (message, model.clone(), reason))
            })
            .collect()
    }

    /// The same context projection, retaining source entries and invisible metadata.
    pub fn project(&self, excluded: &HashSet<String>) -> Vec<(&Entry, Vec<Message>)> {
        let mut path = Vec::new();
        let mut next = self.leaf.as_ref();
        while let Some(id) = next {
            let entry = &self.entries[self.by_id[id]];
            path.push(entry);
            next = entry.parent_id.as_ref();
        }
        path.reverse();
        let visible = |entry: &Entry, skip_system: bool| {
            entry
                .kind
                .context_message()
                .filter(|(message, _, _)| {
                    !(excluded.contains(&entry.id) || skip_system && message.role == Role::System)
                })
                .map(|(message, _, _)| message)
                .into_iter()
                .collect()
        };
        let mut projected = Vec::new();
        let start = if let Some(index) = path
            .iter()
            .rposition(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
        {
            let EntryKind::Compaction {
                summary,
                first_kept_entry_id,
                system_message,
                details,
                ..
            } = &path[index].kind
            else {
                unreachable!()
            };
            let mut messages: Vec<_> = system_message.iter().cloned().collect();
            messages.push(Message {
                role: Role::User,
                content: vec![Content::Text {
                    text: format!("The conversation history before this point was compacted into the following summary:\n\n<summary>\n{summary}\n</summary>"),
                    opaque: None,
                }],
                opaque: None,
            });
            let restored: Vec<_> = details["restored"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| item["text"].as_str())
                .collect();
            if !restored.is_empty() {
                messages.push(Message {
                    role: Role::User,
                    content: vec![Content::Text {
                        text: format!(
                            "Context restored after the summary, as of the checkpoint:\n\n{}",
                            restored.join("\n\n")
                        ),
                        opaque: None,
                    }],
                    opaque: None,
                });
            }
            projected.push((path[index], messages));
            if let Some(keep) = path[..index]
                .iter()
                .position(|entry| Some(entry.id.as_str()) == first_kept_entry_id.as_deref())
            {
                projected.extend(
                    path[keep..index]
                        .iter()
                        .map(|entry| (*entry, visible(entry, true))),
                );
            }
            index + 1
        } else {
            0
        };
        projected.extend(
            path[start..]
                .iter()
                .map(|entry| (*entry, visible(entry, false))),
        );
        projected
    }

    /// Copy exactly the selected path into a new file in the same session directory.
    /// Entry IDs and raw lines survive; the new header records the source path.
    pub fn fork(&self, entry_id: &str, cancel: &CancellationToken) -> Result<Self, SessionError> {
        check_cancel(&self.path, cancel)?;
        let path = self.path_to_root(entry_id)?;
        let directory = self
            .path
            .parent()
            .ok_or_else(|| SessionError::new(&self.path, "missing session directory"))?;
        let mut fork =
            Self::create_in(directory, &self.header.cwd, Some(self.path.clone()), cancel)?;
        let result = (|| {
            for entry in path {
                check_cancel(&self.path, cancel)?;
                let line = &self.lines[self.by_id[&entry.id]];
                fork.write_entry(entry.clone(), line, cancel)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let path = fork.path.clone();
            drop(fork);
            std::fs::remove_file(&path).map_err(|cleanup| {
                SessionError::new(
                    &path,
                    format!("{error}; failed to remove incomplete fork: {cleanup}"),
                )
            })?;
            return Err(error);
        }
        Ok(fork)
    }

    pub fn append(
        &mut self,
        mut kind: EntryKind,
        cancel: &CancellationToken,
    ) -> Result<&Entry, SessionError> {
        check_cancel(&self.path, cancel)?;
        if let EntryKind::Compaction { system_message, .. } = &mut kind {
            *system_message = self
                .build_context()
                .into_iter()
                .rev()
                .find(|message| message.role == Role::System);
        }
        let entry = Entry {
            id: storage::new_id(),
            parent_id: self.leaf.clone(),
            timestamp: storage::now_ms(),
            kind,
        };
        self.validate_entry(&entry)
            .map_err(|message| SessionError::new(&self.path, message))?;
        let mut line =
            serde_json::to_vec(&entry).map_err(|error| SessionError::new(&self.path, error))?;
        line.push(b'\n');
        let index = self.entries.len();
        self.write_entry(entry, &line, cancel)?;
        Ok(&self.entries[index])
    }

    fn validate_entry(&self, entry: &Entry) -> Result<(), &'static str> {
        if entry.id.is_empty() || entry.id == self.header.id || self.by_id.contains_key(&entry.id) {
            return Err("empty or duplicate entry ID");
        }
        if entry
            .parent_id
            .as_ref()
            .is_some_and(|id| !self.by_id.contains_key(id))
        {
            return Err("parent must name an earlier entry");
        }
        if let EntryKind::Message {
            message,
            model: None,
            ..
        } = &entry.kind
            && message.role == Role::Assistant
        {
            return Err("assistant message requires an originating model");
        }
        if let EntryKind::Unknown { kind, fields } = &entry.kind {
            if matches!(
                kind.as_str(),
                "message"
                    | "model_change"
                    | "thinking_change"
                    | "usage"
                    | "compaction"
                    | "label"
                    | "custom"
                    | "custom_message"
            ) {
                return Err("unknown entry must not use a known kind");
            }
            if fields
                .keys()
                .any(|key| matches!(key.as_str(), "id" | "parent_id" | "timestamp" | "kind"))
            {
                return Err("unknown fields must not replace entry metadata");
            }
        }
        if let EntryKind::Compaction {
            system_message: Some(message),
            ..
        } = &entry.kind
            && message.role != Role::System
        {
            return Err("compaction checkpoint must be a system message");
        }
        Ok(())
    }

    fn remember(&mut self, entry: Entry, line: Vec<u8>) {
        self.leaf = Some(entry.id.clone());
        self.by_id.insert(entry.id.clone(), self.entries.len());
        self.entries.push(entry);
        self.lines.push(line);
    }
}

fn check_cancel(path: &Path, cancel: &CancellationToken) -> Result<(), SessionError> {
    if cancel.is_cancelled() {
        Err(SessionError::new(path, "operation cancelled"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
