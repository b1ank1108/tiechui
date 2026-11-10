//! `@` file mentions: one cancellable index per session, matched in memory.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use ignore::{WalkBuilder, WalkState};
use nucleo::{
    Config, Injector, Matcher, Nucleo,
    pattern::{CaseMatching, Normalization},
};
use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::completion::{self, Row};
use crate::theme::Theme;
use tiechui_protocol::{Resource, ResourceKind};
use tokio_util::sync::CancellationToken;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

const MAX_PATHS: usize = 100_000;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
pub const ROWS: usize = 8;

#[derive(Default)]
struct Scan {
    count: usize,
    bytes: usize,
    errors: usize,
    first_error: Option<String>,
    stopped: Option<String>,
    children: BTreeSet<String>,
}
impl Scan {
    fn error(&mut self, error: String) {
        self.errors += 1;
        self.first_error.get_or_insert(error);
    }
    fn warning(&self) -> Option<String> {
        let mut parts: Vec<_> = self.stopped.iter().cloned().collect();
        if let Some(error) = &self.first_error {
            parts.push(format!("{} discovery issue(s): {error}", self.errors));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// One streaming matcher per search root. Query changes reuse its injected paths.
pub struct Index {
    matcher: Nucleo<String>,
    highlighter: Matcher,
    scan: Arc<Mutex<Scan>>,
    loading: Arc<AtomicBool>,
    dirty: Arc<AtomicBool>,
    cancel: CancellationToken,
    running: bool,
    query: String,
    prefix: String,
    pub warning: Option<String>,
}
impl Default for Index {
    fn default() -> Self {
        let dirty = Arc::new(AtomicBool::new(true));
        let notify = dirty.clone();
        Self {
            matcher: Nucleo::new(
                Config::DEFAULT.match_paths(),
                Arc::new(move || notify.store(true, Ordering::Release)),
                Some(2),
                1,
            ),
            highlighter: Matcher::new(Config::DEFAULT.match_paths()),
            scan: Arc::default(),
            loading: Arc::default(),
            dirty,
            cancel: CancellationToken::new(),
            running: true,
            query: String::new(),
            prefix: String::new(),
            warning: None,
        }
    }
}
impl Drop for Index {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
impl Index {
    pub fn from_paths(paths: impl IntoIterator<Item = String>) -> Self {
        let index = Self::default();
        let injector = index.matcher.injector();
        for path in paths {
            if let Ok(mut scan) = index.scan.lock() {
                scan.count += 1;
                if !path.trim_end_matches('/').contains('/') {
                    scan.children.insert(path.clone());
                }
            }
            inject(&injector, path);
        }
        index
    }
    pub fn query(&mut self, query: &str, prefix: &str) {
        if query != self.query {
            self.matcher.pattern.reparse(
                0,
                query,
                CaseMatching::Ignore,
                Normalization::Smart,
                query.starts_with(&self.query),
            );
            self.query = query.into();
            self.running = true;
        }
        self.prefix = prefix.into();
        self.tick();
    }
    pub fn pending(&self) -> bool {
        self.running || self.loading.load(Ordering::Acquire) || self.dirty.load(Ordering::Acquire)
    }
    pub fn tick(&mut self) {
        self.dirty.store(false, Ordering::Release);
        self.running = self.matcher.tick(2).running;
        self.warning = self.scan.lock().ok().and_then(|scan| scan.warning());
    }
    pub fn rows(&mut self) -> Vec<Match> {
        if self.query.is_empty() {
            return self
                .scan
                .lock()
                .map(|scan| {
                    scan.children
                        .iter()
                        .take(ROWS)
                        .map(|path| Match {
                            path: format!("{}{path}", self.prefix),
                            resource: None,
                            indices: vec![],
                            score: 0,
                        })
                        .collect()
                })
                .unwrap_or_default();
        }
        let snapshot = self.matcher.snapshot();
        let pattern = snapshot.pattern().column_pattern(0);
        // The worker can finish the previous query while its replacement is running.
        if pattern.atoms != self.matcher.pattern.column_pattern(0).atoms {
            return Vec::new();
        }
        snapshot
            .matched_items(..snapshot.matched_item_count().min(ROWS as u32))
            .map(|item| {
                let mut hits = Vec::new();
                let score = pattern
                    .indices(
                        item.matcher_columns[0].slice(..),
                        &mut self.highlighter,
                        &mut hits,
                    )
                    .unwrap_or(0);
                let mut offset = self.prefix.chars().count();
                let mut indices = Vec::new();
                for (index, grapheme) in item.data.graphemes(true).enumerate() {
                    let length = grapheme.chars().count();
                    if hits.contains(&(index as u32)) {
                        indices.extend(offset..offset + length);
                    }
                    offset += length;
                }
                Match {
                    path: format!("{}{}", self.prefix, item.data),
                    resource: None,
                    indices,
                    score: -i64::from(score),
                }
            })
            .collect()
    }

    pub fn discovered(&self) -> usize {
        self.scan.lock().map_or(0, |s| s.count)
    }

    pub fn match_count(&self) -> usize {
        if self.query.is_empty() {
            self.scan.lock().map_or(0, |s| s.children.len())
        } else {
            self.matcher.snapshot().matched_item_count() as usize
        }
    }
}
fn inject(injector: &Injector<String>, path: String) {
    injector.push(path, |path, columns| columns[0] = path.as_str().into());
}

/// Include hidden entries, honor ignore files even outside Git, and never follow links.
pub async fn build(cwd: PathBuf, cancel: CancellationToken) -> Index {
    let mut index = Index::default();
    index.cancel = cancel.child_token();
    index.loading.store(true, Ordering::Release);
    let injector = index.matcher.injector();
    let scan = index.scan.clone();
    let loading = index.loading.clone();
    let dirty = index.dirty.clone();
    let cancel = index.cancel.clone();
    tokio::spawn(async move {
        let report = scan.clone();
        let result = tokio::task::spawn_blocking(move || {
            walk(cwd, injector, scan, cancel, Instant::now() + TIMEOUT)
        })
        .await;
        if let Err(error) = result
            && let Ok(mut scan) = report.lock()
        {
            scan.error(error.to_string());
        }
        loading.store(false, Ordering::Release);
        dirty.store(true, Ordering::Release);
    });
    index
}

fn walk(
    cwd: PathBuf,
    injector: Injector<String>,
    scan: Arc<Mutex<Scan>>,
    cancel: CancellationToken,
    deadline: Instant,
) {
    if cancel.is_cancelled() {
        return;
    }
    let filter_cancel = cancel.clone();
    WalkBuilder::new(&cwd).hidden(false).follow_links(false).require_git(false).threads(2)
        .filter_entry(move |entry| !filter_cancel.is_cancelled() && entry.file_name() != ".git")
        .build_parallel().run(|| {
            Box::new(|result| {
                if cancel.is_cancelled() { return WalkState::Quit; }
                let Ok(mut state) = scan.lock() else { return WalkState::Quit; };
                if state.stopped.is_some() { return WalkState::Quit; }
                if Instant::now() >= deadline {
                    state.stopped = Some("Filesystem scan timed out; narrow the directory.".into());
                    return WalkState::Quit;
                }
                let entry = match result {
                    Ok(entry) => entry,
                    Err(error) => { state.error(error.to_string()); return WalkState::Continue; }
                };
                if let Some(error) = entry.error() { state.error(error.to_string()); }
                if entry.depth() == 0 { return WalkState::Continue; }
                let Some(kind) = entry.file_type() else { return WalkState::Continue; };
                if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) { return WalkState::Continue; }
                let Ok(path) = entry.path().strip_prefix(&cwd) else { return WalkState::Continue; };
                let Some(name) = path.to_str().filter(|name| !name.chars().any(char::is_control)) else {
                    state.error(format!("Skipped unsupported filename: {path:?}"));
                    return if kind.is_dir() { WalkState::Skip } else { WalkState::Continue };
                };
                let path = if kind.is_dir() { format!("{name}/") } else { name.to_owned() };
                if state.count >= MAX_PATHS || state.bytes.saturating_add(path.len()) > MAX_BYTES {
                    state.stopped = Some(format!("Filesystem index limit reached ({MAX_PATHS} paths / 16 MiB); narrow the directory."));
                    return WalkState::Quit;
                }
                state.count += 1;
                state.bytes += path.len();
                if entry.depth() == 1 { state.children.insert(path.clone()); }
                drop(state);
                inject(&injector, path);
                WalkState::Continue
            })
        });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub path: String,
    pub resource: Option<Resource>,
    /// Character positions in `path` that matched.
    pub indices: Vec<usize>,
    score: i64,
}

fn fold(text: &str) -> Vec<char> {
    text.chars()
        .map(|c| {
            c.to_string()
                .nfkd()
                .find(|c| !is_combining_mark(*c))
                .unwrap_or('\0')
                .to_lowercase()
                .next()
                .unwrap_or(c)
        })
        .collect()
}

/// Greedy subsequence match; lower scores are better (early start, few gaps).
fn subsequence(needle: &[char], haystack: &[char], offset: usize) -> Option<(i64, Vec<usize>)> {
    let mut indices = Vec::with_capacity(needle.len());
    let mut from = 0;
    for c in needle {
        let at = from + haystack[from..].iter().position(|h| h == c)?;
        indices.push(offset + at);
        from = at + 1;
    }
    let first = indices.first().map_or(0, |i| i - offset);
    let gaps: usize = indices.windows(2).map(|w| w[1] - w[0] - 1).sum();
    Some((i64::try_from(first + gaps * 2).unwrap_or(i64::MAX), indices))
}

fn match_text(query: &str, haystack: &[char], offset: usize) -> Option<(i64, Vec<usize>)> {
    let mut score = 0;
    let mut indices = Vec::new();
    for word in query.split_whitespace() {
        let needle = fold(word)
            .into_iter()
            .filter(|c| *c != '\0')
            .collect::<Vec<_>>();
        let (value, positions) = subsequence(&needle, haystack, offset)?;
        score += value;
        indices.extend(positions);
    }
    indices.sort_unstable();
    indices.dedup();
    Some((score, indices))
}

/// A recoverable path reference; quoting preserves spaces and backslashes.
pub fn insertion(path: &str) -> String {
    if path
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\\'))
    {
        format!("@{}", serde_json::to_string(path).unwrap_or_default())
    } else {
        format!("@{path}")
    }
}

#[allow(clippy::too_many_arguments)]
pub fn popup(
    matches: &[Match],
    selected: usize,
    skills: bool,
    mode: Mode,
    status: &str,
    width: usize,
    height: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let rows = matches
        .iter()
        .map(|candidate| {
            if let Some(resource) = &candidate.resource {
                let skill = resource.kind == ResourceKind::Skill;
                let kind = match resource.kind {
                    ResourceKind::Skill => "Skill",
                    ResourceKind::Plugin => "Plugin",
                    ResourceKind::Task => "Task",
                };
                let source = match resource.source.as_deref() {
                    Some(source) if !skills => format!("{kind} · {source}"),
                    Some(source) => source.into(),
                    None => kind.into(),
                };
                let duplicate = skill
                    && matches.iter().any(|other| {
                        other.resource.as_ref().is_some_and(|other| {
                            other.kind == ResourceKind::Skill
                                && other.name == resource.name
                                && other.id != resource.id
                        })
                    });
                Row {
                    label: format!("{}{}", if skill { '$' } else { '@' }, resource.name),
                    description: if duplicate {
                        resource
                            .path
                            .as_ref()
                            .unwrap_or(&resource.description)
                            .clone()
                    } else {
                        resource.description.clone()
                    },
                    source: source + if resource.manual { " · manual" } else { "" },
                    indices: candidate.indices.iter().map(|i| i + 1).collect(),
                }
            } else {
                let trimmed = candidate.path.trim_end_matches('/');
                let base = trimmed.rfind('/').map_or(0, |i| i + 1);
                let offset = trimmed[..base].chars().count();
                Row {
                    label: candidate.path[base..].into(),
                    description: if base == 0 {
                        "./".into()
                    } else {
                        trimmed[..base].into()
                    },
                    source: if candidate.path.ends_with('/') {
                        "Dir"
                    } else {
                        "File"
                    }
                    .into(),
                    indices: candidate
                        .indices
                        .iter()
                        .filter_map(|i| i.checked_sub(offset))
                        .collect(),
                }
            }
        })
        .collect::<Vec<_>>();
    let title = if skills {
        vec![Span::styled("Skills", theme.dim)]
    } else {
        let (files, all) = if mode == Mode::All {
            (theme.dim, Style::default().add_modifier(Modifier::BOLD))
        } else {
            (Style::default().add_modifier(Modifier::BOLD), theme.dim)
        };
        vec![
            Span::styled("Files", files),
            Span::raw("  "),
            Span::styled("All", all),
        ]
    };
    completion::popup(title, &rows, selected, status, width, height, theme)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    All,
    #[default]
    Filesystem,
}
impl Mode {
    pub fn cycle(self) -> Self {
        match self {
            Self::All => Self::Filesystem,
            Self::Filesystem => Self::All,
        }
    }
}

pub fn catalog(resources: &[Resource], query: &str, mode: Mode, skills: bool) -> Vec<Match> {
    let mut found = resources
        .iter()
        .filter(|r| {
            if skills {
                r.kind == ResourceKind::Skill
            } else {
                mode == Mode::All
            }
        })
        .filter_map(|r| {
            let (score, indices) = match_text(query, &fold(&r.name), 0).or_else(|| {
                std::iter::once(&r.description)
                    .chain(&r.search_terms)
                    .find_map(|term| {
                        match_text(query, &fold(term), 0).map(|(s, _)| (s + 1000, vec![]))
                    })
            })?;
            Some(Match {
                path: r.name.clone(),
                resource: Some(r.clone()),
                score,
                indices,
            })
        })
        .collect::<Vec<_>>();
    let owners = found
        .iter()
        .filter_map(|m| m.resource.as_ref())
        .filter(|r| r.kind == ResourceKind::Plugin)
        .filter_map(|r| r.owner.as_ref())
        .cloned()
        .collect::<HashSet<_>>();
    found.retain(|m| {
        skills
            || m.resource.as_ref().is_none_or(|r| {
                r.kind != ResourceKind::Skill
                    || r.owner.as_ref().is_none_or(|o| !owners.contains(o))
            })
    });
    found.sort_by_key(|m| {
        (
            match m.resource.as_ref().map(|r| r.kind) {
                Some(ResourceKind::Plugin) => 0,
                Some(ResourceKind::Skill) => 1,
                _ => 2,
            },
            m.score,
            m.path.to_lowercase(),
        )
    });
    found
}

pub fn scope(
    cwd: &std::path::Path,
    query: &str,
    home: Option<&std::path::Path>,
) -> (PathBuf, String, String) {
    if let Some(slash) = query.rfind('/') {
        let prefix = &query[..=slash];
        let root = if let Some(rest) = prefix.strip_prefix("~/") {
            home.map(|p| p.join(rest))
        } else {
            Some(cwd.join(prefix))
        };
        if let Some(root) = root.filter(|p| p.is_dir()) {
            return (root, prefix.into(), query[slash + 1..].into());
        }
    }
    (cwd.into(), String::new(), query.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    async fn settle(index: &mut Index) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while index.pending() {
            assert!(Instant::now() < deadline, "index did not settle");
            index.tick();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    #[cfg(unix)]
    fn paths(index: &Index) -> Vec<String> {
        let mut paths: Vec<_> = index
            .matcher
            .snapshot()
            .matched_items(..)
            .map(|item| item.data.clone())
            .collect();
        paths.sort();
        paths
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn native_walk_respects_ignore_hidden_empty_directories_and_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("outside.txt"), "").unwrap();
        for name in ["empty", "中文 dir", ".github", ".git", "nested/.git"] {
            fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        fs::write(dir.path().join(".gitignore"), "ignored/\n*.skip\n").unwrap();
        fs::write(dir.path().join(".ignore"), "*.tmp\n").unwrap();
        fs::create_dir(dir.path().join("ignored")).unwrap();
        for name in [
            "中文 dir/Café.rs",
            ".github/config",
            "ignored/no",
            "no.skip",
            "no.tmp",
            ".git/no",
            "nested/.git/no",
        ] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        symlink(outside.path(), dir.path().join("linked")).unwrap();
        symlink(dir.path(), dir.path().join("loop")).unwrap();
        let mut index = build(dir.path().into(), CancellationToken::new()).await;
        settle(&mut index).await;
        let found = paths(&index);
        assert!(found.contains(&"empty/".into()));
        assert!(found.contains(&".github/config".into()));
        assert!(found.contains(&"中文 dir/Café.rs".into()));
        assert!(found.contains(&"linked".into()) && found.contains(&"loop".into()));
        assert!(!found.iter().any(|p| p.contains("/no")
            || p.starts_with("no.")
            || p.contains(".git/")
            || p.ends_with("outside.txt")));
        assert!(index.warning.is_none(), "{:?}", index.warning);
    }

    #[tokio::test]
    async fn queries_reuse_the_streaming_index_and_highlight_unicode() {
        let mut index = Index::from_paths(
            ["src/app.rs", "docs/中文.rs", "Cafe\u{301}.rs", "Café.rs"].map(str::to_owned),
        );
        index.query("cfe", "../");
        settle(&mut index).await;
        let rows = index.rows();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| {
            row.path.starts_with("../")
                && row
                    .indices
                    .iter()
                    .all(|i| *i >= 3 && *i < row.path.chars().count())
        }));
        let count = index.matcher.snapshot().item_count();
        let injector = index.matcher.injector();
        index.query("中文", "");
        settle(&mut index).await;
        assert_eq!(index.rows()[0].path, "docs/中文.rs");
        assert_eq!(index.matcher.snapshot().item_count(), count);
        inject(&injector, "中文-new.rs".into());
        index.tick();
        settle(&mut index).await;
        assert_eq!(index.rows().len(), 2);
        assert_eq!(index.matcher.snapshot().item_count(), count + 1);
        index.query("absent", "");
        assert!(index.rows().is_empty());
        settle(&mut index).await;
        assert!(index.rows().is_empty());
        assert_eq!(insertion("中文 dir/file.rs"), "@\"中文 dir/file.rs\"");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn discovery_reports_errors_and_cancels_without_external_programs() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        if cfg!(target_os = "linux") {
            fs::write(dir.path().join(OsString::from_vec(vec![0xff])), "").unwrap();
        }
        fs::write(dir.path().join("bad\nname"), "").unwrap();
        let mut index = build(dir.path().into(), CancellationToken::new()).await;
        settle(&mut index).await;
        assert!(
            index
                .warning
                .as_ref()
                .unwrap()
                .contains(if cfg!(target_os = "linux") {
                    "2 discovery issue"
                } else {
                    "1 discovery issue"
                })
        );
        assert!(paths(&index).is_empty());
        let mut missing = build(dir.path().join("missing"), CancellationToken::new()).await;
        settle(&mut missing).await;
        assert!(missing.warning.is_some());
        let cancel = CancellationToken::new();
        let mut cancelled = build(dir.path().into(), cancel.clone()).await;
        cancel.cancel();
        settle(&mut cancelled).await;
        assert!(paths(&cancelled).is_empty());
        let index = build(dir.path().into(), CancellationToken::new()).await;
        let worker_cancel = index.cancel.clone();
        drop(index);
        assert!(worker_cancel.is_cancelled());
    }

    #[test]
    fn walk_limits_and_deadlines_stop_with_visible_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), "").unwrap();
        for (count, bytes, timeout) in [(MAX_PATHS, 0, false), (0, MAX_BYTES, false), (0, 0, true)]
        {
            let index = Index::default();
            let scan = Arc::new(Mutex::new(Scan {
                count,
                bytes,
                ..Default::default()
            }));
            walk(
                dir.path().into(),
                index.matcher.injector(),
                scan.clone(),
                CancellationToken::new(),
                Instant::now() + if timeout { Duration::ZERO } else { TIMEOUT },
            );
            assert!(
                scan.lock()
                    .unwrap()
                    .warning()
                    .unwrap()
                    .contains(if timeout {
                        "timed out"
                    } else {
                        "limit reached"
                    })
            );
        }
    }
}
