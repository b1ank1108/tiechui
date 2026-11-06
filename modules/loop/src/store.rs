//! `.agents/scheduled_tasks.json`: locked read-modify-write and the per-project owner lock.

use std::fs::{File, TryLockError};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tiechui_plugin_api::{CancellationToken, private, serde_json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    pub created_at: i64,
    #[serde(default)]
    pub last_fired_at: Option<i64>,
    pub recurring: bool,
}

#[derive(Serialize, Deserialize)]
struct Document {
    version: u32,
    tasks: Vec<Task>,
}

pub const FILE: &str = ".agents/scheduled_tasks.json";

#[derive(Debug, Clone)]
pub struct Store {
    directory: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self {
            directory: root.join(".agents"),
        }
    }

    fn file(&self) -> PathBuf {
        self.directory.join("scheduled_tasks.json")
    }

    /// Changes whenever the file is replaced; `None` when it is absent.
    pub fn stamp(&self) -> Option<(SystemTime, u64)> {
        let metadata = std::fs::metadata(self.file()).ok()?;
        Some((metadata.modified().ok()?, metadata.len()))
    }

    pub fn read(&self) -> Result<Vec<Task>, String> {
        let text = match std::fs::read_to_string(self.file()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(error) => return Err(self.error(error)),
        };
        let document: Document = serde_json::from_str(&text).map_err(|e| self.error(e))?;
        if document.version != 1 {
            return Err(self.error(format!("unsupported version {}", document.version)));
        }
        Ok(document.tasks)
    }

    /// Re-reads under the write lock; an unreadable file is never overwritten.
    pub fn update<T>(
        &self,
        cancel: &CancellationToken,
        change: impl FnOnce(&mut Vec<Task>) -> Result<T, String>,
    ) -> Result<(T, Vec<Task>), String> {
        std::fs::create_dir_all(&self.directory).map_err(|e| self.error(e))?;
        let lock = private::open_lock(&self.directory.join("scheduled_tasks.lock"))
            .map_err(|e| self.error(e))?;
        for attempt in 0.. {
            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }
            match lock.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) if attempt < 100 => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(error) => return Err(self.error(error)),
            }
        }
        let mut tasks = self.read()?;
        let result = change(&mut tasks)?;
        if tasks.is_empty() {
            match std::fs::remove_file(self.file()) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(self.error(error));
                }
                _ => return Ok((result, tasks)),
            }
        }
        let mut temporary =
            tempfile::NamedTempFile::new_in(&self.directory).map_err(|e| self.error(e))?;
        serde_json::to_writer_pretty(
            &mut temporary,
            &Document {
                version: 1,
                tasks: tasks.clone(),
            },
        )
        .map_err(|e| self.error(e))?;
        temporary.write_all(b"\n").map_err(|e| self.error(e))?;
        temporary.as_file().sync_all().map_err(|e| self.error(e))?;
        temporary.persist(self.file()).map_err(|e| self.error(e))?;
        Ok((result, tasks))
    }

    /// Takes the project's scheduling role when tasks exist and no other session holds it.
    pub fn claim(&self) -> Option<File> {
        self.file().exists().then_some(())?;
        let file = private::open_lock(&self.directory.join("scheduled_tasks.owner.lock")).ok()?;
        file.try_lock().ok()?;
        Some(file)
    }

    fn error(&self, error: impl ToString) -> String {
        format!("{}: {}", self.file().display(), error.to_string())
    }
}
