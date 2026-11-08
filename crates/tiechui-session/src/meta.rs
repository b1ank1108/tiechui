//! Client-owned session attributes, kept beside the session file so they can change while
//! the session's writer holds its lock and without adding entries to the tree.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::SessionError;

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
