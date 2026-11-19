use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tiechui_protocol::workspace::{
    Branches, CommitFile, Comparison, GitChange, GitCommit, GitFileStatus, GitOp, GitOperation,
    GitStatus, PullRequest, PullRequestState,
};

use crate::{Error, Result, fail, fs};

pub(crate) fn command(
    program: &str,
    root: &Path,
    args: &[&str],
    env: &BTreeMap<String, String>,
) -> Command {
    let mut command = Command::new(resolve_program(program, env));
    command
        .current_dir(root)
        .args(args)
        .stdin(Stdio::null())
        .envs(env)
        // Nothing can answer a prompt or an editor.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GIT_EDITOR", "true")
        .env("GIT_OPTIONAL_LOCKS", "0");
    if program == "git"
        && find_env_var(env, "GIT_CONFIG_COUNT").is_none()
        && std::env::var_os("GIT_CONFIG_COUNT").is_none()
    {
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "credential.interactive")
            .env("GIT_CONFIG_VALUE_0", "false");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    for name in tiechui_config::scrubbed_secret_env_vars() {
        command.env_remove(name);
    }
    command
}

pub(crate) fn find_env_var<'a>(env: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    env.get(name).map(String::as_str).or_else(|| {
        env.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    })
}

fn resolve_program(program: &str, env: &BTreeMap<String, String>) -> PathBuf {
    resolve_program_for_platform(program, env, cfg!(windows))
}

fn resolve_program_for_platform(
    program: &str,
    env: &BTreeMap<String, String>,
    windows: bool,
) -> PathBuf {
    let is_bare = !program.is_empty()
        && !program.contains('/')
        && (!windows || (!program.contains('\\') && !program.contains(':')))
        && Path::new(program).components().count() == 1;
    let Some(path_var) = find_env_var(env, "PATH").filter(|_| is_bare) else {
        return program.into();
    };
    split_path_entries(path_var, windows)
        .into_iter()
        .flat_map(|dir| program_candidates(&dir, program, windows))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| program.into())
}

fn split_path_entries(path_var: &str, windows: bool) -> Vec<PathBuf> {
    if windows && !cfg!(windows) {
        return path_var
            .split(';')
            .map(|entry| entry.trim().trim_matches('"'))
            .filter(|entry| !entry.is_empty())
            .map(PathBuf::from)
            .collect();
    }
    std::env::split_paths(path_var).collect()
}

fn program_candidates(dir: &Path, program: &str, windows: bool) -> Vec<PathBuf> {
    if windows {
        vec![
            dir.join(format!("{program}.exe")),
            dir.join(format!("{program}.cmd")),
            dir.join(format!("{program}.bat")),
            dir.join(program),
        ]
    } else {
        vec![dir.join(program)]
    }
}

/// Stdout and stderr of a successful run.
fn output(mut command: Command) -> Result<(String, String)> {
    decode_output(command.output()?)
}

fn decode_output(output: std::process::Output) -> Result<(String, String)> {
    decode_status_output(output.status, &output.stdout, &output.stderr)
}

