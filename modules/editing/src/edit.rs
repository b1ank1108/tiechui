use serde_json::{Value, json};
use similar::{ChangeTag, TextDiff};
use tiechui_plugin_api::tools::ToolError;
use unicode_normalization::UnicodeNormalization;

pub(crate) fn lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}
fn fuzzy(text: &str) -> String {
    text.nfkc()
        .collect::<String>()
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .map(|c| match c {
            '\u{2018}'..='\u{201b}' => '\'',
            '\u{201c}'..='\u{201f}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            _ => c,
        })
        .collect()
}
#[derive(Clone)]
struct Replacement {
    index: usize,
    length: usize,
    new: String,
    edit: usize,
}
pub(crate) struct Prepared {
    pub bytes: Vec<u8>,
    pub details: Value,
    pub recovery_notice: String,
}
pub(crate) fn arguments(input: &Value) -> Result<Vec<(String, String)>, ToolError> {
    let mut edits = input["edits"].clone();
    if let Some(text) = edits.as_str() {
        edits = serde_json::from_str(text).unwrap_or(edits);
    }
    if edits.is_object() && edits["oldText"].is_string() && edits["newText"].is_string() {
        edits = json!([edits]);
    }
    let mut edits = edits.as_array().cloned().unwrap_or_default();
    if input["oldText"].is_string() && input["newText"].is_string() {
        edits.push(json!({"oldText":input["oldText"],"newText":input["newText"]}));
    }
    if edits.is_empty() {
        return Err(ToolError(
            "Edit tool input is invalid. edits must contain at least one replacement.".into(),
        ));
    }
    edits
        .iter()
        .map(|e| {
            Ok((
                lf(crate::string(e, "oldText")?),
                lf(crate::string(e, "newText")?),
            ))
        })
        .collect()
}
fn representations(text: &str) -> Vec<(String, &'static str)> {
    let mut result = Vec::new();
    let numbered: Option<Vec<_>> = text
        .strip_suffix('\n')
        .unwrap_or(text)
        .split('\n')
        .map(|line| {
            let digits = line.bytes().take_while(u8::is_ascii_digit).count();
            let n = line[..digits].parse::<u64>().ok()?;
            let rest = line[digits..]
                .strip_prefix('\t')
                .or_else(|| line[digits..].strip_prefix(": "))?;
            Some((n, rest))
        })
        .collect();
    if let Some(lines) = numbered
        && lines
            .windows(2)
            .all(|w| w[0].0.checked_add(1) == Some(w[1].0))
    {
        result.push((
            lines.iter().map(|(_, s)| *s).collect::<Vec<_>>().join("\n")
                + if text.ends_with('\n') { "\n" } else { "" },
            "read_line_prefix",
        ));
    }
    let mut decoded = String::new();
    let mut chars = text.chars().peekable();
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if c != '\\' {
            decoded.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('n' | 'r' | 't' | '\\') => {
                let c = chars.next().unwrap_or_default();
                escaped |= c != '\\';
                decoded.push(match c {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    _ => '\\',
                });
            }
            Some('u') => {
                let mut look = chars.clone();
                look.next();
                let hex: String = look.by_ref().take(4).collect();
                if hex.len() == 4 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    if let Ok(n) = u32::from_str_radix(&hex, 16) {
                        decoded.push(char::from_u32(n).unwrap_or('�'));
                    }
                    chars = look;
                    escaped = true;
                } else {
                    decoded.push('\\');
                }
            }
            _ => decoded.push('\\'),
        }
    }
    if escaped {
        result.push((decoded, "visible_escape"));
    }
    result.retain(|(s, _)| !s.is_empty() && s != text);
    result
}
fn found(content: &str, old: &str) -> bool {
    content.contains(old) || fuzzy(content).contains(&fuzzy(old))
}
fn missing(
    content: &str,
    old: &str,
    path: &str,
    index: usize,
    total: usize,
    views: &[String],
) -> ToolError {
    let mut message = if total == 1 {
        format!(
            "Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        )
    } else {
        format!(
            "Could not find edits[{index}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    };
    message += &mismatch_hint(content, old, path, index);
    let lines: Vec<_> = old.split('\n').collect();
    if old.encode_utf16().count() <= 8192
        && (3..=40).contains(&lines.len())
        && lines[0].trim().encode_utf16().count() >= 12
    {
        let prefix = lines[0].trim();
        if let Some(at) = content.find(prefix)
            && !content[at + prefix.len()..].contains(prefix)
        {
            let start = content[..at].rfind('\n').map_or(0, |i| i + 1);
            let candidate = content[start..]
                .split('\n')
                .take(lines.len())
                .collect::<Vec<_>>()
                .join("\n");
            if candidate.encode_utf16().count() <= 4000
                && candidate.split('\n').next_back().map(str::trim)
                    == lines.last().map(|s| s.trim())
                && views.iter().any(|v| v.contains(&candidate))
            {
                message += &format!(
                    "\nA previously read block has matching boundary lines, but different content. It was NOT applied. Verify this candidate and retry with its exact oldText: {}",
                    json!(candidate)
                );
            }
        }
    }
    ToolError(message)
}
fn mismatch_hint(content: &str, old: &str, path: &str, index: usize) -> String {
    let mut hint = "\nNo replacements from this call were written. Read the current file, copy a smaller unique oldText exactly (including tabs and spaces), and retry edit. Do not reconstruct oldText from memory or use Python/sed to bypass this mismatch.".to_owned();
    let submitted: Vec<_> = old.encode_utf16().collect();
    let prefix = String::from_utf16_lossy(&submitted[..32.min(submitted.len())]);
    if prefix.trim().encode_utf16().count() < 12 {
        return hint;
    }
    let Some(start) = content.find(&prefix) else {
        return hint;
    };
    if content[start + prefix.chars().next().map_or(0, char::len_utf8)..].contains(&prefix) {
        return hint;
    }
    let current: Vec<_> = content[start..].encode_utf16().collect();
    let limit = submitted.len().min(8192);
    let mut offset = prefix.encode_utf16().count();
    while offset < limit && submitted.get(offset) == current.get(offset) {
        offset += 1;
    }
    if offset == limit {
        return hint;
    }
    let line = content[..start].bytes().filter(|&b| b == b'\n').count()
        + current[..offset.min(current.len())]
            .iter()
            .filter(|&&c| c == 10)
            .count()
        + 1;
    let before = offset.saturating_sub(32);
    let a = String::from_utf16_lossy(
        &submitted[before.min(submitted.len())..(offset + 32).min(submitted.len())],
    );
    let b = String::from_utf16_lossy(
        &current[before.min(current.len())..(offset + 32).min(current.len())],
    );
    hint += &format!(
        "\nPossible matching region differs near file line {line} (edits[{index}].oldText offset {offset}, zero-based UTF-16). Escaped excerpts around the first difference:\nSubmitted: {}\nFile:      {}\nRe-read with read {{\"path\":{},\"offset\":{},\"limit\":8}} and verify the intended region before retrying.",
        json!(a),
        json!(b),
        json!(path),
        line.saturating_sub(2).max(1)
    );
    hint
}
fn replace(content: &str, replacements: &[Replacement], offset: usize, eol: &str) -> String {
    let mut result = content.to_owned();
    for r in replacements.iter().rev() {
        result.replace_range(
            r.index - offset..r.index - offset + r.length,
            &r.new.replace('\n', eol),
        );
    }
    result
}
fn spans(content: &str) -> Vec<(usize, usize)> {
    let mut at = 0;
    content
        .split_inclusive('\n')
        .map(|line| {
            let start = at;
            at += line.len();
            (start, at)
        })
        .collect()
}
fn overlay(
    original: &[String],
    base: &str,
    replacements: &[Replacement],
    eol: &str,
    path: &str,
) -> Result<String, ToolError> {
    let spans = spans(base);
    if original.len() != spans.len() {
        return Err(ToolError(format!(
            "Cannot preserve line endings in {path}; the file was not changed."
        )));
    }
    let mut groups: Vec<(usize, usize, Vec<Replacement>)> = Vec::new();
    for r in replacements {
        let first = spans
            .iter()
            .position(|&(a, b)| r.index >= a && r.index < b)
            .ok_or_else(|| {
                ToolError(format!(
                    "Cannot preserve edit range in {path}; the file was not changed."
                ))
            })?;
        let last = spans
            .iter()
            .position(|&(_, b)| b >= r.index + r.length)
            .unwrap_or(first);
        if let Some(group) = groups.last_mut()
            && first <= group.1
        {
            group.1 = group.1.max(last);
            group.2.push(r.clone());
        } else {
            groups.push((first, last, vec![r.clone()]));
        }
    }
    let mut result = String::new();
    let mut cursor = 0;
    for (first, last, replacements) in groups {
        for line in &original[cursor..first] {
            result += line;
        }
        let start = spans[first].0;
        result +=
            &replace(&base[start..spans[last].1], &replacements, start, "\n").replace('\n', eol);
        cursor = last + 1;
    }
    for line in &original[cursor..] {
        result += line;
    }
    Ok(result)
}
pub(crate) fn prepare(
    bytes: &[u8],
    mut edits: Vec<(String, String)>,
    path: &str,
    views: &[String],
) -> Result<Prepared, ToolError> {
    let raw = std::str::from_utf8(bytes)
        .ok()
        .filter(|s| !s.contains('\0'))
        .ok_or_else(|| {
            ToolError(format!(
                "Cannot losslessly edit {path}: expected UTF-8 text. The file was not changed."
            ))
        })?;
    let content = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let bom = if raw.starts_with('\u{feff}') {
        "\u{feff}"
    } else {
        ""
    };
    let normalized = lf(content);
    let mut recoveries = Vec::new();
    for (i, (old, _)) in edits.iter_mut().enumerate() {
        if !old.is_empty() && !found(&normalized, old) {
            let candidates: Vec<_> = representations(old)
                .into_iter()
                .filter(|(s, _)| {
                    normalized.matches(s).count() == 1 && views.iter().any(|v| v.contains(s))
                })
                .collect();
            if let Some((text, strategy)) = candidates.first()
                && candidates.iter().all(|(s, _)| s == text)
            {
                *old = text.clone();
                recoveries.push(json!({"editIndex":i,"strategy":strategy}));
            }
        }
    }
    for (i, (old, _)) in edits.iter().enumerate() {
        if old.is_empty() {
            return Err(ToolError(if edits.len() == 1 {
                format!("oldText must not be empty in {path}.")
            } else {
                format!("edits[{i}].oldText must not be empty in {path}.")
            }));
        }
    }
    let used_fuzzy = edits
        .iter()
        .any(|(old, _)| !normalized.contains(old) && fuzzy(&normalized).contains(&fuzzy(old)));
    let base = if used_fuzzy {
        fuzzy(&normalized)
    } else {
        normalized.clone()
    };
    let mut replacements = Vec::new();
    for (i, (old, new)) in edits.iter().enumerate() {
        let needle = if base.contains(old) {
            old.clone()
        } else {
            fuzzy(old)
        };
        let Some(index) = base.find(&needle) else {
            return Err(missing(&normalized, old, path, i, edits.len(), views));
        };
        let occurrences = fuzzy(&base).matches(&fuzzy(old)).count();
        if occurrences > 1 {
            return Err(ToolError(if edits.len() == 1 {
                format!(
                    "Found {occurrences} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
                )
            } else {
                format!(
                    "Found {occurrences} occurrences of edits[{i}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
                )
            }));
        }
        replacements.push(Replacement {
            index,
            length: needle.len(),
            new: new.clone(),
            edit: i,
        });
    }
    replacements.sort_by_key(|r| r.index);
    for pair in replacements.windows(2) {
        if pair[0].index + pair[0].length > pair[1].index {
            return Err(ToolError(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                pair[0].edit, pair[1].edit
            )));
        }
    }
    let new_content = if used_fuzzy {
        overlay(
            &normalized
                .split_inclusive('\n')
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            &base,
            &replacements,
            "\n",
            path,
        )?
    } else {
        replace(&base, &replacements, 0, "\n")
    };
    if new_content == normalized {
        return Err(ToolError(if edits.len() == 1 {
            format!(
                "No changes made to {path}. The replacement produced identical content. This might indicate an issue with special characters or the text not existing as expected."
            )
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        }));
    }
    let ending = if content
        .find('\n')
        .is_some_and(|i| i > 0 && content.as_bytes()[i - 1] == b'\r')
    {
        "\r\n"
    } else {
        "\n"
    };
    let final_text = if !content.contains('\r') {
        new_content.clone()
    } else if used_fuzzy {
        overlay(&raw_lines(content), &base, &replacements, ending, path)?
    } else {
        let mut map = Vec::new();
        let mut raw_at = 0;
        for c in content.chars() {
            if c == '\n' && raw_at > 0 && content.as_bytes()[raw_at - 1] == b'\r' {
                raw_at += 1;
                continue;
            }
            for i in 0..c.len_utf8() {
                map.push(raw_at + i);
            }
            raw_at += c.len_utf8();
        }
        map.push(content.len());
        let mut out = content.to_owned();
        for r in replacements.iter().rev() {
            out.replace_range(
                map[r.index]..map[r.index + r.length],
                &r.new.replace('\n', ending),
            );
        }
        out
    };
    let mut details = diff_details(path, &normalized, &new_content);
    let recovery_notice = if recoveries.is_empty() {
        String::new()
    } else {
        details["recoveries"] = json!(recoveries);
        format!(
            " Recovered oldText representation: {}. Replacement text was used literally.",
            recoveries
                .iter()
                .map(|r| format!(
                    "edits[{}] {}",
                    r["editIndex"],
                    r["strategy"].as_str().unwrap_or_default()
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Ok(Prepared {
        bytes: (bom.to_owned() + &final_text).into_bytes(),
        details,
        recovery_notice,
    })
}
fn raw_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = text.char_indices().peekable();
    while let Some((at, c)) = it.next() {
        if c == '\r' || c == '\n' {
            let mut end = at + c.len_utf8();
            if c == '\r' && it.peek().is_some_and(|&(_, c)| c == '\n') {
                it.next();
                end += 1;
            }
            out.push(text[start..end].into());
            start = end;
        }
    }
    if start < text.len() {
        out.push(text[start..].into());
    }
    out
}
fn diff_details(path: &str, old: &str, new: &str) -> Value {
    let diff = TextDiff::from_lines(old, new);
    let width = old
        .split('\n')
        .count()
        .max(new.split('\n').count())
        .to_string()
        .len();
    let mut display = Vec::new();
    let mut first = None;
    let changes: Vec<_> = diff.iter_all_changes().collect();
    let (mut old_line, mut new_line) = (1, 1);
    let mut at = 0;
    while at < changes.len() {
        if changes[at].tag() != ChangeTag::Equal {
            first.get_or_insert(new_line);
            let change = &changes[at];
            let (sign, line) = if change.tag() == ChangeTag::Delete {
                ('-', old_line)
            } else {
                ('+', new_line)
            };
            display.push(format!(
                "{sign}{line:>width$} {}",
                change.value().strip_suffix('\n').unwrap_or(change.value())
            ));
            if sign == '-' {
                old_line += 1;
            } else {
                new_line += 1;
            }
            at += 1;
        } else {
            let start = at;
            while at < changes.len() && changes[at].tag() == ChangeTag::Equal {
                at += 1;
            }
            let n = at - start;
            let leading = start > 0;
            let trailing = at < changes.len();
            let mut omitted = false;
            for i in 0..n {
                if (leading && i < 4) || (trailing && i >= n.saturating_sub(4)) {
                    display.push(format!(
                        " {old_line:>width$} {}",
                        changes[start + i]
                            .value()
                            .strip_suffix('\n')
                            .unwrap_or(changes[start + i].value())
                    ));
                    omitted = false;
                } else if (leading || trailing) && !omitted {
                    display.push(format!(" {:>width$} ...", ""));
                    omitted = true;
                }
                old_line += 1;
                new_line += 1;
            }
        }
    }
    let mut patch = diff
        .unified_diff()
        .context_radius(4)
        .header(path, path)
        .to_string();
    // jsdiff always writes both counts, including a one-line hunk.
    let mut lines = Vec::new();
    let raw_lines: Vec<_> = patch.split_inclusive('\n').collect();
    for (i, line) in raw_lines.iter().enumerate() {
        if line.starts_with("@@ ") {
            let parts: Vec<_> = line.split(' ').collect();
            let body: Vec<_> = raw_lines[i + 1..]
                .iter()
                .take_while(|line| !line.starts_with("@@ "))
                .collect();
            let old_count = body
                .iter()
                .filter(|line| line.starts_with(' ') || line.starts_with('-'))
                .count();
            let new_count = body
                .iter()
                .filter(|line| line.starts_with(' ') || line.starts_with('+'))
                .count();
            let range = |s: &str, count| format!("{},{count}", s.split(',').next().unwrap_or(s));
            lines.push(format!(
                "@@ {} {} @@\n",
                range(parts[1], old_count),
                range(parts[2], new_count)
            ));
        } else {
            lines.push((*line).into());
        }
    }
    patch = lines.join("");
    json!({"diff":display.join("\n"),"patch":patch,"firstChangedLine":first})
}
