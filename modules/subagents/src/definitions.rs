use saphyr::{LoadableYamlNode, YamlOwned};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use tiechui_plugin_api::{
    CancellationToken, ChildThinking, ToolError, parse_child_thinking, serde_json,
};

const MANAGED_KEYS: [&str; 6] = [
    "name",
    "description",
    "tools",
    "model",
    "thinking",
    "background",
];

#[derive(Clone)]
pub struct Definition {
    pub name: String,
    pub description: String,
    pub prompt: String,
    pub tools: Option<Vec<String>>,
    pub model: Option<String>,
    pub thinking: Option<ChildThinking>,
    pub background: Option<bool>,
    pub project: bool,
    pub path: PathBuf,
}

#[derive(Default)]
pub struct Catalog {
    pub agents: BTreeMap<String, Definition>,
    pub invalid: BTreeMap<String, (PathBuf, String)>,
}

struct ParseFailure {
    name: Option<String>,
    reason: String,
}

fn parse_thinking(value: Option<&YamlOwned>) -> Result<Option<ChildThinking>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let Some(raw) = value.as_str() else {
        return Err("invalid `thinking`: expected a string".into());
    };
    parse_child_thinking(raw)
        .map(Some)
        .map_err(|error| format!("invalid `thinking`: {}", error.0))
}

fn parse_background(value: Option<&YamlOwned>) -> Result<Option<bool>, String> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| "invalid `background`: expected a boolean".into())
}

fn parse_optional_string(doc: &YamlOwned, key: &str) -> Result<Option<String>, String> {
    let Some(value) = doc.as_mapping_get(key).filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    value
        .as_str()
        .map(|raw| Some(raw.to_owned()))
        .ok_or_else(|| format!("invalid `{key}`: expected a string"))
}

fn parse_required_string(doc: &YamlOwned, key: &str) -> Result<String, String> {
    doc.as_mapping_get(key)
        .and_then(YamlOwned::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("missing or invalid `{key}`"))
}

fn parse_tools(value: Option<&YamlOwned>) -> Option<Vec<String>> {
    let v = value.filter(|v| !v.is_null())?;
    let items = if let Some(s) = v.as_str() {
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
    };
    (!items.is_empty()).then_some(items)
}

fn extract_frontmatter(text: &str) -> Result<(YamlOwned, String), ParseFailure> {
    let fail = |reason: &str| ParseFailure {
        name: None,
        reason: reason.into(),
    };
    let normalized = text.trim_start_matches('\u{feff}').replace("\r\n", "\n");
    let Some((header, prompt)) = normalized
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---"))
    else {
        return Err(fail("missing YAML frontmatter"));
    };
    let docs = YamlOwned::load_from_str(header).map_err(|_| fail("invalid YAML frontmatter"))?;
    let doc = docs
        .into_iter()
        .next()
        .ok_or_else(|| fail("empty YAML frontmatter"))?;
    Ok((doc, prompt.trim().to_owned()))
}

fn parse(text: &str, project: bool, path: PathBuf) -> Result<Definition, ParseFailure> {
    let (doc, prompt) = extract_frontmatter(text)?;
    let name = parse_required_string(&doc, "name")
        .map_err(|reason| ParseFailure { name: None, reason })?;
    let with_name = |reason: String| ParseFailure {
        name: Some(name.clone()),
        reason,
    };
    let description = parse_required_string(&doc, "description").map_err(with_name)?;
    let model = parse_optional_string(&doc, "model").map_err(with_name)?;
    let thinking = parse_thinking(doc.as_mapping_get("thinking")).map_err(with_name)?;
    let background = parse_background(doc.as_mapping_get("background")).map_err(with_name)?;
    Ok(Definition {
        name,
        description,
        prompt,
        tools: parse_tools(doc.as_mapping_get("tools")),
        model,
        thinking,
        background,
        project,
        path,
    })
}

fn record_parsed(catalog: &mut Catalog, path: PathBuf, parsed: Result<Definition, ParseFailure>) {
    match parsed {
        Ok(agent) => {
            catalog.invalid.remove(&agent.name);
            catalog.agents.insert(agent.name.clone(), agent);
        }
        Err(failure) => {
            if let Some(name) = failure.name {
                catalog.agents.remove(&name);
                catalog.invalid.insert(name, (path, failure.reason));
            } else if let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                && !catalog.agents.contains_key(stem)
            {
                catalog
                    .invalid
                    .insert(stem.to_owned(), (path, failure.reason));
            }
        }
    }
}

fn load_directory(
    directory: PathBuf,
    project: bool,
    cancel: &CancellationToken,
    catalog: &mut Catalog,
) -> Result<(), ToolError> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Ok(());
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
        if let Ok(text) = std::fs::read_to_string(&path) {
            let parsed = parse(&text, project, path.clone());
            record_parsed(catalog, path, parsed);
        }
    }
    Ok(())
}

pub fn discover_catalog(
    home: &Path,
    cwd: &Path,
    scope: &str,
    cancel: &CancellationToken,
) -> Result<Catalog, ToolError> {
    if !matches!(scope, "user" | "project" | "both") {
        return Err(ToolError("Invalid agentScope".into()));
    }
    let mut catalog = Catalog::default();
    // User definitions deliberately win over project definitions with the same name.
    for (directory, project) in directories(home, cwd) {
        let included = if project {
            scope != "user"
        } else {
            scope != "project"
        };
        if included {
            load_directory(directory, project, cancel, &mut catalog)?;
        }
    }
    Ok(catalog)
}

pub fn discover(
    home: &Path,
    cwd: &Path,
    scope: &str,
    cancel: &CancellationToken,
) -> Result<BTreeMap<String, Definition>, ToolError> {
    discover_catalog(home, cwd, scope, cancel).map(|catalog| catalog.agents)
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

fn retained_frontmatter(existing: &str) -> String {
    let normalized = existing
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n");
    let header = normalized
        .strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---"))
        .map_or("", |(header, _)| header);
    let mut out = String::new();
    let mut kept = true;
    for line in header.lines() {
        // Indented lines and block sequence items continue the key above; comments stay.
        if !line.starts_with([' ', '\t', '-', '#'])
            && let Some((key, _)) = line.split_once(':')
        {
            kept = !MANAGED_KEYS.contains(&key.trim());
        }
        if kept || line.starts_with('#') {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// A definition file with these fields; frontmatter keys other than `name`, `description`,
/// `tools`, `model`, `thinking` and `background` in `existing` are kept.
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
        ("thinking", definition.thinking.map(ChildThinking::as_str)),
    ] {
        if let Some(value) = value {
            text.push_str(&format!("{key}: {}\n", quote(value)));
        }
    }
    if let Some(background) = definition.background {
        text.push_str(&format!("background: {background}\n"));
    }
    text.push_str(&retained_frontmatter(existing));
    text.push_str("---\n\n");
    text.push_str(definition.prompt.trim());
    text.push('\n');
    text
}
