//! `chui app-server` workspace services: files, Git, terminals and listening ports for desktop
//! clients. File paths are relative to the project `root` and use `/`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Names a project directory.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RootParams {
    pub root: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileParams {
    pub root: String,
    pub path: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileEntry {
    pub path: String,
    pub dir: bool,
    /// Excluded by `.gitignore`; an ignored directory's entries are listed separately.
    pub ignored: bool,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileTree {
    pub entries: Vec<FileEntry>,
    /// The listing stopped at its entry limit.
    pub truncated: bool,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileContent {
    /// None for binary files.
    pub text: Option<String>,
    /// SHA-256 of the bytes, for [`WriteParams::expected`].
    pub hash: String,
    pub size: u64,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WriteParams {
    pub root: String,
    pub path: String,
    pub text: String,
    /// Fails when the file's hash differs, or it exists while this is empty; None overwrites.
    pub expected: Option<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WriteResult {
    pub hash: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MoveParams {
    pub root: String,
    pub from: String,
    pub to: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct SearchParams {
    pub root: String,
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub whole_word: bool,
    /// Globs as in `.gitignore`; a pattern without `/` matches at any depth.
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchHit {
    /// One-based.
    pub line: u32,
    pub text: String,
    /// UTF-16 offsets into `text`.
    pub ranges: Vec<(u32, u32)>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchGroup {
    pub path: String,
    pub hits: Vec<SearchHit>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchResult {
    pub groups: Vec<SearchGroup>,
    pub total: u32,
    /// The search stopped at its hit limit.
    pub truncated: bool,
}

/// The `fs/changed` notification for a watched root.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FsChanged {
    pub root: String,
    pub paths: Vec<String>,
}

/// The `git/changed` notification: HEAD, the index or refs of a watched root changed.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitChanged {
    pub root: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum GitFileStatus {
    #[serde(rename = "M")]
    Modified,
    #[serde(rename = "A")]
    Added,
    #[serde(rename = "D")]
    Deleted,
    /// Untracked.
    #[serde(rename = "U")]
    Untracked,
    #[serde(rename = "R")]
    Renamed,
    #[serde(rename = "C")]
    Copied,
}

/// A file's staged or unstaged change; a partly staged file has one of each.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitChange {
    pub path: String,
    pub status: GitFileStatus,
    pub staged: bool,
    pub added: u32,
    pub removed: u32,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitOperation {
    Merge,
    Rebase,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitStatus {
    /// None when HEAD is detached.
    pub branch: Option<String>,
    /// None before the first commit.
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub operation: Option<GitOperation>,
    pub changes: Vec<GitChange>,
    /// Unmerged paths.
    pub conflicts: Vec<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommitFile {
    pub path: String,
    pub status: GitFileStatus,
    pub added: u32,
    pub removed: u32,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CompareParams {
    pub root: String,
    pub base: String,
}

/// HEAD against the merge base with `base`.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Comparison {
    pub merge_base: Option<String>,
    /// Commits on HEAD that `base` lacks.
    pub ahead: u32,
    /// Commits on `base` that HEAD lacks.
    pub behind: u32,
    pub files: Vec<CommitFile>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LogParams {
    pub root: String,
    pub limit: u32,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitCommit {
    pub hash: String,
    pub parents: Vec<String>,
    pub subject: String,
    pub author: String,
    /// Unix milliseconds.
    pub at: u64,
    /// Decorations as `git log` prints them: `HEAD -> main`, `origin/main`, `tag: v1`.
    pub refs: Vec<String>,
    pub files: Vec<CommitFile>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ShowParams {
    pub root: String,
    pub rev: String,
    pub path: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Branches {
    pub local: Vec<String>,
    pub remote: Vec<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PullRequestState {
    Open,
    Draft,
    Merged,
    Closed,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PullRequest {
    pub number: u32,
    pub state: PullRequestState,
    pub url: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GitOp {
    Stage {
        paths: Vec<String>,
    },
    Unstage {
        paths: Vec<String>,
    },
    /// Restores tracked paths and deletes untracked ones.
    Discard {
        paths: Vec<String>,
    },
    Commit {
        message: String,
    },
    Fetch,
    Pull {
        ff_only: bool,
    },
    Push {
        force: bool,
    },
    /// Pushes the current branch to `origin` and sets it as upstream.
    Publish,
    /// Pulls, then pushes.
    Sync,
    Rebase {
        onto: String,
    },
    /// Aborts the merge or rebase in progress.
    Abort,
    /// Continues the merge or rebase in progress.
    Continue,
    Checkout {
        branch: String,
        create: bool,
    },
    /// Opens a pull request for the current branch with `gh`.
    CreatePr,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitRunParams {
    pub root: String,
    pub op: GitOp,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct GitRunResult {
    /// Combined output of the commands.
    pub output: String,
}

/// Starts a shell in `cwd` under a client-chosen id.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TerminalCreateParams {
    pub id: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub env: Option<std::collections::BTreeMap<String, String>>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TerminalParams {
    pub id: String,
}

/// The output kept for a terminal, after which `terminal/output` continues.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TerminalAttached {
    /// The last 64 KiB.
    pub buffer: String,
    /// Output bytes so far; later `terminal/output` notifications have a greater `seq`.
    pub seq: u64,
    /// The shell has exited with this code.
    pub exit: Option<i32>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TerminalWriteParams {
    pub id: String,
    pub data: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TerminalResizeParams {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
}

/// The `terminal/output` notification.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TerminalOutput {
    pub id: String,
    /// Output bytes so far, including `data`.
    pub seq: u64,
    pub data: String,
}

/// The `terminal/exit` notification.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TerminalExit {
    pub id: String,
    pub code: i32,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PortsListParams {
    pub roots: Vec<String>,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct PortEntry {
    pub root: String,
    pub port: u16,
    pub pid: u32,
    pub process: String,
    pub address: String,
}

#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PortStopParams {
    pub root: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "typescript", ts(optional))]
    pub port: Option<u16>,
}

/// The `ports/changed` notification.
#[cfg_attr(feature = "typescript", derive(ts_rs::TS))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PortsChanged {
    pub ports: Vec<PortEntry>,
}
