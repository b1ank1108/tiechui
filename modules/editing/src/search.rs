use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tiechui_plugin_api::tools::{
    ToolContext, ToolError, ToolOutput, exec,
    output::{DEFAULT_MAX_BYTES, TextHead, format_size},
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::{check, resolve, string};

const MAX_RECORD: usize = 8 * 1024 * 1024;
const SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

async fn ripgrep(cancel: &CancellationToken) -> Result<PathBuf, ToolError> {
    // TIECHUI_HOME/bin holds user-installed binaries; nothing is downloaded.
    let home = std::env::var_os("TIECHUI_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            tiechui_plugin_api::user_home()
                .unwrap_or_default()
                .join(".tiechui")
        });
    let local = home.join("bin/rg");
    find_binary(&local, "rg", cancel).await
}

async fn find_binary(
    local: &Path,
    program: &str,
    cancel: &CancellationToken,
) -> Result<PathBuf, ToolError> {
    if local.exists() {
        return Ok(local.into());
    }
    check(cancel)?;
    let mut command = Command::new(program);
    command.arg("--version");
    if scan_command(&mut command, cancel, |_| Ok(false))
        .await
        .is_ok()
    {
        return Ok(PathBuf::from(program));
    }
    check(cancel)?;
    Err(ToolError("ripgrep (rg) is not available. Install a reviewed rg binary before using grep; Tiechui will not download it.".into()))
}

struct ProcessResult {
    code: Option<i32>,
    stderr: String,
    stopped: bool,
}

async fn scan_command(
    command: &mut Command,
    cancel: &CancellationToken,
    mut visit: impl FnMut(&str) -> Result<bool, ToolError>,
) -> Result<ProcessResult, ToolError> {
    check(cancel)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let (mut child, tree) = exec::Tree::spawn(command)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolError("Missing search stdout".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolError("Missing search stderr".into()))?;
    let mut record = Vec::new();
    let mut errors = Vec::new();
    let mut out_chunk = [0; 65536];
    let mut err_chunk = [0; 8192];
    let (mut out_eof, mut err_eof) = (false, false);
    let mut stopped = false;
    let mut failure = None;
    let mut force_at = None;
    while !out_eof || !err_eof {
        tokio::select! {
            biased;
            _ = cancel.cancelled(), if !stopped => {
                stopped = true;
            }
            n = stdout.read(&mut out_chunk), if !out_eof => {
                match n {
                    Err(e) => { failure = Some(ToolError(e.to_string())); stopped = true; out_eof = true; }
                    Ok(0) => {
                        out_eof = true;
                        if !stopped && !record.is_empty() {
                            let line=String::from_utf8_lossy(&record);
                            match visit(line.strip_suffix('\r').unwrap_or(&line)) {
                                Ok(stop) => stopped = stop, Err(e) => { failure = Some(e); stopped = true; }
                            }
                        }
                    }
                    Ok(n) if !stopped => {
                        for part in out_chunk[..n].split_inclusive(|&b| b == b'\n') {
                            let newline = part.last() == Some(&b'\n');
                            let part = if newline { &part[..part.len()-1] } else { part };
                            if record.len() + part.len() > MAX_RECORD {
                                failure = Some(ToolError("Search output contains a record larger than 8 MiB. Narrow the path/pattern or use read on the file; no partial search result was returned.".into()));
                                stopped = true;
                                break;
                            }
                            record.extend_from_slice(part);
                            if newline {
                                let line=String::from_utf8_lossy(&record);
                                match visit(line.strip_suffix('\r').unwrap_or(&line)) {
                                    Ok(stop) => stopped = stop, Err(e) => { failure = Some(e); stopped = true; }
                                }
                                record.clear();
                                if stopped { break; }
                            }
                        }
                    }
                    Ok(_) => {}
                }
            }
            n = stderr.read(&mut err_chunk), if !err_eof => {
                match n { Ok(0) => err_eof = true, Ok(n) => errors.extend(String::from_utf8_lossy(&err_chunk[..n]).encode_utf16().take(65536-errors.len())),
                    Err(e) => { err_eof = true; stopped = true; failure = Some(ToolError(e.to_string())); } }
            }
            _ = async { match force_at { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending().await } } => {
                let _ = tree.kill();
                force_at = None;
            }
        }
        if stopped && force_at.is_none() {
            let _ = tree.interrupt();
            force_at = Some(tokio::time::Instant::now() + Duration::from_secs(1));
        }
    }
    let status = if stopped {
        match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let _ = tree.kill();
                child.wait().await?
            }
        }
    } else {
        tokio::select! { status = child.wait() => status?, _ = cancel.cancelled() => {
            let _ = tree.kill();
            child.wait().await?
        } }
    };
    check(cancel)?;
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(ProcessResult {
        code: status.code(),
        stderr: String::from_utf16_lossy(&errors),
        stopped,
    })
}

