use crate::ToolError;
use std::path::{Component, Path, PathBuf};

pub fn resolve_tool_path(input: &str, cwd: &Path) -> Result<PathBuf, ToolError> {
    let input: String = input
        .chars()
        .map(|c| match c {
            '\u{a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => ' ',
            _ => c,
        })
        .collect();
    let input = input.strip_prefix('@').unwrap_or(&input);
    let path = if input == "~" || input.starts_with("~/") {
        tiechui_config::user_home()
            .unwrap_or_default()
            .join(input.strip_prefix("~/").unwrap_or(""))
    } else {
        if input.starts_with("file://") {
            url::Url::parse(input)
                .map_err(|e| ToolError(e.to_string()))?
                .to_file_path()
                .map_err(|_| ToolError("Invalid file URL path".into()))?
        } else {
            PathBuf::from(input)
        }
    };
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    let mut normalized = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            _ => normalized.push(part),
        }
    }
    Ok(normalized)
}
