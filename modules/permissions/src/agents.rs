//! Agent policy sources are fixed by the parent, never by the child's execution cwd.
use super::*;
use saphyr::{LoadableYamlNode, YamlOwned};
use tiechui_plugin_api::PermissionRule;

pub(super) fn load(config: &mut ModuleConfig, path: &Path) {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text.replace("\r\n", "\n"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            config.permissions.project_invalid = true;
            return;
        }
    };
    let Some(header) = text
        .strip_prefix("---\n")
        .and_then(|text| text.split_once("\n---").map(|(header, _)| header))
    else {
        return;
    };
    let docs = match YamlOwned::load_from_str(header) {
        Ok(docs) => docs,
        Err(_) => {
            config.permissions.project_invalid = true;
            return;
        }
    };
    let Some(permission) = docs
        .first()
        .and_then(|d| d.as_mapping_get("permission"))
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
