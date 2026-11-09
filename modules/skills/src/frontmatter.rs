use std::fs;
use std::path::Path;

use saphyr::{LoadableYamlNode, ScalarOwned, YamlOwned as Value};
use tiechui_plugin_api::{CancellationToken, ToolError};

use crate::{Diagnostic, Root, Skill, check};

pub(crate) fn load(
    path: &Path,
    root: &Root,
    diagnostics: &mut Vec<Diagnostic>,
    cancel: &CancellationToken,
) -> Result<Option<Skill>, ToolError> {
    check(cancel)?;
    let declared = path.file_name().is_some_and(|name| name == "SKILL.md");
    let raw = match fs::read(path) {
        Ok(raw) => raw,
        Err(error) => {
            diagnostics.push(Diagnostic::warning(path, error.to_string()));
            return Ok(None);
        }
    };
    check(cancel)?;
    let content = String::from_utf8_lossy(&raw);
    let content = content
        .strip_prefix('\u{feff}')
        .unwrap_or(&content)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let yaml = if content.starts_with("---") {
        content
            .get(3..)
            .and_then(|rest| rest.find("\n---"))
            .and_then(|end| content.get(4..end + 3))
    } else {
        None
    };
    let fields = match yaml
        .filter(|yaml| !yaml.is_empty())
        // Block scalars clip as if the frontmatter ended with a newline.
        .map(|yaml| {
            Value::load_from_str(&format!("{yaml}\n")).or_else(|original| {
                // Third-party skills sometimes leave prose containing `: ` unquoted.
                // Repair only after a parse failure, and retain the original diagnostic.
                repair_scalars(yaml)
                    .and_then(|repaired| Value::load_from_str(&format!("{repaired}\n")).ok())
                    .ok_or(original)
            })
        })
        .transpose()
    {
        Ok(value) => value
            .and_then(|documents| documents.into_iter().next())
            .unwrap_or(Value::Value(ScalarOwned::Null)),
        Err(error) => {
            if declared {
                diagnostics.push(Diagnostic::warning(path, format!("invalid YAML: {error}")));
            }
            return Ok(None);
        }
    };
    check(cancel)?;
    let description = fields.as_mapping_get("description").and_then(Value::as_str);
    let has_description = description.is_some_and(|value| !js_trim(value).is_empty());
    if !declared && !has_description {
        return Ok(None);
    }
    if !has_description {
        diagnostics.push(Diagnostic::warning(path, "description is required"));
    } else if let Some(description) = description {
        let length = description.encode_utf16().count();
        if length > 1024 {
            diagnostics.push(Diagnostic::warning(
                path,
                format!("description exceeds 1024 characters ({length})"),
            ));
        }
    }
    let fallback = path
        .parent()
        .and_then(Path::file_name)
        .unwrap_or_default()
        .to_string_lossy();
    let name = fields
        .as_mapping_get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(&fallback);
    let length = name.encode_utf16().count();
    if length > 64 {
        diagnostics.push(Diagnostic::warning(
            path,
            format!("name exceeds 64 characters ({length})"),
        ));
    }
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        diagnostics.push(Diagnostic::warning(
            path,
            "name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)",
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        diagnostics.push(Diagnostic::warning(
            path,
            "name must not start or end with a hyphen",
        ));
    }
    if name.contains("--") {
        diagnostics.push(Diagnostic::warning(
            path,
            "name must not contain consecutive hyphens",
        ));
    }
    Ok(description
        .filter(|_| has_description)
        .map(|description| Skill {
            name: name.into(),
            description: description.into(),
            path: path.to_owned(),
            root: root.clone(),
            disable_model_invocation: fields
                .as_mapping_get("disable-model-invocation")
                .and_then(Value::as_bool)
                == Some(true),
        }))
}

// ECMAScript trim excludes NEL but includes BOM; Rust's trim does the reverse.
fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}')
}

/// Codex-compatible fallback for unquoted scalar prose. Never edits the source file.
fn repair_scalars(yaml: &str) -> Option<String> {
    let mut changed = false;
    let mut block_indent = None;
    let mut lines = Vec::new();
    for line in yaml.lines() {
        let indent = line.bytes().take_while(|b| *b == b' ').count();
        if let Some(block) = block_indent {
            if line.trim().is_empty() || indent > block {
                lines.push(line.to_owned());
                continue;
            }
            block_indent = None;
        }
        let Some((key, value)) = line.split_once(':') else {
            lines.push(line.to_owned());
            continue;
        };
        if key.trim().is_empty() || !value.chars().next().is_none_or(char::is_whitespace) {
            lines.push(line.to_owned());
            continue;
        }
        let trimmed = value.trim_start();
        let whitespace = &value[..value.len() - trimmed.len()];
        let mut scalar = trimmed;
        let mut comment = "";
        for (index, ch) in trimmed.char_indices() {
            if ch == '#'
                && (index == 0
                    || trimmed[..index]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_whitespace))
            {
                let start = trimmed[..index].trim_end().len();
                scalar = &trimmed[..start];
                comment = &trimmed[start..];
                break;
            }
        }
        let scalar = scalar.trim_end();
        let first = scalar.chars().next();
        if matches!(first, Some('|' | '>')) {
            block_indent = Some(indent);
        }
        if matches!(first, None | Some('|' | '>' | '\'' | '"')) {
            lines.push(line.to_owned());
            continue;
        }
        let colon = scalar.char_indices().any(|(at, ch)| {
            ch == ':'
                && scalar[at + 1..]
                    .chars()
                    .next()
                    .is_some_and(char::is_whitespace)
        });
        let invalid_flow =
            matches!(first, Some('[' | '{' | '@' | '`')) && Value::load_from_str(scalar).is_err();
        if colon || invalid_flow {
            let quoted = scalar.replace('\'', "''");
            lines.push(format!("{key}:{whitespace}'{quoted}'{comment}"));
            changed = true;
        } else {
            lines.push(line.to_owned());
        }
    }
    changed.then(|| lines.join("\n"))
}
