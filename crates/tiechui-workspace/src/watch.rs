use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::json;
use tiechui_protocol::workspace::{FsChanged, GitChanged};

use crate::{Result, Sink};

const SETTLE: Duration = Duration::from_millis(150);
/// The longest wait for changes to settle.
const BATCH: Duration = Duration::from_secs(1);

/// Git state that the status, branch and history views show.
fn tracked_git_state(path: &Path) -> bool {
    let name = path.to_string_lossy();
    [
        "HEAD",
        "ORIG_HEAD",
        "MERGE_HEAD",
        "FETCH_HEAD",
        "index",
        "packed-refs",
    ]
    .contains(&name.as_ref())
        || ["refs", "rebase-merge", "rebase-apply"]
            .iter()
            .any(|dir| path.starts_with(dir))
}

pub(crate) struct Watch {
    _watcher: RecommendedWatcher,
}

impl Watch {
    pub(crate) fn start(root: &Path, sink: Sink) -> Result<Self> {
        let canonical = root.canonicalize()?;
        let git_dir = std::process::Command::new("git")
            .current_dir(root)
            .args(["rev-parse", "--absolute-git-dir"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|dir| PathBuf::from(dir.trim()).canonicalize().ok());
        let mut ignores = GitignoreBuilder::new(&canonical);
        ignores.add(canonical.join(".gitignore"));
        let ignores = ignores.build().unwrap_or_else(|_| Gitignore::empty());
        let (events, received) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(events)?;
        watcher.watch(&canonical, RecursiveMode::Recursive)?;
        if let Some(dir) = git_dir.as_ref().filter(|dir| !dir.starts_with(&canonical)) {
            watcher.watch(dir, RecursiveMode::Recursive)?;
        }
        let name = root.display().to_string();
        std::thread::spawn(move || {
            // Ends when the watcher drops its sender.
            while let Ok(first) = received.recv() {
                let mut batch = vec![first];
                let deadline = Instant::now() + BATCH;
                while Instant::now() < deadline
                    && let Ok(event) = received.recv_timeout(SETTLE)
                {
                    batch.push(event);
                }
                let mut paths = BTreeSet::new();
                let mut git = false;
                for path in batch.into_iter().flatten().flat_map(|event| event.paths) {
                    if let Some(inner) =
                        git_dir.as_ref().and_then(|dir| path.strip_prefix(dir).ok())
                    {
                        git |= tracked_git_state(inner);
                    } else if let Ok(relative) = path.strip_prefix(&canonical)
                        && !relative.as_os_str().is_empty()
                        && !ignores
                            .matched_path_or_any_parents(relative, path.is_dir())
                            .is_ignore()
                    {
                        paths.insert(relative.to_string_lossy().replace('\\', "/"));
                    }
                }
                if !paths.is_empty() {
                    let paths = paths.into_iter().collect();
                    sink(
                        "fs/changed",
                        json!(FsChanged {
                            root: name.clone(),
                            paths
                        }),
                    );
                }
                if git {
                    sink("git/changed", json!(GitChanged { root: name.clone() }));
                }
            }
        });
        Ok(Self { _watcher: watcher })
    }
}
