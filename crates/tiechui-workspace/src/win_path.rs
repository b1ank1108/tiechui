use std::path::{Path, PathBuf};

use crate::worktree_isolation::PathOverlap;

pub(crate) fn strip_windows_verbatim_str(raw: &str) -> String {
    let Some(rest) = strip_verbatim_marker(raw) else {
        return raw.to_owned();
    };
    if let Some(unc_tail) = strip_unc_marker(rest) {
        return format!(r"\\{unc_tail}");
    }
    if has_drive_prefix(rest) {
        return rest.to_owned();
    }
    raw.to_owned()
}

fn strip_verbatim_marker(raw: &str) -> Option<&str> {
    let b = raw.as_bytes();
    let is_sep = |c: u8| matches!(c, b'\\' | b'/');
    (b.len() >= 4 && is_sep(b[0]) && is_sep(b[1]) && b[2] == b'?' && is_sep(b[3]))
        .then(|| &raw[4..])
}

fn strip_unc_marker(rest: &str) -> Option<&str> {
    let b = rest.as_bytes();
    (b.len() >= 4 && rest[..3].eq_ignore_ascii_case("unc") && matches!(b[3], b'\\' | b'/'))
        .then(|| &rest[4..])
}

fn has_drive_prefix(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || matches!(b[2], b'\\' | b'/'))
}

pub(crate) fn strip_verbatim_path(path: &Path) -> PathBuf {
    if let Some(raw) = path.to_str() {
        let stripped = strip_windows_verbatim_str(raw);
        if stripped.len() != raw.len() {
            return PathBuf::from(stripped);
        }
    }
    path.to_path_buf()
}

