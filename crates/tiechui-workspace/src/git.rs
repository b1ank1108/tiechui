use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use tiechui_protocol::workspace::{
    Branches, CommitFile, Comparison, GitChange, GitCommit, GitFileStatus, GitOp, GitOperation,
    GitStatus, PullRequest, PullRequestState,
};

use crate::{Error, Result, fail, fs};

fn command(program: &str, root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(program);
    command
        .current_dir(root)
        .args(args)
        .stdin(Stdio::null())
        // Nothing can answer a prompt or an editor.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_OPTIONAL_LOCKS", "0");
    command
}

/// Stdout and stderr of a successful run.
fn output(mut command: Command) -> Result<(String, String)> {
    let output = command.output()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        let message = if stderr.trim().is_empty() {
            &stdout
        } else {
            &stderr
        };
        return fail(message.trim());
    }
    Ok((stdout, stderr))
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    Ok(output(command("git", root, args))?.0)
}

fn git_with(root: &Path, args: &[&str], paths: &[String]) -> Result<String> {
    let mut command = command("git", root, args);
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
fn repo(root: &Path) -> Result<Option<Repo>> {
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
pub fn status(root: &Path) -> Result<Option<GitStatus>> {
    let Some(repo) = repo(root)? else {
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
    )?;
    let index = numstat(&git(
        root,
        &["diff", "--cached", "--numstat", "-z", "--relative"],
    )?);
    let worktree = numstat(&git(root, &["diff", "--numstat", "-z", "--relative"])?);
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

pub fn compare(root: &Path, base: &str) -> Result<Comparison> {
    let Ok(merge_base) = git(root, &["merge-base", "HEAD", base]) else {
        return Ok(Comparison {
            merge_base: None,
            ahead: 0,
            behind: 0,
            files: vec![],
        });
    };
    let merge_base = merge_base.trim().to_owned();
    let range = format!("HEAD...{base}");
    let counts = git(root, &["rev-list", "--left-right", "--count", &range])?;
    let (ahead, behind) = counts.split_once('\t').unwrap_or_default();
    let diff = |format: &str| {
        git(
            root,
            &["diff", format, "-z", "--relative", &merge_base, "HEAD"],
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

pub fn log(root: &Path, limit: u32) -> Result<Vec<GitCommit>> {
    if git(root, &["rev-parse", "--verify", "-q", "HEAD"]).is_err() {
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
pub fn show(root: &Path, rev: &str, path: &str) -> Result<Option<String>> {
    fs::resolve(root, path)?;
    let mut command = command("git", root, &["show", &format!("{rev}:./{path}")]);
    let output = command.output()?;
    Ok(output
        .status
        .success()
        .then(|| fs::text(output.stdout))
        .flatten())
}

pub fn branches(root: &Path) -> Result<Branches> {
    let text = git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
            "refs/remotes",
        ],
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
pub fn pull_request(root: &Path) -> Result<Option<PullRequest>> {
    let Ok((text, _)) = output(command(
        "gh",
        root,
        &["pr", "view", "--json", "number,state,isDraft,url"],
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

pub fn run(root: &Path, op: &GitOp) -> Result<String> {
    let mut out = String::new();
    let mut step = |command: Command| -> Result<()> {
        let (stdout, stderr) = output(command)?;
        for text in [stdout, stderr] {
            out.push_str(&text);
        }
        Ok(())
    };
    let git = |args: &[&str]| command("git", root, args);
    let operation = || -> Result<GitOperation> {
        repo(root)?
            .and_then(|repo| repo.operation)
            .ok_or_else(|| Error("No merge or rebase is in progress".into()))
    };
    match op {
        GitOp::Stage { paths } => {
            git_with(root, &["add", "-A"], paths)?;
        }
        GitOp::Unstage { paths } => {
            if git(&["rev-parse", "--verify", "-q", "HEAD"])
                .status()?
                .success()
            {
                git_with(root, &["restore", "--staged"], paths)?;
            } else {
                git_with(root, &["rm", "--cached", "-r", "-q"], paths)?;
            }
        }
        GitOp::Discard { paths } => {
            // Unstaged, new files are untracked: delete those, then restore the rest.
            if git(&["rev-parse", "--verify", "-q", "HEAD"])
                .status()?
                .success()
            {
                git_with(root, &["reset", "-q"], paths)?;
            }
            git_with(root, &["clean", "-f", "-q"], paths)?;
            let tracked: Vec<String> = git_with(root, &["ls-files", "-z"], paths)?
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned)
                .collect();
            if !tracked.is_empty() {
                git_with(root, &["checkout", "-q"], &tracked)?;
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
        GitOp::CreatePr => step(command("gh", root, &["pr", "create", "--fill"]))?,
    }
    Ok(out.trim().to_owned())
}
