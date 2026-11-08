use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use ignore::overrides::OverrideBuilder;
use ignore::{WalkBuilder, WalkState};
use regex::{Regex, RegexBuilder};
use tiechui_protocol::workspace::{SearchGroup, SearchHit, SearchParams, SearchResult};

use crate::{Result, fail, fs, lock};

const MAX_HITS: u32 = 2000;
const MAX_FILE: u64 = 2 << 20;
const MAX_LINE: usize = 1000;

/// Content search over files outside `.gitignore`, in parallel.
pub fn search(root: &Path, params: &SearchParams) -> Result<SearchResult> {
    if params.query.is_empty() {
        return Ok(SearchResult {
            groups: vec![],
            total: 0,
            truncated: false,
        });
    }
    let pattern = if params.regex {
        params.query.clone()
    } else {
        regex::escape(&params.query)
    };
    let pattern = if params.whole_word {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern
    };
    let Ok(matcher) = RegexBuilder::new(&pattern)
        .case_insensitive(!params.case_sensitive)
        .build()
    else {
        return fail("Invalid regular expression");
    };
    let mut overrides = OverrideBuilder::new(root);
    for (globs, prefix) in [(&params.include, ""), (&params.exclude, "!")] {
        for glob in globs
            .iter()
            .map(|glob| glob.trim())
            .filter(|glob| !glob.is_empty())
        {
            // A directory matches with its contents, at any depth unless the glob has a `/`.
            let glob = glob.trim_end_matches('/');
            let deep = if glob.contains('/') {
                glob.to_owned()
            } else {
                format!("**/{glob}")
            };
            overrides.add(&format!("{prefix}{glob}"))?;
            overrides.add(&format!("{prefix}{deep}/**"))?;
        }
    }
    let overrides = overrides.build()?;
    let groups = Mutex::new(Vec::new());
    let total = AtomicU32::new(0);
    WalkBuilder::new(root)
        .hidden(false)
        .overrides(overrides)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build_parallel()
        .run(|| {
            Box::new(|entry| {
                if total.load(Ordering::Relaxed) >= MAX_HITS {
                    return WalkState::Quit;
                }
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };
                if !entry.file_type().is_some_and(|kind| kind.is_file())
                    || entry.metadata().map_or(true, |m| m.len() > MAX_FILE)
                {
                    return WalkState::Continue;
                }
                let hits = std::fs::read(entry.path())
                    .ok()
                    .and_then(fs::text)
                    .map(|text| hits(&matcher, &text))
                    .unwrap_or_default();
                if hits.is_empty() {
                    return WalkState::Continue;
                }
                total.fetch_add(hits.len() as u32, Ordering::Relaxed);
                if let Ok(path) = entry.path().strip_prefix(root) {
                    lock(&groups).push(SearchGroup {
                        path: path.to_string_lossy().replace('\\', "/"),
                        hits,
                    });
                }
                WalkState::Continue
            })
        });
    let mut groups = groups.into_inner().unwrap_or_else(|e| e.into_inner());
    groups.sort_by(|a, b| a.path.cmp(&b.path));
    let total = total.into_inner();
    Ok(SearchResult {
        groups,
        total,
        truncated: total >= MAX_HITS,
    })
}

fn hits(matcher: &Regex, text: &str) -> Vec<SearchHit> {
    let utf16 = |s: &str| s.encode_utf16().count() as u32;
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let mut end = line.len().min(MAX_LINE);
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        let line = &line[..end];
        let ranges: Vec<_> = matcher
            .find_iter(line)
            .filter(|m| !m.is_empty())
            .map(|m| (utf16(&line[..m.start()]), utf16(&line[..m.end()])))
            .collect();
        if !ranges.is_empty() {
            hits.push(SearchHit {
                line: index as u32 + 1,
                text: line.to_owned(),
                ranges,
            });
        }
    }
    hits
}