pub(crate) fn decode_status_output(
    status: std::process::ExitStatus,
    stdout_bytes: &[u8],
    stderr_bytes: &[u8],
) -> Result<(String, String)> {
    let stdout = String::from_utf8_lossy(stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(stderr_bytes).into_owned();
    if !status.success() {
        let message = if stderr.trim().is_empty() {
            &stdout
        } else {
            &stderr
        };
        return fail(message.trim());
    }
    Ok((stdout, stderr))
}

pub(crate) fn git(root: &Path, args: &[&str], env: &BTreeMap<String, String>) -> Result<String> {
    Ok(output(command("git", root, args, env))?.0)
}

pub(crate) fn git_with_stdin(
    root: &Path,
    args: &[&str],
    stdin_bytes: &[u8],
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let mut cmd = command("git", root, args, env);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let child_stdin = child.stdin.take();
    let out = std::thread::scope(|scope| {
        if let Some(mut pipe) = child_stdin {
            scope.spawn(move || {
                use std::io::Write;
                let _ = pipe.write_all(stdin_bytes);
            });
        }
        child.wait_with_output()
    })?;
    Ok(decode_output(out)?.0)
}

fn git_with(
    root: &Path,
    args: &[&str],
    paths: &[String],
    env: &BTreeMap<String, String>,
) -> Result<String> {
    let mut command = command("git", root, args, env);
    command.arg("--").args(paths);
    Ok(output(command)?.0)
}

fn count(text: &str) -> u32 {
    text.trim().parse().unwrap_or(0)
}

fn file_status(code: char) -> Option<GitFileStatus> {
    Some(match code {
        'M' | 'T' => GitFileStatus::Modified,
        'A' => GitFileStatus::Added,
        'D' => GitFileStatus::Deleted,
        'R' => GitFileStatus::Renamed,
        'C' => GitFileStatus::Copied,
        _ => return None,
    })
}

/// Line counts by path from `git diff --numstat -z`; binary files count zero.
fn numstat(text: &str) -> HashMap<String, (u32, u32)> {
    let mut stats = HashMap::new();
    let mut fields = text.split('\0');
    while let Some(record) = fields.next() {
        let mut parts = record.splitn(3, '\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        // A rename has an empty path, then the old and new paths.
        let path = if path.is_empty() {
            fields.next();
            fields.next().unwrap_or_default()
        } else {
            path
        };
        stats.insert(path.to_owned(), (count(added), count(removed)));
    }
    stats
}

/// Lines of an untracked file, as `git diff --no-index` would count them.
fn lines(root: &Path, path: &str) -> u32 {
    std::fs::read(root.join(path))
        .ok()
        .and_then(fs::text)
        .map_or(0, |text| text.lines().count() as u32)
}

struct Repo {
    /// The root's path inside the repository, ending in `/` unless empty.
    prefix: String,
    operation: Option<GitOperation>,
}

/// None outside a Git work tree.
fn repo(root: &Path, env: &BTreeMap<String, String>) -> Result<Option<Repo>> {
    let Ok(text) = git(
        root,
        &[
            "rev-parse",
            "--show-prefix",
            "--git-path",
            "MERGE_HEAD",
            "--git-path",
            "rebase-merge",
            "--git-path",
            "rebase-apply",
        ],
        env,
    ) else {
        return Ok(None);
    };
    let lines: Vec<_> = text.lines().collect();
    let [prefix, merge, rebase, apply] = lines[..] else {
        return fail("Unexpected git rev-parse output");
    };
    let exists = |path: &str| root.join(path).exists();
    let operation = if exists(merge) {
        Some(GitOperation::Merge)
    } else if exists(rebase) || exists(apply) {
        Some(GitOperation::Rebase)
    } else {
        None
    };
    Ok(Some(Repo {
        prefix: prefix.to_owned(),
        operation,
    }))
}

/// None outside a Git work tree.
pub fn status_with_env(root: &Path, env: &BTreeMap<String, String>) -> Result<Option<GitStatus>> {
    let Some(repo) = repo(root, env)? else {
        return Ok(None);
    };
    let text = git(
        root,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
            "--",
            ".",
        ],
        env,
    )?;
    let index = numstat(&git(
        root,
        &["diff", "--cached", "--numstat", "-z", "--relative"],
        env,
    )?);
    let worktree = numstat(&git(root, &["diff", "--numstat", "-z", "--relative"], env)?);
    let mut status = GitStatus {
        branch: None,
        head: None,
        upstream: None,
        ahead: 0,
        behind: 0,
        operation: repo.operation,
        changes: vec![],
        conflicts: vec![],
    };
    let local = |path: &str| path.strip_prefix(&repo.prefix).unwrap_or(path).to_owned();
    let mut records = text.split('\0');
    while let Some(record) = records.next() {
        let fields: Vec<_> = record.split(' ').collect();
        match fields[..] {
            ["#", "branch.oid", oid] => status.head = (oid != "(initial)").then(|| oid.into()),
            ["#", "branch.head", head] => {
                status.branch = (head != "(detached)").then(|| head.into())
            }
            ["#", "branch.upstream", upstream] => status.upstream = Some(upstream.into()),
            ["#", "branch.ab", ahead, behind] => {
                status.ahead = count(ahead.trim_start_matches('+'));
                status.behind = count(behind.trim_start_matches('-'));
            }
            ["?", ..] => {
                let path = local(&record[2..]);
                status.changes.push(GitChange {
                    added: lines(root, &path),
                    path,
                    status: GitFileStatus::Untracked,
                    staged: false,
                    removed: 0,
                });
            }
            ["u", ..] => status.conflicts.push(local(&fields[10..].join(" "))),
            [kind @ ("1" | "2"), xy, ..] => {
                let skip = if kind == "1" { 8 } else { 9 };
                let path = local(&fields[skip..].join(" "));
                if kind == "2" {
                    records.next();
                }
                let mut codes = xy.chars();
                for (code, staged, stats) in [
                    (codes.next(), true, &index),
                    (codes.next(), false, &worktree),
                ] {
                    if let Some(kind) = code.and_then(file_status) {
                        let (added, removed) = stats.get(&path).copied().unwrap_or_default();
                        status.changes.push(GitChange {
                            path: path.clone(),
                            status: kind,
                            staged,
                            added,
                            removed,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    status.changes.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Some(status))
}

/// Files from `--name-status -z` with counts from `--numstat -z`.
fn files(name_status: &str, stats: &HashMap<String, (u32, u32)>) -> Vec<CommitFile> {
    let mut files = Vec::new();
    let mut fields = name_status.split('\0').filter(|field| !field.is_empty());
    while let Some(code) = fields.next() {
        let Some(path) = fields.next() else {
            break;
        };
        let kind = code
            .chars()
            .next()
            .and_then(file_status)
            .unwrap_or(GitFileStatus::Modified);
        let path = if matches!(kind, GitFileStatus::Renamed | GitFileStatus::Copied) {
            fields.next().unwrap_or(path)
        } else {
            path
        };
        let (added, removed) = stats.get(path).copied().unwrap_or_default();
        files.push(CommitFile {
            path: path.to_owned(),
            status: kind,
            added,
            removed,
        });
    }
    files
}

pub fn compare_with_env(
    root: &Path,
    base: &str,
    env: &BTreeMap<String, String>,
) -> Result<Comparison> {
    let Ok(merge_base) = git(root, &["merge-base", "HEAD", base], env) else {
        return Ok(Comparison {
            merge_base: None,
            ahead: 0,
            behind: 0,
            files: vec![],
        });
    };
    let merge_base = merge_base.trim().to_owned();
    let range = format!("HEAD...{base}");
    let counts = git(root, &["rev-list", "--left-right", "--count", &range], env)?;
    let (ahead, behind) = counts.split_once('\t').unwrap_or_default();
    let diff = |format: &str| {
        git(
            root,
            &["diff", format, "-z", "--relative", &merge_base, "HEAD"],
            env,
        )
    };
    let files = files(&diff("--name-status")?, &numstat(&diff("--numstat")?));
    Ok(Comparison {
        merge_base: Some(merge_base),
        ahead: count(ahead),
        behind: count(behind),
        files,
    })
}

pub fn log_with_env(
    root: &Path,
    limit: u32,
    env: &BTreeMap<String, String>,
) -> Result<Vec<GitCommit>> {
    if git(root, &["rev-parse", "--verify", "-q", "HEAD"], env).is_err() {
        return Ok(vec![]);
    }
    let limit = format!("-n{limit}");
    let text = git(
        root,
        &[
            "log",
            &limit,
            "--relative",
            "--no-renames",
            "--raw",
            "--numstat",
            "-z",
            "--format=%x1e%H%x1f%P%x1f%s%x1f%an%x1f%ct%x1f%D%x1f",
        ],
        env,
    )?;
    let mut commits = Vec::new();
    for record in text.split('\x1e').filter(|record| !record.is_empty()) {
        let fields: Vec<_> = record.splitn(7, '\x1f').collect();
        let [hash, parents, subject, author, at, refs, body] = fields[..] else {
            continue;
        };
        // `--raw` records are `:<modes> <hashes> <status>\0<path>\0`, then numstat ones.
        let mut statuses = String::new();
        let mut stats = String::new();
        let mut parts = body.trim_start_matches('\n').split('\0');
        while let Some(part) = parts.next() {
            let part = part.trim_start_matches('\n');
            if let Some(raw) = part.strip_prefix(':') {
                let code = raw.rsplit(' ').next().unwrap_or("M");
                statuses.push_str(&format!("{code}\0{}\0", parts.next().unwrap_or_default()));
            } else if !part.is_empty() {
                stats.push_str(part);
                stats.push('\0');
            }
        }
        commits.push(GitCommit {
            hash: hash.into(),
            parents: parents.split_whitespace().map(str::to_owned).collect(),
            subject: subject.into(),
            author: author.into(),
            at: at.trim().parse::<u64>().unwrap_or(0) * 1000,
            refs: refs
                .split(", ")
                .filter(|r| !r.is_empty())
                .map(str::to_owned)
                .collect(),
            files: files(&statuses, &numstat(&stats)),
        });
    }
    Ok(commits)
}

/// The file's text at `rev`; None when it is absent there or binary.
pub fn show_with_env(
    root: &Path,
    rev: &str,
    path: &str,
    env: &BTreeMap<String, String>,
) -> Result<Option<String>> {
    fs::resolve(root, path)?;
    let mut command = command("git", root, &["show", &format!("{rev}:./{path}")], env);
    let output = command.output()?;
    Ok(output
        .status
        .success()
        .then(|| fs::text(output.stdout))
        .flatten())
}

pub fn branches_with_env(root: &Path, env: &BTreeMap<String, String>) -> Result<Branches> {
    let text = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
        env,
    )?;
    let mut branches = Branches {
        local: vec![],
        remote: vec![],
    };
    for name in text.lines() {
        if let Some(local) = name.strip_prefix("refs/heads/") {
            branches.local.push(local.into());
        } else if let Some(remote) = name.strip_prefix("refs/remotes/")
            && !remote.ends_with("/HEAD")
        {
            branches.remote.push(remote.into());
        }
    }
    Ok(branches)
}

/// The current branch's pull request, when `gh` knows one.
pub fn pull_request_with_env(
    root: &Path,
    env: &BTreeMap<String, String>,
) -> Result<Option<PullRequest>> {
    let Ok((text, _)) = output(command(
        "gh",
        root,
        &["pr", "view", "--json", "number,state,isDraft,url"],
        env,
    )) else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(&text)?;
    let state = match (value["state"].as_str(), value["isDraft"].as_bool()) {
        (Some("OPEN"), Some(true)) => PullRequestState::Draft,
        (Some("OPEN"), _) => PullRequestState::Open,
        (Some("MERGED"), _) => PullRequestState::Merged,
        _ => PullRequestState::Closed,
    };
    Ok(Some(PullRequest {
        number: value["number"].as_u64().unwrap_or(0) as u32,
        state,
        url: value["url"].as_str().unwrap_or_default().into(),
    }))
}

pub fn run_with_env(root: &Path, op: &GitOp, env: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::new();
    let mut step = |command: Command| -> Result<()> {
        let (stdout, stderr) = output(command)?;
        for text in [stdout, stderr] {
            out.push_str(&text);
        }
        Ok(())
    };
    let git = |args: &[&str]| command("git", root, args, env);
    let operation = || -> Result<GitOperation> {
        repo(root, env)?
            .and_then(|repo| repo.operation)
            .ok_or_else(|| Error("No merge or rebase is in progress".into()))
    };
    match op {
        GitOp::Stage { paths } => {
            git_with(root, &["add", "-A"], paths, env)?;
        }
        GitOp::Unstage { paths } => {
            if git(&["rev-parse", "--verify", "-q", "HEAD"])
                .output()?
                .status
                .success()
            {
                git_with(root, &["restore", "--staged"], paths, env)?;
            } else {
                git_with(root, &["rm", "--cached", "-r", "-q"], paths, env)?;
            }
        }
        GitOp::Discard { paths } => {
            // Unstaged, new files are untracked: delete those, then restore the rest.
            if git(&["rev-parse", "--verify", "-q", "HEAD"])
                .output()?
                .status
                .success()
            {
                git_with(root, &["reset", "-q"], paths, env)?;
            }
            git_with(root, &["clean", "-f", "-q"], paths, env)?;
            let tracked: Vec<String> = git_with(root, &["ls-files", "-z"], paths, env)?
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect();
            if !tracked.is_empty() {
                git_with(root, &["checkout", "-q"], &tracked, env)?;
            }
        }
        GitOp::Commit { message } => step(git(&["commit", "-q", "-m", message]))?,
        GitOp::Fetch => step(git(&["fetch"]))?,
        GitOp::Pull { ff_only } => step(git(&[
            "pull",
            if *ff_only { "--ff-only" } else { "--no-rebase" },
        ]))?,
        GitOp::Push { force: true } => step(git(&["push", "--force-with-lease"]))?,
        GitOp::Push { force: false } => step(git(&["push"]))?,
        GitOp::Publish => step(git(&["push", "-u", "origin", "HEAD"]))?,
        GitOp::Sync => {
            step(git(&["pull", "--no-rebase"]))?;
            step(git(&["push"]))?;
        }
        GitOp::Rebase { onto } => step(git(&["rebase", onto]))?,
        GitOp::Abort => match operation()? {
            GitOperation::Merge => step(git(&["merge", "--abort"]))?,
            GitOperation::Rebase => step(git(&["rebase", "--abort"]))?,
        },
        GitOp::Continue => match operation()? {
            GitOperation::Merge => step(git(&["commit", "--no-edit"]))?,
            GitOperation::Rebase => step(git(&["rebase", "--continue"]))?,
        },
        GitOp::Checkout {
            branch,
            create: true,
        } => step(git(&["switch", "-c", branch]))?,
        GitOp::Checkout {
            branch,
            create: false,
        } => step(git(&["switch", branch]))?,
        GitOp::CreatePr => step(command("gh", root, &["pr", "create", "--fill"], env))?,
    }
    Ok(out.trim().to_owned())
}

const UNTRACKED_FILE_CAP: usize = 64 << 10;
const DIFF_COLLECT_CAP: usize = 128 << 10;

/// Diff for commit-message generation: staged changes when present, otherwise all uncommitted changes.
pub fn commit_diff_with_env(root: &Path, env: &BTreeMap<String, String>) -> Result<String> {
    if repo(root, env)?.is_none() {
        return fail("Not a Git repository");
    }
    let staged = git(
        root,
        &[
            "diff",
            "--cached",
            "--relative",
            "--no-ext-diff",
            "--no-color",
        ],
        env,
    )?;
    if !staged.trim().is_empty() {
        return Ok(staged);
    }
    let mut combined = git(
        root,
        &["diff", "--relative", "--no-ext-diff", "--no-color"],
        env,
    )?;
    append_untracked_diffs(root, env, &mut combined)?;
    if combined.trim().is_empty() {
        return fail("No uncommitted changes");
    }
    Ok(combined)
}

fn append_untracked_diffs(
    root: &Path,
    env: &BTreeMap<String, String>,
    out: &mut String,
) -> Result<()> {
    let listing = git(
        root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ],
        env,
    )?;
    for rel in listing.split('\0').filter(|path| !path.is_empty()) {
        if out.len() >= DIFF_COLLECT_CAP {
            out.push_str(&format!(
                "diff --git a/{rel} b/{rel}\nnew file mode 100644\n"
            ));
        } else {
            out.push_str(&format_untracked_file(root, rel));
        }
    }
    Ok(())
}

fn format_untracked_file(root: &Path, rel: &str) -> String {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(root.join(rel)) else {
        return String::new();
    };
    let mut bytes = Vec::new();
    if file
        .take((UNTRACKED_FILE_CAP + 1) as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return String::new();
    }
    let truncated = bytes.len() > UNTRACKED_FILE_CAP;
    if truncated {
        bytes.truncate(UNTRACKED_FILE_CAP);
    }
    let Some(text) = utf8_prefix(&bytes, truncated) else {
        return format!(
            "diff --git a/{rel} b/{rel}\nnew file mode 100644\nBinary files /dev/null and b/{rel} differ\n"
        );
    };
    render_new_file_patch(rel, text)
}

fn utf8_prefix(bytes: &[u8], truncated: bool) -> Option<&str> {
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(err) if truncated && err.error_len().is_none() => {
            std::str::from_utf8(&bytes[..err.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

fn render_new_file_patch(rel: &str, text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut patch = format!(
        "diff --git a/{rel} b/{rel}\nnew file mode 100644\n--- /dev/null\n+++ b/{rel}\n@@ -0,0 +1,{} @@\n",
        lines.len()
    );
    for line in lines {
        patch.push('+');
        patch.push_str(line);
        patch.push('\n');
    }
    patch
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn git_cmd(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn commit_diff_rejects_non_git_and_clean_worktree() {
        let non_git = tempfile::tempdir().unwrap();
        let err = commit_diff_with_env(non_git.path(), &BTreeMap::new())
            .unwrap_err()
            .0;
        assert!(err.contains("Not a Git repository"), "{err}");

        let repo_dir = tempfile::tempdir().unwrap();
        git_cmd(repo_dir.path(), &["init", "-q"]);
        let unborn_err = commit_diff_with_env(repo_dir.path(), &BTreeMap::new())
            .unwrap_err()
            .0;
        assert!(
            unborn_err.contains("No uncommitted changes"),
            "{unborn_err}"
        );

        std::fs::write(repo_dir.path().join("README.md"), "# demo\n").unwrap();
        git_cmd(repo_dir.path(), &["add", "README.md"]);
        git_cmd(repo_dir.path(), &["commit", "-q", "-m", "init"]);

        let clean_err = commit_diff_with_env(repo_dir.path(), &BTreeMap::new())
            .unwrap_err()
            .0;
        assert!(clean_err.contains("No uncommitted changes"), "{clean_err}");
    }

    #[test]
    fn commit_diff_prefers_staged_and_falls_back_to_unstaged_plus_untracked() {
        let repo_dir = tempfile::tempdir().unwrap();
        let root = repo_dir.path();
        git_cmd(root, &["init", "-q"]);

        std::fs::write(root.join("initial.txt"), "hello unborn\n").unwrap();
        let unborn_untracked = commit_diff_with_env(root, &BTreeMap::new()).unwrap();
        assert!(
            unborn_untracked.contains("initial.txt"),
            "{unborn_untracked}"
        );
        assert!(
            unborn_untracked.contains("+hello unborn"),
            "{unborn_untracked}"
        );

        git_cmd(root, &["add", "initial.txt"]);
        git_cmd(root, &["commit", "-q", "-m", "init"]);

        std::fs::write(root.join("initial.txt"), "hello unstaged\n").unwrap();
        std::fs::write(root.join("notes.txt"), "new untracked line\n").unwrap();

        let fallback = commit_diff_with_env(root, &BTreeMap::new()).unwrap();
        assert!(fallback.contains("initial.txt"), "{fallback}");
        assert!(fallback.contains("+hello unstaged"), "{fallback}");
        assert!(fallback.contains("notes.txt"), "{fallback}");
        assert!(fallback.contains("+new untracked line"), "{fallback}");

        std::fs::write(root.join("staged.txt"), "only staged payload\n").unwrap();
        git_cmd(root, &["add", "staged.txt"]);

        let staged_only = commit_diff_with_env(root, &BTreeMap::new()).unwrap();
        assert!(staged_only.contains("staged.txt"), "{staged_only}");
        assert!(
            staged_only.contains("+only staged payload"),
            "{staged_only}"
        );
        assert!(!staged_only.contains("hello unstaged"), "{staged_only}");
        assert!(!staged_only.contains("new untracked line"), "{staged_only}");
    }

    #[test]
    fn resolve_program_reads_case_insensitive_path_and_command_disables_gcm_prompts() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe_name = if cfg!(windows) { "git.exe" } else { "git" };
        let fake_git = bin.join(exe_name);
        std::fs::write(&fake_git, "").unwrap();

        let env = BTreeMap::from([("Path".into(), bin.display().to_string())]);
        assert_eq!(resolve_program("git", &env), fake_git);

        let git_cmd = command("git", temp.path(), &["status"], &env);
        let envs: BTreeMap<_, _> = git_cmd
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_owned(), v?.to_str()?.to_owned())))
            .collect();
        assert_eq!(
            envs.get("GIT_TERMINAL_PROMPT").map(String::as_str),
            Some("0")
        );
        assert_eq!(
            envs.get("GCM_INTERACTIVE").map(String::as_str),
            Some("never")
        );
        assert_eq!(
            envs.get("GIT_CONFIG_KEY_0").map(String::as_str),
            Some("credential.interactive")
        );
        assert_eq!(
            envs.get("GIT_CONFIG_VALUE_0").map(String::as_str),
            Some("false")
        );
        let git_args: Vec<_> = git_cmd.get_args().filter_map(|a| a.to_str()).collect();
        assert_eq!(git_args, vec!["status"]);

        let gh_cmd = command("gh", temp.path(), &["pr", "view"], &env);
        let gh_args: Vec<_> = gh_cmd.get_args().filter_map(|a| a.to_str()).collect();
        assert_eq!(gh_args, vec!["pr", "view"]);

        let win_bin = temp.path().join("win-bin");
        std::fs::create_dir_all(&win_bin).unwrap();
        let gh_cmd_shim = win_bin.join("gh.cmd");
        std::fs::write(&gh_cmd_shim, "").unwrap();
        let win_env = BTreeMap::from([("path".into(), format!("\"{}\"", win_bin.display()))]);
        assert_eq!(
            resolve_program_for_platform("gh", &win_env, true),
            gh_cmd_shim
        );
        let gh_exe = win_bin.join("gh.exe");
        std::fs::write(&gh_exe, "").unwrap();
        assert_eq!(resolve_program_for_platform("gh", &win_env, true), gh_exe);
    }
}