pub(crate) async fn run(
    name: &str,
    input: Value,
    ctx: &ToolContext,
) -> Result<ToolOutput, ToolError> {
    if name == "find" {
        find(input, ctx).await
    } else {
        grep(input, ctx).await
    }
}

async fn find(input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let root = resolve(
        input["path"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("."),
        &ctx.cwd,
    )?;
    let pattern = string(&input, "pattern")?.to_owned();
    let limit = match input.get("limit") {
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| ToolError("limit must be a nonnegative integer".into()))?,
        None => 1000,
    };
    let cancel = ctx.cancel.clone();
    tokio::task::spawn_blocking(move || find_paths(&root, &pattern, limit, &cancel))
        .await
        .map_err(|e| ToolError(e.to_string()))?
}

fn find_paths(
    root: &Path,
    pattern: &str,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<ToolOutput, ToolError> {
    check(cancel)?;
    if !root.is_dir() {
        return Err(ToolError(format!(
            "Search path is not a directory: {}",
            root.display()
        )));
    }
    if Path::new(pattern).is_absolute() {
        return Err(ToolError(
            "Glob patterns must be relative to the search directory".into(),
        ));
    }
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| ToolError(format!("Invalid glob: {e}")))?
        .compile_matcher();
    let full_path = pattern.contains('/');
    let filter_cancel = cancel.clone();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .filter_entry(move |entry| !filter_cancel.is_cancelled() && entry.file_name() != ".git")
        .build();
    let mut paths = BTreeSet::new();
    let mut count = 0usize;
    for entry in walker {
        check(cancel)?;
        let entry = entry.map_err(|e| ToolError(format!("File discovery failed: {e}")))?;
        if let Some(error) = entry.error() {
            return Err(ToolError(format!("File discovery failed: {error}")));
        }
        if entry.depth() == 0 {
            continue;
        }
        let path = entry
            .path()
            .strip_prefix(root)
            .map_err(|e| ToolError(e.to_string()))?;
        let matched = if full_path {
            glob.is_match(path)
        } else {
            glob.is_match(Path::new(entry.file_name()))
        };
        if !matched {
            continue;
        }
        let name = path
            .to_str()
            .filter(|name| !name.chars().any(char::is_control))
            .ok_or_else(|| {
                ToolError(format!(
                    "Cannot return a non-UTF-8 or control-character path: {:?}",
                    path
                ))
            })?;
        let name = if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            format!("{name}/")
        } else {
            name.to_owned()
        };
        count = count.saturating_add(1);
        paths.insert(name);
        if limit != 0 && paths.len() > limit {
            paths.pop_last();
        }
    }
    check(cancel)?;
    if paths.is_empty() {
        return Ok(ToolOutput::text("No files found matching pattern"));
    }
    let mut head = TextHead::new(9_007_199_254_740_991);
    for (index, path) in paths.into_iter().enumerate() {
        check(cancel)?;
        if index > 0 {
            head.append("\n");
        }
        head.append(&path);
    }
    let truncation = head.finish();
    let mut details = json!({});
    let mut notices = Vec::new();
    if limit != 0 && count > limit {
        details["resultLimitReached"] = json!(limit);
        notices.push(format!(
            "{limit} results limit reached. Use limit={} for more, or refine pattern",
            limit.saturating_mul(2)
        ));
    }
    if truncation.truncated {
        details["truncation"] = json!(truncation);
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
    }
    Ok(output(truncation.content, details, notices))
}

