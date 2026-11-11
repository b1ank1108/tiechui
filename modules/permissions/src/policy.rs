use std::path::Path;

use tiechui_plugin_api::{PermissionAction as Action, PermissionRule, PermissionsConfig};

#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub surface: String,
    pub pattern: String,
    pub action: Action,
    pub reason: Option<String>,
    pub origin: &'static str,
}

impl Rule {
    pub fn new(surface: &str, pattern: &str, action: Action, origin: &'static str) -> Self {
        Self {
            surface: surface.into(),
            pattern: pattern.into(),
            action,
            reason: None,
            origin,
        }
    }
}

/// Tools that only read or stay within the session; the rest ask unless rules allow them.
const SAFE_TOOLS: [&str; 24] = [
    "read",
    "grep",
    "find",
    "ls",
    "create_goal",
    "get_goal",
    "update_goal",
    "context",
    "tool_search",
    "request_user_input",
    "context_status",
    "compact_history",
    "search_history",
    "read_history",
    "skills",
    "cron_create",
    "cron_list",
    "cron_delete",
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
    "shell_list",
    "shell_output",
    "shell_stop",
];
const MCP_METADATA: [&str; 5] = [
    "mcp_status",
    "mcp_list",
    "mcp_search",
    "mcp_describe",
    "mcp_connect",
];

pub(crate) fn compose(config: &PermissionsConfig) -> Vec<Rule> {
    let allow = |surface: &str, pattern: &str| PermissionRule {
        surface: surface.into(),
        pattern: pattern.into(),
        action: Action::Allow,
        reason: None,
    };
    let defaults: Vec<_> = SAFE_TOOLS
        .iter()
        .map(|tool| allow(tool, "*"))
        .chain(MCP_METADATA.iter().map(|pattern| allow("mcp", pattern)))
        .collect();
    let mut rules = vec![Rule::new("*", "*", Action::Ask, "builtin")];
    for (scope, origin) in [
        match &config.rules {
            Some(rules) => (rules.as_slice(), "user"),
            None => (defaults.as_slice(), "default"),
        },
        (config.project_rules.as_slice(), "project"),
    ] {
        // Each scope flattens into ordered surface maps before merging.
        let mut surfaces: Vec<(String, Vec<Rule>)> = Vec::new();
        for source in scope {
            if source.surface == "*" {
                rules[0] = Rule::new("*", "*", source.action, origin);
                continue;
            }
            if !surfaces.iter().any(|(name, _)| name == &source.surface) {
                surfaces.push((source.surface.clone(), Vec::new()));
            }
            if let Some((_, entries)) = surfaces
                .iter_mut()
                .find(|(name, _)| name == &source.surface)
            {
                entries.retain(|rule| rule.pattern != source.pattern);
                entries.push(Rule {
                    surface: source.surface.clone(),
                    pattern: source.pattern.clone(),
                    action: source.action,
                    reason: source.reason.clone(),
                    origin,
                });
            }
        }
        let mut expanded = Vec::new();
        for (surface, entries) in &surfaces {
            if matches!(surface.as_str(), "path" | "external_directory") {
                for direction in ["read", "write"] {
                    let name = format!("{surface}_{direction}");
                    let explicit = surfaces.iter().find(|(s, _)| s == &name);
                    for rule in entries {
                        if explicit.is_some_and(|(_, entries)| {
                            entries.iter().any(|r| r.pattern == rule.pattern)
                        }) {
                            continue;
                        }
                        let mut rule = rule.clone();
                        rule.surface = name.clone();
                        expanded.push(rule);
                    }
                    if let Some((_, entries)) = explicit {
                        expanded.extend(entries.clone());
                    }
                }
            } else if ![
                "path_read",
                "path_write",
                "external_directory_read",
                "external_directory_write",
            ]
            .contains(&surface.as_str())
                || !surfaces.iter().any(|(name, _)| {
                    Some(name.as_str()) == surface.rsplit_once('_').map(|(name, _)| name)
                })
            {
                expanded.extend(entries.clone());
            }
        }
        for rule in expanded {
            if let Some(existing) = rules
                .iter_mut()
                .find(|r| r.surface == rule.surface && r.pattern == rule.pattern)
            {
                *existing = rule;
            } else {
                let at = rules
                    .iter()
                    .rposition(|r| r.surface == rule.surface)
                    .map_or(rules.len(), |at| at + 1);
                rules.insert(at, rule);
            }
        }
    }
    let relocated: Vec<_> = rules
        .iter()
        .filter(|r| r.surface.starts_with("mcp__") && r.pattern == "*")
        .map(|r| Rule {
            surface: "mcp".into(),
            pattern: r.surface.clone(),
            ..r.clone()
        })
        .collect();
    rules.extend(relocated);
    if rules
        .iter()
        .any(|r| r.surface == "mcp" && r.action == Action::Allow)
    {
        for pattern in MCP_METADATA {
            if !rules
                .iter()
                .any(|r| r.surface == "mcp" && r.pattern == pattern)
            {
                rules.insert(1, Rule::new("mcp", pattern, Action::Allow, "baseline"));
            }
        }
    }
    if config.project_invalid {
        for rule in &mut rules {
            if rule.action != Action::Deny {
                rule.action = Action::Ask;
                rule.origin = "fail-closed";
            }
        }
    }
    rules
}

pub(crate) fn evaluate(surface: &str, values: &[String], rules: &[Rule], home: &Path) -> Rule {
    rules
        .iter()
        .rev()
        .find(|rule| {
            wildcard(&rule.surface, surface)
                && values.iter().any(|value| {
                    let pattern = expand_home(&rule.pattern, home);
                    if cfg!(windows) && is_path(surface) {
                        wildcard(
                            &pattern.replace('/', "\\").to_lowercase(),
                            &value.replace('/', "\\").to_lowercase(),
                        )
                    } else {
                        wildcard(&pattern, value)
                    }
                })
        })
        .cloned()
        .unwrap_or_else(|| Rule::new(surface, "*", Action::Ask, "builtin"))
}

fn is_path(surface: &str) -> bool {
    matches!(surface, "read" | "write" | "edit" | "find" | "grep" | "ls")
        || surface.starts_with("path_")
        || surface.starts_with("external_directory_")
}

pub(crate) fn expand_home(value: &str, home: &Path) -> String {
    for prefix in ["~", "$HOME", "${HOME}"] {
        if value == prefix {
            return home.to_string_lossy().into_owned();
        }
        if let Some(suffix) = value.strip_prefix(&format!("{prefix}/")) {
            return home.join(suffix).to_string_lossy().into_owned();
        }
    }
    value.into()
}

pub(crate) fn wildcard(pattern: &str, value: &str) -> bool {
    if let Some(base) = pattern.strip_suffix(" *")
        && wildcard(base, value)
    {
        return true;
    }
    let pattern: Vec<_> = pattern.chars().collect();
    let value: Vec<_> = value.chars().collect();
    let (mut p, mut v, mut star, mut retry) = (0, 0, None, 0);
    while v < value.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == value[v]) {
            p += 1;
            v += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            p += 1;
            retry = v;
        } else if let Some(at) = star {
            retry += 1;
            v = retry;
            p = at + 1;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == '*' {
        p += 1;
    }
    p == pattern.len()
}
