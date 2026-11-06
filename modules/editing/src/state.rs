use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sha2::{Digest, Sha256};
use tiechui_plugin_api::tools::ToolError;

use crate::edit::lf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Version {
    pub key: PathBuf,
    pub target: PathBuf,
    pub stamp: Vec<i128>,
}
/// File identity for change detection. Index 5 is the link count and 6..9 mode/uid/gid.
#[cfg(unix)]
pub(crate) fn stamp(m: &fs::Metadata) -> Vec<i128> {
    use std::os::unix::fs::MetadataExt;
    vec![
        m.dev() as i128,
        m.ino() as i128,
        m.size() as i128,
        m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128,
        m.ctime() as i128 * 1_000_000_000 + m.ctime_nsec() as i128,
        m.nlink() as i128,
        m.mode() as i128,
        m.uid() as i128,
        m.gid() as i128,
    ]
}
#[cfg(windows)]
pub(crate) fn stamp(m: &fs::Metadata) -> Vec<i128> {
    use std::os::windows::fs::MetadataExt;
    vec![
        0,
        0,
        m.file_size() as i128,
        m.last_write_time() as i128,
        m.creation_time() as i128,
        1,
        i128::from(m.permissions().readonly()),
        0,
        0,
    ]
}
pub(crate) fn version(path: &Path) -> Result<Version, ToolError> {
    let target = fs::canonicalize(path)?;
    let mut stamp = stamp(&fs::symlink_metadata(&target)?);
    stamp.truncate(5);
    Ok(Version {
        key: path.into(),
        target,
        stamp,
    })
}
fn logical(text: &str) -> String {
    lf(text.strip_prefix('\u{feff}').unwrap_or(text))
}
fn hash(text: &str) -> Vec<u8> {
    Sha256::digest(text.as_bytes()).to_vec()
}

struct View {
    text: String,
    call_id: Option<String>,
}
struct Observation {
    version: Version,
    views: Vec<View>,
    hash: Option<Vec<u8>>,
}
#[derive(Default)]
struct Inner {
    generation: u64,
    files: VecDeque<Observation>,
}

/// Session-local evidence. This is not a permission grant or a cross-process lock.
#[derive(Default)]
pub struct EditingState {
    inner: Mutex<Inner>,
}
impl EditingState {
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.generation += 1;
        inner.files.clear();
    }
    pub fn retain_visible_reads(&self, results: &BTreeMap<String, String>) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for observation in &mut inner.files {
            observation.views.retain(|view| {
                view.call_id.as_ref().is_none_or(|id| {
                    results
                        .get(id)
                        .is_some_and(|text| logical(text).contains(&view.text))
                })
            });
        }
    }
    pub(crate) fn generation(&self) -> u64 {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .generation
    }
    pub(crate) fn record(
        &self,
        version: Version,
        text: &str,
        generation: u64,
        full: bool,
        call_id: Option<String>,
        retain: bool,
    ) {
        let text = logical(text);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if generation != inner.generation {
            return;
        }
        let old = inner
            .files
            .iter()
            .position(|o| o.version.key == version.key)
            .and_then(|at| inner.files.remove(at));
        let mut views = old
            .filter(|o| o.version == version)
            .map(|o| o.views)
            .unwrap_or_default();
        if retain && !text.is_empty() && text.encode_utf16().count() <= 128_000 {
            views.retain(|v| v.text != text);
            views.push(View {
                text: text.clone(),
                call_id,
            });
        }
        while views
            .iter()
            .map(|v| v.text.encode_utf16().count())
            .sum::<usize>()
            > 128_000
        {
            views.remove(0);
        }
        inner.files.push_back(Observation {
            version,
            views,
            hash: full.then(|| hash(&text)),
        });
        while inner.files.len() > 128
            || inner
                .files
                .iter()
                .flat_map(|o| &o.views)
                .map(|v| v.text.encode_utf16().count())
                .sum::<usize>()
                > 1_000_000
        {
            inner.files.pop_front();
        }
    }
    pub(crate) fn check(&self, version: &Version, text: &str) -> Result<Vec<String>, ToolError> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(old) = inner.files.iter().find(|o| o.version.key == version.key) else {
            return Ok(Vec::new());
        };
        if old.version.target != version.target
            || old.hash.as_ref().map_or_else(
                || old.version.stamp != version.stamp,
                |h| *h != hash(&logical(text)),
            )
        {
            return Err(ToolError(format!(
                "File changed since it was read: {}. No changes were written. Read the current region and retry.",
                version.key.display()
            )));
        }
        Ok(old.views.iter().map(|v| v.text.clone()).collect())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn generations_and_visible_result_identity_bound_recovery_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a");
        fs::write(&path, "first\nsecond").unwrap();
        let state = EditingState::default();
        let version = version(&path).unwrap();
        let generation = state.generation();
        state.clear();
        state.record(
            version.clone(),
            "first\nsecond",
            generation,
            true,
            Some("old".into()),
            true,
        );
        assert!(state.check(&version, "different").unwrap().is_empty());
        let generation = state.generation();
        state.record(
            version.clone(),
            "first\nsecond",
            generation,
            true,
            Some("old".into()),
            true,
        );
        state.record(
            version.clone(),
            "first\nsecond",
            generation,
            true,
            Some("new".into()),
            true,
        );
        state.retain_visible_reads(&BTreeMap::from([("new".into(), "first\nsecond".into())]));
        assert_eq!(
            state.check(&version, "first\nsecond").unwrap(),
            ["first\nsecond"]
        );
        state.retain_visible_reads(&BTreeMap::new());
        assert!(state.check(&version, "first\nsecond").unwrap().is_empty());
        assert!(
            state
                .check(&version, "changed")
                .unwrap_err()
                .0
                .contains("changed since it was read")
        );
    }
}
