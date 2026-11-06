//! Profile-wide submitted input, appended so concurrent processes never replace each other.

use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// Only the newest part of the file is read at startup.
const TAIL_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Default)]
pub struct InputHistory {
    path: Option<PathBuf>,
    items: Vec<String>,
    warned: bool,
}

impl InputHistory {
    /// Loads `path`, skipping corrupt rows; the error is reported once and history stays in memory.
    pub fn load(path: PathBuf) -> (Self, Option<String>) {
        let mut history = Self {
            path: Some(path),
            ..Self::default()
        };
        let warning = history.read().err().and_then(|error| history.fail(&error));
        (history, warning)
    }

    fn read(&mut self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let start = file.metadata()?.len().saturating_sub(TAIL_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let mut rows = bytes.split(|b| *b == b'\n');
        if start > 0 {
            // The first row may be cut by the tail window.
            rows.next();
        }
        for row in rows {
            let Ok(record) = serde_json::from_slice::<Value>(row) else {
                continue;
            };
            if record["schemaVersion"] == 1
                && let Some(text) = record["text"].as_str()
            {
                self.add(text);
            }
        }
        Ok(())
    }

    pub fn entries(&self) -> &[String] {
        &self.items
    }

    fn add(&mut self, text: &str) {
        if !text.trim().is_empty() && self.items.last().is_none_or(|last| last != text) {
            self.items.push(text.to_owned());
        }
    }

    fn fail(&mut self, error: &std::io::Error) -> Option<String> {
        (!std::mem::replace(&mut self.warned, true))
            .then(|| format!("input history unavailable: {error}"))
    }

    /// Appends one record; returns a warning the first time writing fails.
    pub fn remember(&mut self, text: &str, session_id: &str) -> Option<String> {
        if text.trim().is_empty() {
            return None;
        }
        self.add(text);
        self.write(text, session_id)
            .err()
            .and_then(|error| self.fail(&error))
    }

    fn write(&self, text: &str, session_id: &str) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let mut options = OpenOptions::new();
        options.append(true).create(true);
        if let Some(parent) = path.parent() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
                builder.mode(0o700);
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            builder.create(parent)?;
        }
        let mut file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let record = json!({
            "schemaVersion": 1,
            "sessionId": session_id,
            "timestamp": timestamp,
            "text": text,
        });
        // The leading newline keeps an interrupted tail from swallowing this row.
        file.write_all(format!("\n{record}\n").as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_append_skip_corrupt_rows_and_merge_adjacent_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/input-history.jsonl");
        let (mut history, warning) = InputHistory::load(path.clone());
        assert!(warning.is_none());
        assert!(history.remember("first", "s").is_none());
        history.remember("second", "s");
        history.remember("second", "s");
        history.remember("  ", "s");
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"schemaVersion\":1,\"text\":\"cut")
            .unwrap();
        let (reloaded, _) = InputHistory::load(path.clone());
        assert_eq!(reloaded.entries(), ["first", "second"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_failures_warn_once_and_keep_memory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o500)).unwrap();
        let (mut history, warning) = InputHistory::load(state.join("input-history.jsonl"));
        assert!(warning.is_none());
        assert!(history.remember("a", "s").is_some());
        assert!(history.remember("b", "s").is_none());
        assert_eq!(history.entries(), ["a", "b"]);
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
    }
}