fn code(code: Option<i32>) -> String {
    code.map_or_else(|| "null".into(), |c| c.to_string())
}
fn relative(root: &Path, path: &Path) -> String {
    let a: Vec<_> = root.components().collect();
    let b: Vec<_> = path.components().collect();
    let shared = a.iter().zip(&b).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in shared..a.len() {
        out.push("..");
    }
    for c in &b[shared..] {
        out.push(c);
    }
    out.to_string_lossy().into_owned()
}
fn decode(value: &Value) -> Option<String> {
    value["text"].as_str().map(str::to_owned).or_else(|| {
        STANDARD
            .decode(value["bytes"].as_str()?)
            .ok()
            .map(|v| String::from_utf8_lossy(&v).into_owned())
    })
}
fn clip(text: &str) -> (String, bool) {
    let chars: Vec<_> = text.encode_utf16().take(501).collect();
    if chars.len() > 500 {
        (
            format!("{}... [truncated]", String::from_utf16_lossy(&chars[..500])),
            true,
        )
    } else {
        (text.into(), false)
    }
}
fn output(mut text: String, details: Value, notices: Vec<String>) -> ToolOutput {
    if !notices.is_empty() {
        text += &format!("\n\n[{}]", notices.join(". "));
    }
    let mut out = ToolOutput::text(text);
    if details.as_object().is_some_and(|m| !m.is_empty()) {
        out.details = Some(details);
    }
    out
}

