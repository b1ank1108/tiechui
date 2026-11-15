//! Client-owned session attributes, kept beside the session file so they can change while
//! the session's writer holds its lock and without adding entries to the tree.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::SessionError;

static META_LOCK: Mutex<()> = Mutex::new(());

fn meta_guard() -> MutexGuard<'static, ()> {
    META_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Meta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Unix milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<u64>,
}

fn meta_path(session: &Path) -> PathBuf {
    session.with_extension("meta.json")
}

/// A missing file is the default; an unreadable one is an error.
pub fn meta(session: &Path) -> Result<Meta, SessionError> {
    let path = meta_path(session);
    match fs::read(&path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|error| SessionError::new(&path, error))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Meta::default()),
        Err(error) => Err(SessionError::new(&path, error)),
    }
}

/// Replaces the file atomically; the default removes it.
pub fn save_meta(session: &Path, meta: &Meta) -> Result<(), SessionError> {
    let path = meta_path(session);
    if *meta == Meta::default() {
        return match fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(SessionError::new(&path, error))
            }
            _ => Ok(()),
        };
    }
    let temp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(meta).map_err(|error| SessionError::new(&path, error))?;
    fs::write(&temp, bytes)
        .and_then(|()| fs::rename(&temp, &path))
        .map_err(|error| SessionError::new(&path, error))
}

/// Moves the session file and its attributes under `<home>/trash/`, keeping the
/// per-directory grouping. Returns the new session path.
pub fn trash(home: &Path, session: &Path) -> Result<PathBuf, SessionError> {
    let _guard = meta_guard();
    let (Some(name), Some(group)) = (
        session.file_name(),
        session.parent().and_then(Path::file_name),
    ) else {
        return Err(SessionError::new(session, "not a session file"));
    };
    let directory = home.join("trash").join(group);
    fs::create_dir_all(&directory).map_err(|error| SessionError::new(&directory, error))?;
    let target = directory.join(name);
    fs::rename(session, &target).map_err(|error| SessionError::new(session, error))?;
    let meta = meta_path(session);
    if meta.exists() {
        fs::rename(&meta, meta_path(&target)).map_err(|error| SessionError::new(&meta, error))?;
    }
    Ok(target)
}

/// Atomically updates session attributes under the in-process metadata lock.
pub fn update_meta(session: &Path, change: impl FnOnce(&mut Meta)) -> Result<(), SessionError> {
    let _guard = meta_guard();
    let mut current = meta(session)?;
    change(&mut current);
    save_meta(session, &current)
}

/// Sets `title` only when the session still exists on disk and has no title yet.
pub fn set_initial_title(session: &Path, title: &str) -> Result<bool, SessionError> {
    let _guard = meta_guard();
    if !session.exists() {
        return Ok(false);
    }
    let mut current = meta(session)?;
    if current.title.is_some() {
        return Ok(false);
    }
    current.title = Some(title.to_owned());
    save_meta(session, &current)?;
    Ok(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn set_initial_title_respects_existing_rename_and_missing_session() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.jsonl");
        assert!(!set_initial_title(&missing, "Ghost").unwrap());
        assert!(!meta_path(&missing).exists());

        let session = dir.path().join("s1.jsonl");
        fs::write(&session, "{}\n").unwrap();
        assert!(set_initial_title(&session, "Generated Title").unwrap());
        assert_eq!(
            meta(&session).unwrap().title.as_deref(),
            Some("Generated Title")
        );

        assert!(!set_initial_title(&session, "Second Attempt").unwrap());
        assert_eq!(
            meta(&session).unwrap().title.as_deref(),
            Some("Generated Title")
        );

        let renamed = dir.path().join("s2.jsonl");
        fs::write(&renamed, "{}\n").unwrap();
        update_meta(&renamed, |m| m.title = Some("User Custom".into())).unwrap();
        assert!(!set_initial_title(&renamed, "AI Title").unwrap());
        assert_eq!(
            meta(&renamed).unwrap().title.as_deref(),
            Some("User Custom")
        );
    }
}
