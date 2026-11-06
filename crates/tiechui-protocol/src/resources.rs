use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ResourceKind {
    Plugin,
    Skill,
    Task,
}

/// Display metadata and the exact, explicitly selected input reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Resource {
    pub id: String,
    pub kind: ResourceKind,
    pub name: String,
    pub description: String,
    pub path: Option<String>,
    pub owner: Option<String>,
    pub search_terms: Vec<String>,
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default)]
    pub manual: bool,
}

pub const RESOURCE_PICKER_EVENT: &str = "composer.resource_picker";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourcePickerRequest {
    pub kind: ResourceKind,
}

/// Shared lexical boundaries for explicit resource references in ordinary user text.
pub fn reference_start(text: &str, at: usize) -> bool {
    if !text.is_char_boundary(at) || text.trim_start().starts_with('!') {
        return false;
    }
    let before = &text[..at];
    if before.bytes().rev().take_while(|b| *b == b'\\').count() % 2 == 1 {
        return false;
    }
    if before
        .chars()
        .next_back()
        .is_some_and(|c| !c.is_whitespace() && !"([{<'\"=，。！？：；、".contains(c))
    {
        return false;
    }
    let mut code = None;
    let bytes = before.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let ch = bytes[index];
        if ch == b'\\' && code.is_none() {
            index += 2;
            continue;
        }
        if ch != b'`' && ch != b'~' {
            index += 1;
            continue;
        }
        let length = bytes[index..].iter().take_while(|c| **c == ch).count();
        let line_start = before[..index].rfind('\n').map_or(0, |i| i + 1);
        let fence = length >= 3 && before[line_start..index].trim().is_empty();
        match code {
            Some((marker, count, fenced))
                if marker == ch
                    && if fenced {
                        fence && length >= count
                    } else {
                        length == count
                    } =>
            {
                code = None
            }
            None if ch == b'`' || fence => code = Some((ch, length, fence)),
            _ => {}
        }
        index += length;
    }
    code.is_none()
}

pub fn literal_dollar(query: &str) -> bool {
    query.starts_with(['{', '('])
        || matches!(
            query,
            "PATH"
                | "HOME"
                | "USER"
                | "SHELL"
                | "PWD"
                | "TMPDIR"
                | "TEMP"
                | "TMP"
                | "LANG"
                | "TERM"
                | "XDG_CONFIG_HOME"
                | "?"
                | "!"
                | "#"
                | "*"
                | "@"
                | "-"
                | "_"
        )
        || (!query.is_empty()
            && query
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '.' | ',')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_require_ordinary_text_boundaries() {
        for text in [
            "user@file",
            r"\@file",
            "`@file",
            "```rust\n@file",
            "~~~\n@file",
            "```\n`inline`\n@file",
            "! cat @file",
        ] {
            assert!(!reference_start(text, text.rfind('@').unwrap()), "{text}");
        }
        for text in [
            "@file",
            "check (@file)",
            "中文，@file",
            "`code` @file",
            "~~~\ncode\n~~~~\n@file",
            "```\ncode\n````\n@file",
        ] {
            assert!(reference_start(text, text.rfind('@').unwrap()), "{text}");
        }
    }
}