struct Grep {
    root: PathBuf,
    directory: bool,
    name: String,
    file: String,
    mode: String,
    only: bool,
    before: usize,
    after: usize,
    offset: usize,
    limit: usize,
    lines: BTreeMap<usize, (String, bool)>,
    pending: Vec<(usize, usize)>,
    last: usize,
    newline: bool,
    count: usize,
    tail: usize,
    reached: bool,
    first: bool,
    clipped: bool,
    seen: usize,
    occurrences: usize,
    head: TextHead,
}
impl Grep {
    fn append(&mut self, text: &str) {
        if !self.first {
            self.head.append("\n");
        }
        self.first = false;
        self.head.append(text);
    }
    fn finish_file(&mut self) {
        if self.mode == "content" || self.occurrences == 0 {
            return;
        }
        if self.seen >= self.offset && self.count < self.limit {
            let text = if self.mode == "count" {
                format!("{}: {}", self.name, self.occurrences)
            } else {
                self.name.clone()
            };
            self.append(&text);
            self.count += 1;
            self.reached = self.count >= self.limit;
        }
        self.seen += 1;
        self.occurrences = 0;
    }
    fn flush(&mut self, through: usize) {
        while self
            .pending
            .first()
            .is_some_and(|&(_, end)| end.saturating_add(self.after) <= through)
        {
            let (start, end) = self.pending.remove(0);
            for (&number, (text, clipped)) in self
                .lines
                .range(start.saturating_sub(self.before).max(1)..=end.saturating_add(self.after))
            {
                let sep = if number >= start && number <= end {
                    ':'
                } else {
                    '-'
                };
                if !self.first {
                    self.head.append("\n");
                }
                self.first = false;
                self.head
                    .append(&format!("{}{sep}{number}{sep} {text}", self.name));
                self.clipped |= clipped;
            }
        }
        let keep = self
            .pending
            .first()
            .map_or(self.last, |&(start, _)| start)
            .saturating_sub(self.before)
            .max(1);
        self.lines.retain(|&number, _| number >= keep);
    }
    fn event(&mut self, raw: &str) -> Result<bool, ToolError> {
        let Ok(event) = serde_json::from_str::<Value>(raw) else {
            return Ok(false);
        };
        let data = &event["data"];
        let Some(file) = decode(&data["path"]) else {
            return Ok(false);
        };
        if file != self.file {
            self.flush(usize::MAX);
            self.finish_file();
            self.lines.clear();
            self.pending.clear();
            self.last = 0;
            self.newline = false;
            self.file = file;
            self.name = if self.directory {
                relative(&self.root, Path::new(&self.file))
            } else {
                String::new()
            };
            if self.name.is_empty() || self.name == ".." || self.name.starts_with("../") {
                self.name = Path::new(&self.file)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
            }
        }
        let kind = event["type"].as_str().unwrap_or("");
        if kind == "end" {
            if self.after > 0 && self.newline {
                self.lines.insert(self.last + 1, (String::new(), false));
            }
            self.flush(usize::MAX);
            self.finish_file();
            self.lines.clear();
            self.pending.clear();
            return Ok(self.reached);
        }
        if kind != "match" && kind != "context" {
            return Ok(false);
        }
        let (Some(number), Some(text)) = (data["line_number"].as_u64(), decode(&data["lines"]))
        else {
            return Ok(false);
        };
        let number = number as usize;
        if self.mode != "content" {
            if kind == "match" {
                self.occurrences += data["submatches"].as_array().map_or(1, Vec::len);
            }
            return Ok(false);
        }
        let selected = kind == "match" && !self.reached && self.seen >= self.offset;
        if kind == "match" && !self.reached {
            self.seen += 1;
        }
        if self.only {
            if selected {
                if let Some(matches) = data["submatches"].as_array() {
                    for m in matches {
                        let start = m["start"].as_u64().unwrap_or(0) as usize;
                        let line = number
                            + text.as_bytes()[..start.min(text.len())]
                                .iter()
                                .filter(|&&b| b == b'\n')
                                .count();
                        for (i, part) in decode(&m["match"])
                            .unwrap_or_default()
                            .replace("\r\n", "\n")
                            .split('\n')
                            .enumerate()
                        {
                            let (part, clipped) = clip(part);
                            self.clipped |= clipped;
                            self.append(&format!("{}:{}: {part}", self.name, line + i));
                        }
                    }
                }
                self.count += 1;
                self.reached = self.count >= self.limit;
            }
            return Ok(self.reached);
        }
        let clean = text.replace("\r\n", "\n").replace('\r', "");
        let parts: Vec<_> = clean
            .strip_suffix('\n')
            .unwrap_or(&clean)
            .split('\n')
            .collect();
        self.last = number + parts.len() - 1;
        self.newline = text.ends_with('\n');
        if selected {
            self.pending.push((number, self.last));
            self.count += 1;
            if self.count >= self.limit {
                self.reached = true;
                self.tail = self.last.saturating_add(self.after);
            }
        }
        for (i, part) in parts.iter().enumerate() {
            self.lines.insert(number + i, clip(part));
        }
        self.flush(self.last);
        Ok(self.reached && self.last >= self.tail)
    }
}

