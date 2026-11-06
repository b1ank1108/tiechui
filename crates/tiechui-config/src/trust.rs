use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::ConfigError;

type Decisions = BTreeMap<PathBuf, Option<bool>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustEntry {
    pub path: PathBuf,
    pub decision: bool,
}

/// Path-keyed decisions. A missing/null entry inherits the nearest ancestor.
pub struct TrustStore {
    path: PathBuf,
}

impl TrustStore {
    pub fn new(home: &Path) -> Self {
        Self {
            path: home.join("trust.json"),
        }
    }

    pub fn get(&self, cwd: &Path, cancel: &CancellationToken) -> Result<Option<bool>, ConfigError> {
        Ok(self.get_entry(cwd, cancel)?.map(|entry| entry.decision))
    }

    pub fn get_entry(
        &self,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<Option<TrustEntry>, ConfigError> {
        let _lock = self.lock(cancel)?;
        let data = self.read()?;
        let cwd = normalize(cwd)?;
        for path in cwd.ancestors() {
            if let Some(Some(decision)) = data.get(path) {
                return Ok(Some(TrustEntry {
                    path: path.to_owned(),
                    decision: *decision,
                }));
            }
        }
        Ok(None)
    }

    pub fn set(
        &self,
        cwd: &Path,
        decision: Option<bool>,
        cancel: &CancellationToken,
    ) -> Result<(), ConfigError> {
        self.set_many(&[(cwd.to_owned(), decision)], cancel)
    }

    /// `None` removes an explicit decision so the path inherits its nearest ancestor's.
    pub fn set_many(
        &self,
        updates: &[(PathBuf, Option<bool>)],
        cancel: &CancellationToken,
    ) -> Result<(), ConfigError> {
        let _lock = self.lock(cancel)?;
        let mut data = self.read()?;
        for (path, decision) in updates {
            self.check(cancel)?;
            let path = normalize(path)?;
            if decision.is_some() {
                data.insert(path, *decision);
            } else {
                data.remove(&path);
            }
        }
        let parent = self.path.parent().unwrap_or(Path::new("."));
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|e| self.error(e))?;
        serde_json::to_writer_pretty(&mut temporary, &data).map_err(|e| self.error(e))?;
        temporary.write_all(b"\n").map_err(|e| self.error(e))?;
        temporary.as_file().sync_all().map_err(|e| self.error(e))?;
        self.check(cancel)?;
        temporary.persist(&self.path).map_err(|e| self.error(e))?;
        Ok(())
    }

    fn read(&self) -> Result<Decisions, ConfigError> {
        match fs::read_to_string(&self.path) {
            Ok(text) => {
                serde_json::from_str(text.trim_start_matches('\u{feff}')).map_err(|e| self.error(e))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(error) => Err(self.error(error)),
        }
    }

    fn lock(&self, cancel: &CancellationToken) -> Result<File, ConfigError> {
        self.check(cancel)?;
        let parent = self.path.parent().unwrap_or(Path::new("."));
        crate::private::create_dir_all(parent).map_err(|e| self.error(e))?;
        // Lock a separate inode so atomic replacement cannot release the lock early.
        let file = crate::private::open_lock(&self.path.with_extension("json.lock"))
            .map_err(|e| self.error(e))?;
        for attempt in 0..10 {
            self.check(cancel)?;
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) if attempt < 9 => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                Err(error) => return Err(self.error(error)),
            }
        }
        Err(self.error("trust store is locked"))
    }

    fn check(&self, cancel: &CancellationToken) -> Result<(), ConfigError> {
        if cancel.is_cancelled() {
            Err(self.error("Operation aborted"))
        } else {
            Ok(())
        }
    }

    fn error(&self, error: impl ToString) -> ConfigError {
        ConfigError::new(&self.path, "<trust>", error.to_string())
    }
}

fn normalize(path: &Path) -> Result<PathBuf, ConfigError> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|e| ConfigError::new(path, "<directory>", e.to_string()))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(part),
        }
    }
    Ok(normalized.canonicalize().unwrap_or(normalized))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_inheritance_child_override_and_removal() {
        let root = tempfile::tempdir().unwrap();
        let parent = root.path().join("parent");
        let child = parent.join("project");
        fs::create_dir_all(&child).unwrap();
        let store = TrustStore::new(&root.path().join("home"));
        let cancel = CancellationToken::new();
        assert_eq!(store.get(&child, &cancel).unwrap(), None);
        store.set(&parent, Some(true), &cancel).unwrap();
        assert_eq!(store.get(&child, &cancel).unwrap(), Some(true));
        store.set(&child, Some(false), &cancel).unwrap();
        assert_eq!(store.get(&child, &cancel).unwrap(), Some(false));
        store.set(&child, None, &cancel).unwrap();
        assert_eq!(store.get(&child, &cancel).unwrap(), Some(true));
        store.set(&parent, Some(false), &cancel).unwrap();
        assert_eq!(store.get(&child, &cancel).unwrap(), Some(false));
        store.set(&child, Some(true), &cancel).unwrap();
        assert_eq!(
            store
                .get(
                    &child.canonicalize().unwrap().join("missing/deeper"),
                    &cancel
                )
                .unwrap(),
            Some(true)
        );
        assert_eq!(
            store
                .get(&root.path().join("parent-sibling"), &cancel)
                .unwrap(),
            None
        );
        store
            .set_many(
                &[(parent.clone(), Some(true)), (child.clone(), None)],
                &cancel,
            )
            .unwrap();
        assert_eq!(
            store.get_entry(&child, &cancel).unwrap().unwrap().path,
            parent.canonicalize().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn canonical_paths_share_decisions_and_null_inherits() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        let alias = root.path().join("alias");
        fs::create_dir_all(real.join("child")).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let store = TrustStore::new(root.path());
        let cancel = CancellationToken::new();
        store.set(&alias, Some(true), &cancel).unwrap();
        assert_eq!(store.get(&real, &cancel).unwrap(), Some(true));
        let child = real.join("child").canonicalize().unwrap();
        let data = serde_json::json!({real.canonicalize().unwrap().to_str().unwrap(): true, child.to_str().unwrap(): null});
        fs::write(&store.path, format!("\u{feff}{data}")).unwrap();
        assert_eq!(
            store.get(&alias.join("child"), &cancel).unwrap(),
            Some(true)
        );
    }

    #[test]
    fn malformed_stores_fail_and_cancellation_does_not_write() {
        let root = tempfile::tempdir().unwrap();
        let store = TrustStore::new(root.path());
        let cancel = CancellationToken::new();
        for text in ["[]", "null", "{\"/project\":42}", "{"] {
            fs::write(&store.path, text).unwrap();
            assert!(store.get(root.path(), &cancel).is_err());
            assert!(store.set(root.path(), Some(true), &cancel).is_err());
            assert_eq!(fs::read_to_string(&store.path).unwrap(), text);
        }
        cancel.cancel();
        assert!(store.set(root.path(), Some(true), &cancel).is_err());
    }
}
