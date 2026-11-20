use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{Result, ports};

const MAX_HEADER_BYTES: u64 = 65_536;
const SESSION_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSessionCwd {
    pub session_id: String,
    pub cwd: PathBuf,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathOverlap {
    Equal,
    ContainsWorktree,
    InsideWorktree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeIsolationConflict {
    pub session_id: String,
    pub cwd: String,
    pub title: Option<String>,
    pub archived: bool,
    pub overlap: PathOverlap,
    pub reason: String,
}

#[derive(Clone)]
struct CandidateSession {
    session_id: String,
    cwd: PathBuf,
    title: Option<String>,
    archived: bool,
}

#[derive(Default)]
pub(crate) struct DiskMeta {
    pub(crate) title: Option<String>,
    pub(crate) archived: bool,
}

pub(crate) struct DiskSessionHeader {
    pub(crate) path: PathBuf,
    pub(crate) session_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) created_at: u64,
}

pub fn check_worktree_isolation(
    home: &Path,
    worktree_cwd: &Path,
    current_session_id: &str,
    live_sessions: &[LiveSessionCwd],
) -> Result<Option<WorktreeIsolationConflict>> {
    let target = normalize_dir(worktree_cwd);
    let mut candidates = collect_disk_sessions(home, current_session_id)?;
    merge_live_sessions(&mut candidates, current_session_id, live_sessions);
    for candidate in candidates.into_values() {
        let other = normalize_dir(&candidate.cwd);
        if let Some(overlap) = classify_overlap(&target, &other) {
            return Ok(Some(build_conflict(candidate, overlap)));
        }
    }
    Ok(None)
}

pub(crate) fn normalize_dir(path: &Path) -> PathBuf {
    ports::resolve_existing_prefix(&ports::clean_path(path))
}

pub(crate) fn classify_overlap(worktree_dir: &Path, other_dir: &Path) -> Option<PathOverlap> {
    if crate::win_path::looks_like_windows_root(worktree_dir)
        || crate::win_path::looks_like_windows_root(other_dir)
    {
        return crate::win_path::classify_windows_overlap(
            &worktree_dir.to_string_lossy(),
            &other_dir.to_string_lossy(),
        );
    }
    if worktree_dir == other_dir {
        Some(PathOverlap::Equal)
    } else if worktree_dir.starts_with(other_dir) {
        Some(PathOverlap::ContainsWorktree)
    } else if other_dir.starts_with(worktree_dir) {
        Some(PathOverlap::InsideWorktree)
    } else {
        None
    }
}

fn build_conflict(candidate: CandidateSession, overlap: PathOverlap) -> WorktreeIsolationConflict {
    let cwd = candidate.cwd.display().to_string();
    let label = match &candidate.title {
        Some(title) if !title.trim().is_empty() => {
            format!("\"{title}\" ({})", candidate.session_id)
        }
        _ => candidate.session_id.clone(),
    };
    let archived_note = if candidate.archived {
        " [archived]"
    } else {
        ""
    };
    let relation = match overlap {
        PathOverlap::Equal => "shares this worktree directory",
        PathOverlap::ContainsWorktree => "uses an ancestor directory containing this worktree",
        PathOverlap::InsideWorktree => "uses a subdirectory inside this worktree",
    };
    let reason = format!(
        "whole-directory restore refused: session {label}{archived_note} {relation} ({cwd})"
    );
    WorktreeIsolationConflict {
        session_id: candidate.session_id,
        cwd,
        title: candidate.title,
        archived: candidate.archived,
        overlap,
        reason,
    }
}

fn collect_disk_sessions(
    home: &Path,
    current_session_id: &str,
) -> Result<BTreeMap<String, CandidateSession>> {
    let mut out = BTreeMap::new();
    for header in list_disk_session_headers(home)? {
        if header.session_id == current_session_id {
            continue;
        }
        let meta = read_disk_meta(&header.path);
        out.insert(
            header.session_id.clone(),
            CandidateSession {
                session_id: header.session_id,
                cwd: header.cwd,
                title: meta.title,
                archived: meta.archived,
            },
        );
    }
    Ok(out)
}

pub(crate) fn list_disk_session_headers(home: &Path) -> Result<Vec<DiskSessionHeader>> {
    let mut out = Vec::new();
    let groups = match fs::read_dir(home.join("sessions")) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(err) => return Err(err.into()),
    };
    for group in groups {
        let group = group?;
        if group.file_type()?.is_dir() {
            scan_session_group_headers(&group.path(), &mut out)?;
        }
    }
    Ok(out)
}

fn scan_session_group_headers(group_dir: &Path, out: &mut Vec<DiskSessionHeader>) -> Result<()> {
    let entries = match fs::read_dir(group_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl")
            && let Some(header) = read_disk_header(&path)
        {
            out.push(header);
        }
    }
    Ok(())
}

fn read_disk_header(path: &Path) -> Option<DiskSessionHeader> {
    let line = read_header_line(path)?;
    let header: Value = serde_json::from_str(&line).ok()?;
    parse_disk_header(path, &header)
}

fn parse_disk_header(path: &Path, header: &Value) -> Option<DiskSessionHeader> {
    let created_at = valid_header_created_at(header)?;
    let id = header["id"].as_str()?;
    let cwd_path = Path::new(header["cwd"].as_str()?);
    (!id.is_empty() && cwd_path.is_absolute()).then(|| DiskSessionHeader {
        path: path.to_path_buf(),
        session_id: id.to_owned(),
        cwd: cwd_path.to_path_buf(),
        created_at,
    })
}

fn valid_header_created_at(header: &Value) -> Option<u64> {
    (header["format_version"].as_u64() == Some(u64::from(SESSION_FORMAT_VERSION)))
        .then(|| header["created_at"].as_u64())
        .flatten()
}

fn read_header_line(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    let mut line = String::new();
    let n = BufReader::new(file.take(MAX_HEADER_BYTES))
        .read_line(&mut line)
        .ok()?;
    (n > 0 && line.ends_with('\n')).then_some(line)
}

pub(crate) fn read_disk_meta(session_path: &Path) -> DiskMeta {
    let Some(val) = fs::read(session_path.with_extension("meta.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    else {
        return DiskMeta::default();
    };
    DiskMeta {
        title: val.get("title").and_then(Value::as_str).map(str::to_owned),
        archived: val.get("archived_at").is_some_and(|v| !v.is_null()),
    }
}

fn merge_live_sessions(
    candidates: &mut BTreeMap<String, CandidateSession>,
    current_session_id: &str,
    live_sessions: &[LiveSessionCwd],
) {
    for live in live_sessions {
        if live.session_id == current_session_id {
            continue;
        }
        let meta = live.path.as_deref().map(read_disk_meta).unwrap_or_default();
        candidates
            .entry(live.session_id.clone())
            .and_modify(|existing| {
                existing.cwd.clone_from(&live.cwd);
                if meta.title.is_some() {
                    existing.title = meta.title.clone();
                }
                existing.archived |= meta.archived;
            })
            .or_insert_with(|| CandidateSession {
                session_id: live.session_id.clone(),
                cwd: live.cwd.clone(),
                title: meta.title,
                archived: meta.archived,
            });
    }
}