pub(crate) fn normalize_windows_path_key(raw: &str) -> String {
    let stripped = strip_windows_verbatim_str(raw);
    let slashed = stripped.replace('/', "\\");
    if let Some(unc) = normalize_unc_path(&slashed) {
        return unc;
    }
    if has_drive_prefix(&slashed) {
        return normalize_drive_path(&slashed);
    }
    let rooted = slashed.starts_with('\\');
    let segments = resolve_dot_segments(&slashed);
    match (rooted, segments.is_empty()) {
        (true, true) => r"\".into(),
        (true, false) => format!(r"\{}", segments.join(r"\")),
        (false, _) => segments.join(r"\"),
    }
}

fn normalize_unc_path(slashed: &str) -> Option<String> {
    let tail = slashed.strip_prefix(r"\\")?;
    let mut parts = tail.split('\\').filter(|s| !s.is_empty());
    let server = parts.next()?.to_lowercase();
    let share = parts.next()?.to_lowercase();
    let rest: Vec<&str> = parts.collect();
    let segments = resolve_dot_slice(&rest);
    if segments.is_empty() {
        Some(format!(r"\\{server}\{share}"))
    } else {
        Some(format!(r"\\{server}\{share}\{}", segments.join(r"\")))
    }
}

fn normalize_drive_path(slashed: &str) -> String {
    let drive = (slashed.as_bytes()[0] as char).to_ascii_lowercase();
    let rest = &slashed[2..];
    let rooted = rest.starts_with('\\');
    let segments = resolve_dot_segments(rest);
    match (rooted, segments.is_empty()) {
        (true, true) => format!(r"{drive}:\"),
        (true, false) => format!(r"{drive}:\{}", segments.join(r"\")),
        (false, true) => format!("{drive}:"),
        (false, false) => format!(r"{drive}:{}", segments.join(r"\")),
    }
}

fn resolve_dot_segments(raw: &str) -> Vec<String> {
    let parts: Vec<&str> = raw.split('\\').filter(|s| !s.is_empty()).collect();
    resolve_dot_slice(&parts)
}

fn resolve_dot_slice(parts: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for &part in parts {
        match part {
            "." => {}
            ".." => {
                let _ = out.pop();
            }
            other => out.push(other.to_lowercase()),
        }
    }
    out
}

pub(crate) fn windows_path_starts_with(candidate_norm: &str, base_norm: &str) -> bool {
    if base_norm.is_empty() {
        return false;
    }
    if candidate_norm == base_norm {
        return true;
    }
    if base_norm.ends_with('\\') {
        candidate_norm.starts_with(base_norm)
    } else {
        candidate_norm
            .strip_prefix(base_norm)
            .is_some_and(|rest| rest.starts_with('\\'))
    }
}

fn windows_crosses_worktrees_boundary(ancestor_norm: &str, descendant_norm: &str) -> bool {
    let rest = if ancestor_norm.ends_with('\\') {
        descendant_norm.strip_prefix(ancestor_norm)
    } else {
        descendant_norm
            .strip_prefix(ancestor_norm)
            .and_then(|r| r.strip_prefix('\\'))
    };
    let Some(rest) = rest else {
        return false;
    };
    let parts: Vec<&str> = rest.split('\\').filter(|s| !s.is_empty()).collect();
    parts
        .windows(3)
        .any(|w| w[0] == ".tiechui" && w[1] == "worktrees")
}

pub(crate) fn classify_windows_overlap(worktree_raw: &str, other_raw: &str) -> Option<PathOverlap> {
    let wt = normalize_windows_path_key(worktree_raw);
    let other = normalize_windows_path_key(other_raw);
    if wt == other {
        Some(PathOverlap::Equal)
    } else if windows_path_starts_with(&wt, &other) {
        (!windows_crosses_worktrees_boundary(&other, &wt)).then_some(PathOverlap::ContainsWorktree)
    } else if windows_path_starts_with(&other, &wt) {
        (!windows_crosses_worktrees_boundary(&wt, &other)).then_some(PathOverlap::InsideWorktree)
    } else {
        None
    }
}

pub(crate) fn path_key(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(normalize_windows_path_key(&path.to_string_lossy()))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

pub(crate) fn paths_equal(a: &Path, b: &Path) -> bool {
    #[cfg(windows)]
    {
        normalize_windows_path_key(&a.to_string_lossy())
            == normalize_windows_path_key(&b.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        a == b
    }
}

pub(crate) fn path_starts_with(candidate: &Path, base: &Path) -> bool {
    #[cfg(windows)]
    {
        windows_path_starts_with(
            &normalize_windows_path_key(&candidate.to_string_lossy()),
            &normalize_windows_path_key(&base.to_string_lossy()),
        )
    }
    #[cfg(not(windows))]
    {
        candidate.starts_with(base)
    }
}

pub(crate) fn path_strip_prefix(candidate: &Path, base: &Path) -> Option<PathBuf> {
    if let Ok(rel) = candidate.strip_prefix(base) {
        return Some(rel.to_path_buf());
    }
    #[cfg(windows)]
    {
        let cand_norm = normalize_windows_path_key(&candidate.to_string_lossy());
        let base_norm = normalize_windows_path_key(&base.to_string_lossy());
        if cand_norm == base_norm {
            return Some(PathBuf::new());
        }
        let rest = if base_norm.ends_with('\\') {
            cand_norm.strip_prefix(&base_norm)
        } else {
            cand_norm
                .strip_prefix(&base_norm)
                .and_then(|r| r.strip_prefix('\\'))
        };
        if let Some(rel) = rest {
            return Some(PathBuf::from(rel));
        }
    }
    None
}

pub(crate) fn strip_inline_verbatim_prefixes(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut idx = 0;
    while idx < input.len() {
        let tail = &input[idx..];
        if let Some(after_marker) = strip_verbatim_marker(tail) {
            if let Some(unc_tail) = strip_unc_marker(after_marker) {
                out.push_str(r"\\");
                idx = input.len() - unc_tail.len();
                continue;
            }
            if has_drive_prefix(after_marker) {
                idx = input.len() - after_marker.len();
                continue;
            }
        }
        let Some(ch) = tail.chars().next() else {
            break;
        };
        out.push(ch);
        idx += ch.len_utf8();
    }
    out
}

pub(crate) fn windows_cmdline_contains_root(cmdline_norm: &str, root_norm: &str) -> bool {
    if root_norm.len() <= 1 {
        return false;
    }
    let bytes = cmdline_norm.as_bytes();
    let is_left = |pos: usize| -> bool {
        pos == 0
            || matches!(
                bytes[pos - 1],
                b' ' | b'\t' | b'=' | b'"' | b'\'' | b';' | b':'
            )
    };
    let is_right = |end: usize| -> bool {
        root_norm.ends_with('\\')
            || end >= bytes.len()
            || matches!(
                bytes[end],
                b'\\' | b' ' | b'\t' | b'"' | b'\'' | b';' | b':'
            )
    };
    cmdline_norm.match_indices(root_norm).any(|(pos, _)| {
        if !is_left(pos) || !is_right(pos + root_norm.len()) {
            return false;
        }
        let span = extract_cmdline_path_span(cmdline_norm, pos, root_norm.len());
        windows_path_starts_with(&normalize_windows_path_key(span), root_norm)
    })
}

fn extract_cmdline_path_span(cmdline: &str, start: usize, matched_len: usize) -> &str {
    let bytes = cmdline.as_bytes();
    let quote = (start > 0)
        .then(|| bytes[start - 1])
        .filter(|&b| b == b'"' || b == b'\'');
    let after = start + matched_len;
    let tail = &cmdline[after..];
    let end_offset = match quote {
        Some(q) => tail.find(q as char).unwrap_or(tail.len()),
        None => tail.find([' ', '\t', '"', '\'', ';']).unwrap_or(tail.len()),
    };
    &cmdline[start..after + end_offset]
}

pub(crate) fn normalize_windows_cmdline_for_search(cmdline: &str) -> String {
    strip_inline_verbatim_prefixes(cmdline)
        .replace('/', "\\")
        .to_lowercase()
}

pub(crate) fn looks_like_windows_root(path: &Path) -> bool {
    cfg!(windows)
        || path.to_str().is_some_and(|s| {
            has_drive_prefix(s) || strip_verbatim_marker(s).is_some() || s.starts_with(r"\\")
        })
}

pub(crate) fn is_windows_absolute(s: &str) -> bool {
    (s.len() > 2 && has_drive_prefix(s)) || s.starts_with(r"\\")
}

pub(crate) fn parse_windows_cmdline(cmdline: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut chars = cmdline.chars().peekable();
    while let Some(&ch) = chars.peek() {
        if matches!(ch, ' ' | '\t') {
            let _ = chars.next();
            continue;
        }
        args.push(read_one_windows_arg(&mut chars));
    }
    args
}

fn read_one_windows_arg(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut current = String::new();
    let mut in_quotes = false;
    while let Some(&ch) = chars.peek() {
        if ch == '\\' {
            consume_backslashes_and_quote(chars, &mut current, &mut in_quotes);
        } else if ch == '"' {
            let _ = chars.next();
            if in_quotes && chars.peek() == Some(&'"') {
                current.push('"');
                let _ = chars.next();
            } else {
                in_quotes = !in_quotes;
            }
        } else if !in_quotes && matches!(ch, ' ' | '\t') {
            break;
        } else {
            current.push(ch);
            let _ = chars.next();
        }
    }
    current
}

fn consume_backslashes_and_quote(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    current: &mut String,
    in_quotes: &mut bool,
) {
    let mut backslashes = 0_usize;
    while chars.peek() == Some(&'\\') {
        backslashes += 1;
        let _ = chars.next();
    }
    if chars.peek() == Some(&'"') {
        let _ = chars.next();
        for _ in 0..(backslashes / 2) {
            current.push('\\');
        }
        if backslashes.is_multiple_of(2) {
            if *in_quotes && chars.peek() == Some(&'"') {
                current.push('"');
                let _ = chars.next();
            } else {
                *in_quotes = !*in_quotes;
            }
        } else {
            current.push('"');
        }
    } else {
        for _ in 0..backslashes {
            current.push('\\');
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn strips_verbatim_drive_and_unc_prefixes_while_preserving_volume_guids() {
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\C:\Users\Alice\Repo"),
            r"C:\Users\Alice\Repo"
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\c:/Users/Alice/Repo"),
            r"c:/Users/Alice/Repo"
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\UNC\fileserver\team\repo"),
            r"\\fileserver\team\repo"
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\unc\fileserver\team\repo"),
            r"\\fileserver\team\repo"
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\Volume{b75e2c83-0000-0000-0000-602f00000000}\"),
            r"\\?\Volume{b75e2c83-0000-0000-0000-602f00000000}\"
        );
    }

    #[test]
    fn normalizes_slashes_case_dot_segments_and_checks_segment_boundaries() {
        let base = normalize_windows_path_key(r"\\?\C:\Users\Alice\Work\..\Repo\");
        let same = normalize_windows_path_key("c:/users/alice/repo/.");
        let child = normalize_windows_path_key(r"c:\USERS\alice\repo\packages/web");
        let sibling = normalize_windows_path_key(r"C:\Users\Alice\Repo-extra");

        assert_eq!(base, r"c:\users\alice\repo");
        assert_eq!(base, same);
        assert!(windows_path_starts_with(&child, &base));
        assert!(!windows_path_starts_with(&sibling, &base));

        let unc_base = normalize_windows_path_key(r"\\?\UNC\Srv\Share\Repo\");
        let unc_child = normalize_windows_path_key("//srv/share/repo/sub");
        let unc_sibling = normalize_windows_path_key(r"\\srv\share\repo2");
        assert_eq!(unc_base, r"\\srv\share\repo");
        assert!(windows_path_starts_with(&unc_child, &unc_base));
        assert!(!windows_path_starts_with(&unc_sibling, &unc_base));
    }

    #[test]
    fn classifies_worktree_overlap_across_verbatim_case_and_slash_variants() {
        assert_eq!(
            classify_windows_overlap(
                r"\\?\C:\Users\Alice\.tiechui\worktrees\repo-1234\wt1",
                "c:/users/alice/.tiechui/worktrees/repo-1234/wt1/"
            ),
            Some(PathOverlap::Equal)
        );
        assert_eq!(
            classify_windows_overlap(
                r"C:\Users\Alice\.tiechui\worktrees\repo-1234\wt1",
                r"\\?\c:\users\alice\.tiechui"
            ),
            Some(PathOverlap::ContainsWorktree)
        );
        assert_eq!(
            classify_windows_overlap(
                r"\\?\C:\Users\Alice\.tiechui\worktrees\repo-1234\wt1",
                "c:/users/alice/.tiechui/worktrees/repo-1234/wt1/packages/app"
            ),
            Some(PathOverlap::InsideWorktree)
        );
        assert_eq!(
            classify_windows_overlap(
                r"C:\Users\Alice\.tiechui\worktrees\repo-1234\wt1",
                r"c:\users\alice\.tiechui\worktrees\repo-1234\wt1-other"
            ),
            None
        );
    }

    #[test]
    fn parses_quoted_windows_command_lines_with_spaces_and_escaped_quotes() {
        let parsed = parse_windows_cmdline(
            r#""C:\Program Files\nodejs\node.exe" --prefix="C:\My Projects\repo\packages\web" "\\?\c:\My Projects\repo\server.js""#,
        );
        assert_eq!(
            parsed,
            vec![
                r"C:\Program Files\nodejs\node.exe",
                r"--prefix=C:\My Projects\repo\packages\web",
                r"\\?\c:\My Projects\repo\server.js",
            ]
        );
    }
}
