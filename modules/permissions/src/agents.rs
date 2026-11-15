//! Agent policy sources are fixed by the parent, never by the child's execution cwd.
use super::*;
use saphyr::{LoadableYamlNode, YamlOwned};
use tiechui_plugin_api::PermissionRule;

fn frontmatter_doc(text: &str) -> Result<Option<YamlOwned>, ()> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let Some((header, _)) = text
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---"))
    else {
        return Ok(None);
    };
    let docs = YamlOwned::load_from_str(header).map_err(|_| ())?;
    Ok(docs.into_iter().next())
}

fn declared_name(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let doc = frontmatter_doc(&text).ok()??;
    doc.as_mapping_get("name")
        .and_then(YamlOwned::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

pub(super) fn find_in_dir(directory: &Path, target: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(directory).ok()?;
    let mut paths: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    paths.sort();
    let by_name = paths
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "md") && path.is_file())
        .rfind(|path| declared_name(path).as_deref() == Some(target));
    if by_name.is_some() {
        return by_name;
    }
    let fallback = directory.join(format!("{target}.md"));
    (fallback.is_file() && declared_name(&fallback).is_none()).then_some(fallback)
}

pub(super) fn load(config: &mut ModuleConfig, path: &Path) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            config.permissions.project_invalid = true;
            return;
        }
    };
    let doc = match frontmatter_doc(&text) {
        Ok(Some(doc)) => doc,
        Ok(None) => return,
        Err(()) => {
            config.permissions.project_invalid = true;
            return;
        }
    };
    let Some(permission) = doc
        .as_mapping_get("permission")
        .and_then(YamlOwned::as_mapping)
    else {
        return;
    };
    let mut add = |surface: &str, pattern: &str, value: &YamlOwned| {
        let action = match value.as_str() {
            Some("allow") => Action::Allow,
            Some("ask") => Action::Ask,
            Some("deny") => Action::Deny,
            _ => return,
        };
        let rule = PermissionRule {
            surface: surface.into(),
            pattern: pattern.into(),
            action,
            reason: None,
        };
        if rule.validate().is_ok() {
            config.permissions.project_rules.push(rule);
        }
    };
    for (surface, value) in permission {
        let Some(surface) = surface.as_str() else {
            continue;
        };
        if let Some(patterns) = value.as_mapping() {
            for (pattern, value) in patterns {
                if let Some(pattern) = pattern.as_str() {
                    add(surface, pattern, value);
                }
            }
        } else {
            add(surface, "*", value);
        }
    }
}
