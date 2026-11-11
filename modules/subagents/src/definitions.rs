use saphyr::{LoadableYamlNode, YamlOwned};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tiechui_plugin_api::{CancellationToken, ToolError, parse_child_thinking, serde_json};

#[derive(Clone)]
pub struct Definition {
    pub name: String,
    pub description: String,
    pub prompt: String,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub project: bool,
    pub path: PathBuf,
}

fn parse_thinking(value: Option<&YamlOwned>) -> Option<Option<String>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Some(None);
    };
    let raw = value.as_str()?;
    parse_child_thinking(raw).ok()?;
    Some(Some(raw.to_owned()))
}

fn parse(text: &str, project: bool, path: PathBuf) -> Option<Definition> {
    let text = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let (header, prompt) = text.strip_prefix("---\n")?.split_once("\n---")?;
    let docs = YamlOwned::load_from_str(header).ok()?;
    let doc = docs.first()?;
    let value = |key| doc.as_mapping_get(key);
    let string = |key| value(key).and_then(YamlOwned::as_str).map(str::to_owned);
    let description = string("description")?;
    let tools = value("tools")
        .map(|v| {
            if let Some(s) = v.as_str() {
                s.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            } else {
                v.as_sequence()
                    .into_iter()
                    .flatten()
                    .filter_map(YamlOwned::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            }
        })
        .filter(|v| !v.is_empty());
    Some(Definition {
        name: string("name")?,
        description,
        prompt: prompt.trim().into(),
        tools,
        model: string("model"),
        thinking: parse_thinking(value("thinking"))?,
        project,
        path,
    })
}

pub fn discover(
    home: &Path,
    cwd: &Path,
    scope: &str,
    cancel: &CancellationToken,
) -> Result<BTreeMap<String, Definition>, ToolError> {
    if !matches!(scope, "user" | "project" | "both") {
        return Err(ToolError("Invalid agentScope".into()));
    }
    let mut agents = BTreeMap::new();
    if scope != "project" {
        for text in [
            include_str!("../agents/scout.md"),
            include_str!("../agents/planner.md"),
            include_str!("../agents/reviewer.md"),
            include_str!("../agents/worker.md"),
        ] {
            if let Some(agent) = parse(text, false, PathBuf::new()) {
                agents.insert(agent.name.clone(), agent);
            }
        }
    }
    // User definitions deliberately win over project definitions with the same name.
    for (directory, project) in directories(home, cwd).into_iter().filter(|(_, project)| {
        if *project {
            scope != "user"
        } else {
            scope != "project"
        }
    }) {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        let mut paths = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            if cancel.is_cancelled() {
                return Err(ToolError("Discovery cancelled".into()));
            }
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            if let Some(agent) = parse(&text, project, path) {
                agents.insert(agent.name.clone(), agent);
            }
        }
    }
    Ok(agents)
}

/// Definition directories in load order, flagged when they belong to the project: the
/// nearest ancestor's `.claude/agents` and `.agents/agents`, then `<home>/agents`.
pub fn directories(home: &Path, cwd: &Path) -> Vec<(PathBuf, bool)> {
    let mut directories: Vec<_> = cwd
        .ancestors()
        .find_map(|p| {
            let paths: Vec<_> = [".claude/agents", ".agents/agents"]
                .into_iter()
                .map(|name| (p.join(name), true))
                .filter(|(p, _)| p.is_dir())
                .collect();
            (!paths.is_empty()).then_some(paths)
        })
        .unwrap_or_default();
    directories.push((home.join("agents"), false));
    directories
}

/// A definition file with these fields; frontmatter keys other than `name`, `description`,
/// `tools`, `model` and `thinking` in `existing` are kept.
pub fn render(existing: &str, definition: &Definition) -> String {
    let quote = |value: &str| serde_json::Value::from(value.replace('\n', " ")).to_string();
    let mut text = format!(
        "---\nname: {}\ndescription: {}\n",
        quote(&definition.name),
        quote(&definition.description)
    );
    if let Some(tools) = &definition.tools {
        text.push_str(&format!(
            "tools: {}\n",
            serde_json::Value::from(tools.clone())
        ));
    }
    for (key, value) in [
        ("model", definition.model.as_deref()),
        ("thinking", definition.thinking.as_deref()),
    ] {
        if let Some(value) = value {
            text.push_str(&format!("{key}: {}\n", quote(value)));
        }
    }
    let existing = existing
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n");
    let header = existing
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---"))
        .map_or("", |(header, _)| header);
    let mut kept = true;
    for line in header.lines() {
        // Indented lines and block sequence items continue the key above; comments stay.
        if !line.starts_with([' ', '\t', '-', '#'])
            && let Some((key, _)) = line.split_once(':')
        {
            kept = !["name", "description", "tools", "model", "thinking"].contains(&key.trim());
        }
        if kept || line.starts_with('#') {
            text.push_str(line);
            text.push('\n');
        }
    }
    text.push_str("---\n\n");
    text.push_str(definition.prompt.trim());
    text.push('\n');
    text
}