async fn grep(input: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
    let executable = ripgrep(&ctx.cancel).await?;
    let context = input["context"]
        .as_f64()
        .filter(|v| *v > 0.0)
        .unwrap_or(0.0);
    let limit = input["limit"].as_f64().unwrap_or(100.0).max(1.0);
    let mode = input["outputMode"].as_str().unwrap_or("content");
    let offset = input["offset"].as_f64().unwrap_or(0.0);
    let only = mode == "content" && input["onlyMatching"] == true;
    let before = if mode == "content" && !only {
        input["beforeContext"].as_f64().unwrap_or(context)
    } else {
        0.0
    };
    let after = if mode == "content" && !only {
        input["afterContext"].as_f64().unwrap_or(context)
    } else {
        0.0
    };
    if ![context, before, after, offset, limit]
        .iter()
        .all(|v| v.is_finite() && v.fract() == 0.0 && *v >= 0.0 && *v <= SAFE_INTEGER)
    {
        return Err(ToolError("grep context/offset must be non-negative safe integers and limit must be a positive safe integer".into()));
    }
    if !["content", "files_with_matches", "count"].contains(&mode) {
        return Err(ToolError("Invalid grep outputMode".into()));
    }
    let root = resolve(
        input["path"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("."),
        &ctx.cwd,
    )?;
    let directory = root
        .metadata()
        .map_err(|_| ToolError(format!("Path not found: {}", root.display())))?
        .is_dir();
    let paged = !input["offset"].is_null() || mode != "content";
    let mut command = Command::new(executable);
    command.args([
        "--no-config",
        "--json",
        "--line-number",
        "--color=never",
        "--hidden",
    ]);
    if before > 0.0 {
        command.args(["--before-context", &before.to_string()]);
    }
    if after > 0.0 {
        command.args(["--after-context", &after.to_string()]);
    }
    if paged {
        command.args(["--sort", "path"]);
    }
    if input["multiline"] == true {
        command.args(["--multiline", "--multiline-dotall"]);
    }
    if mode == "files_with_matches" {
        command.args(["--max-count", "1"]);
    }
    if input["ignoreCase"] == true {
        command.arg("--ignore-case");
    }
    if input["literal"] == true {
        command.arg("--fixed-strings");
    }
    for (key, flag) in [("glob", "--glob"), ("type", "--type")] {
        if let Some(value) = input[key].as_str().filter(|v| !v.is_empty()) {
            command.args([flag, value]);
        }
    }
    command.arg("--").arg(string(&input, "pattern")?).arg(&root);
    let mut grep = Grep {
        root,
        directory,
        name: String::new(),
        file: String::new(),
        mode: mode.into(),
        only,
        before: before as usize,
        after: after as usize,
        offset: offset as usize,
        limit: limit as usize,
        lines: BTreeMap::new(),
        pending: Vec::new(),
        last: 0,
        newline: false,
        count: 0,
        tail: 0,
        reached: false,
        first: true,
        clipped: false,
        seen: 0,
        occurrences: 0,
        head: TextHead::new(SAFE_INTEGER as usize),
    };
    let result = scan_command(&mut command, &ctx.cancel, |raw| grep.event(raw)).await?;
    if !(grep.reached && result.stopped || result.code == Some(0) || result.code == Some(1)) {
        return Err(ToolError(if result.stderr.trim().is_empty() {
            format!("ripgrep exited with code {}", code(result.code))
        } else {
            result.stderr.trim().into()
        }));
    }
    grep.flush(usize::MAX);
    grep.finish_file();
    check(&ctx.cancel)?;
    if grep.count == 0 {
        return Ok(ToolOutput::text(if offset > 0.0 {
            format!("No more matches at offset={offset}")
        } else {
            "No matches found".into()
        }));
    }
    let truncation = grep.head.finish();
    let mut details = json!({});
    let mut notices = Vec::new();
    if paged {
        details = json!({"outputMode":mode,"offset":offset as usize,"returned":grep.count});
    }
    if grep.reached {
        if paged && !truncation.truncated {
            details["nextOffset"] = json!(offset as usize + grep.count);
            notices.push(format!("Page limit reached. Continue with offset={}, limit={limit} and the same search options; files may have changed", offset as usize + grep.count));
        } else if !paged {
            notices.push(format!(
                "{limit} matches limit reached. Use limit={} for more, or refine pattern",
                limit * 2.0
            ));
        }
        details["matchLimitReached"] = json!(limit as usize);
    }
    if truncation.truncated {
        notices.push(format!(
            "{} limit reached{}",
            format_size(DEFAULT_MAX_BYTES),
            if paged {
                "; narrow the search or reduce limit/context before advancing offset"
            } else {
                ""
            }
        ));
        details["truncation"] = json!(truncation);
    }
    if grep.clipped {
        notices.push("Some lines truncated to 500 chars. Use read tool to see full lines".into());
        details["linesTruncated"] = json!(true);
    }
    Ok(output(truncation.content, details, notices))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[tokio::test]
    async fn unavailable_binaries_fail_without_falling_back_or_downloading() {
        let directory = tempfile::tempdir().unwrap();
        let error = find_binary(
            &directory.path().join("rg"),
            "tiechui-fixture-command-that-does-not-exist",
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.0.contains("will not download"));
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn native_find_sorts_limits_and_honors_ignore_without_a_git_repository() {
        use std::fs;
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("outside.rs"), "").unwrap();
        symlink(outside.path(), dir.path().join("linked")).unwrap();
        fs::create_dir_all(dir.path().join(".git/nested")).unwrap();
        fs::write(dir.path().join(".git/nested/no.rs"), "").unwrap();
        // require_git(false) also applies when the root has no .git marker.
        fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(dir.path().join(".ignore"), "ignored2.rs\n").unwrap();
        for name in ["b.rs", "ignored.rs", ".hidden.rs", "a.rs", "ignored2.rs"] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        let cancel = CancellationToken::new();
        let all = find_paths(dir.path(), "*.rs", 0, &cancel).unwrap();
        assert_eq!(
            all.content,
            ToolOutput::text(".hidden.rs\na.rs\nb.rs").content
        );
        let page = find_paths(dir.path(), "*.rs", 2, &cancel).unwrap();
        assert_eq!(page.details.unwrap()["resultLimitReached"], 2);
        assert!(
            matches!(&page.content[0], tiechui_plugin_api::model::Content::Text { text, .. } if text.starts_with(".hidden.rs\na.rs\n"))
        );
        assert!(
            find_paths(dir.path(), "*.rs", 3, &cancel)
                .unwrap()
                .details
                .is_none()
        );
        fs::remove_dir_all(dir.path().join(".git")).unwrap();
        assert_eq!(
            find_paths(dir.path(), "*.rs", 0, &cancel).unwrap().content,
            all.content
        );
        cancel.cancel();
        assert!(find_paths(dir.path(), "*", 0, &cancel).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn native_find_reports_unrepresentable_names_and_invalid_roots() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        assert!(find_paths(&dir.path().join("missing"), "*", 0, &cancel).is_err());
        for name in [
            Some(OsString::from("bad\nname")),
            cfg!(target_os = "linux").then(|| OsString::from_vec(vec![0xff])),
        ]
        .into_iter()
        .flatten()
        {
            let path = dir.path().join(name);
            std::fs::write(&path, "").unwrap();
            assert!(
                find_paths(dir.path(), "*", 0, &cancel)
                    .unwrap_err()
                    .0
                    .contains("Cannot return")
            );
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn decoder_handles_utf8_crlf_and_final_record() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf '中文😀\\r\\nlast'"]);
        let mut rows = Vec::new();
        scan_command(&mut command, &CancellationToken::new(), |line| {
            rows.push(line.to_owned());
            Ok(false)
        })
        .await
        .unwrap();
        assert_eq!(rows, ["中文😀", "last"]);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_reaps_a_process_that_ignores_term() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "trap '' TERM; printf '%s\\n' $$; while :; do sleep 1; done",
        ]);
        let cancel = CancellationToken::new();
        let mut pid = 0;
        let error = scan_command(&mut command, &cancel, |line| {
            pid = line.parse::<i32>().unwrap();
            cancel.cancel();
            Ok(false)
        })
        .await
        .err()
        .unwrap();
        assert_eq!(error.0, "Operation aborted");
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }

    #[tokio::test]
    async fn oversized_records_fail_before_results_and_limit_stop_drains_them() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'first\\n'; head -c 8388609 /dev/zero"]);
        let error = scan_command(&mut command, &CancellationToken::new(), |_| Ok(false))
            .await
            .err()
            .unwrap();
        assert!(
            error.0.contains("record larger than 8 MiB")
                && error.0.contains("no partial search result")
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'first\\n'; head -c 8388609 /dev/zero"]);
        let result = scan_command(&mut command, &CancellationToken::new(), |_| Ok(true))
            .await
            .unwrap();
        assert!(result.stopped);
    }
}
