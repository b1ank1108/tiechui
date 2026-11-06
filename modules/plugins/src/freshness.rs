use std::{
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};
use tiechui_plugin_api::{CancellationToken, ToolError};

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    path: PathBuf,
    kind: fs::FileType,
    size: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, u32),
    target: Option<PathBuf>,
}

fn fingerprint(root: &Path, cancel: &CancellationToken) -> Result<Vec<Entry>, ToolError> {
    fn walk(
        root: &Path,
        relative: &Path,
        cancel: &CancellationToken,
        entries: &mut Vec<Entry>,
    ) -> Result<(), ToolError> {
        crate::check(cancel)?;
        let path = root.join(relative);
        let metadata = fs::symlink_metadata(&path)?;
        let kind = metadata.file_type();
        entries.push(Entry {
            path: relative.to_owned(),
            kind,
            size: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino(), metadata.mode())
            },
            target: if kind.is_symlink() {
                Some(fs::read_link(&path)?)
            } else {
                None
            },
        });
        if kind.is_dir() {
            for child in fs::read_dir(path)? {
                crate::check(cancel)?;
                walk(root, &relative.join(child?.file_name()), cancel, entries)?;
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    walk(root, Path::new(""), cancel, &mut entries)?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

#[derive(Default)]
pub(super) struct Freshness {
    snapshot: Option<(Vec<Entry>, String)>,
    #[cfg(test)]
    pub hashes: usize,
}

impl Freshness {
    pub fn hash(
        &mut self,
        root: &Path,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<String, ToolError> {
        let metadata = fingerprint(root, cancel)?;
        if !force
            && let Some((previous, hash)) = &self.snapshot
            && *previous == metadata
        {
            return Ok(hash.clone());
        }
        #[cfg(test)]
        {
            self.hashes += 1;
        }
        let hash = crate::content_hash(root, cancel)?;
        // Never cache a hash alongside metadata from a different version of the tree.
        if metadata != fingerprint(root, cancel)? {
            return Err(ToolError("Plugin package changed while hashing".into()));
        }
        self.snapshot = Some((metadata, hash.clone()));
        Ok(hash)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn large_unchanged_package_never_rehashes_per_call() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-tests");
        fs::create_dir_all(&base).unwrap();
        let root = tempfile::tempdir_in(base).unwrap();
        let file = fs::File::create(root.path().join("binary")).unwrap();
        file.set_len(64 * 1024 * 1024).unwrap();
        let cancel = CancellationToken::new();
        let mut cache = Freshness::default();
        let hash = cache.hash(root.path(), true, &cancel).unwrap();
        assert_eq!(cache.hash(root.path(), true, &cancel).unwrap(), hash);
        assert_eq!(cache.hashes, 2, "authorization and start both hash");
        for _ in 0..10 {
            assert_eq!(cache.hash(root.path(), false, &cancel).unwrap(), hash);
        }
        assert_eq!(cache.hashes, 2, "per-call checks must not read contents");
        let modified = file.metadata().unwrap().modified().unwrap();
        file.set_modified(modified + std::time::Duration::from_nanos(1_000_000))
            .unwrap();
        assert_eq!(cache.hash(root.path(), false, &cancel).unwrap(), hash);
        assert_eq!(cache.hashes, 3, "metadata changes force a full hash");
        assert_eq!(cache.hash(root.path(), false, &cancel).unwrap(), hash);
        assert_eq!(cache.hashes, 3, "unchanged contents refresh the cache");
    }
}
