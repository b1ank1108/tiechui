//! `prompts/*`: the prompt templates a session registers as slash commands, kept as
//! Markdown files in the user and project prompt directories.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tiechui_config::TrustStore;
use tiechui_module_prompts::{PromptsModule, render, roots};
use tiechui_protocol::app::*;

use super::{Reply, blocking, parse, value};

pub(super) fn handles(method: &str) -> bool {
    matches!(method, "prompts/list" | "prompts/save" | "prompts/delete")
}

pub(super) async fn handle(home: &Path, method: &str, params: Value) -> Reply {
    let home = home.to_owned();
    match method {
        "prompts/save" => {
            let p: PromptSaveParams = parse(params)?;
            blocking(move |_| save(&home, p)).await.and_then(value)
        }
        "prompts/delete" => {
            let p: PromptDeleteParams = parse(params)?;
            blocking(move |_| {
                let path = PathBuf::from(p.path);
                source(&project(&home, p.cwd)?, &home, &path).ok_or("not a prompt file")?;
                fs::remove_file(path).map_err(|e| e.to_string())
            })
            .await
            .and_then(value)
        }
        _ => {
            let p: PromptsListParams = parse(params)?;
            let dir = home.join("prompts").display().to_string();
            let module = blocking(move |cancel| {
                let cwd = project(&home, p.cwd)?;
                let trusted = TrustStore::new(&home)
                    .get(&cwd, cancel)
                    .map_err(|e| e.to_string())?
                    == Some(true);
                Ok(PromptsModule::load(&cwd, &home, trusted))
            })
            .await?;
            let prompts = module
                .prompts
                .into_iter()
                .map(|prompt| PromptEntry {
                    name: prompt.name,
                    description: prompt.description,
                    argument_hint: prompt.argument_hint,
                    body: prompt.body,
                    path: prompt.path.display().to_string(),
                    source: prompt.source.into(),
                })
                .collect();
            value(PromptList {
                dir,
                prompts,
                warnings: module.warnings,
            })
        }
    }
}

/// The canonical project directory, or the home for user files alone.
pub(super) fn project(home: &Path, cwd: Option<String>) -> Result<PathBuf, String> {
    cwd.map_or_else(
        || Ok(home.to_owned()),
        |cwd| fs::canonicalize(cwd).map_err(|e| e.to_string()),
    )
}

/// The source of the root holding a prompt file.
fn source(cwd: &Path, home: &Path, path: &Path) -> Option<&'static str> {
    let md = path.extension().is_some_and(|ext| ext == "md");
    roots(cwd, home)
        .into_iter()
        .find(|(root, _)| md && path.parent() == Some(root))
        .map(|(_, source)| source)
}

fn save(home: &Path, p: PromptSaveParams) -> Result<(), String> {
    let name = file_name(&p.name)?;
    let scope = match (p.scope.as_str(), &p.cwd) {
        ("user", _) => "user",
        ("project", Some(_)) => "project",
        _ => return Err("the scope is `user`, or `project` with a cwd".into()),
    };
    let cwd = project(home, p.cwd)?;
    let previous = p.previous.map(PathBuf::from);
    let mut target = roots(&cwd, home)
        .into_iter()
        .find(|(_, source)| *source == scope)
        .map(|(root, _)| root.join(format!("{name}.md")))
        .ok_or("no prompt directory")?;
    if let Some(previous) = &previous {
        let kept = source(&cwd, home, previous).ok_or("not a prompt file")?;
        if kept == scope && previous.file_stem().is_some_and(|stem| stem == name) {
            target.clone_from(previous);
        }
    }
    let text = render(&p.description, p.argument_hint.as_deref(), &p.body);
    replace(&target, previous.as_deref(), &text, &format!("/{name}"))
}

/// A name that becomes `<name>.md`.
pub(super) fn file_name(name: &str) -> Result<&str, String> {
    let name = name.trim();
    if name.is_empty()
        || name.starts_with('.')
        || name.contains(|c: char| c.is_whitespace() || "/\\".contains(c))
    {
        return Err(format!("{name} is not a valid name"));
    }
    Ok(name)
}

/// Writes `target`, then removes `previous` if the file moved; refuses to overwrite another
/// file, which `label` names.
pub(super) fn replace(
    target: &Path,
    previous: Option<&Path>,
    text: &str,
    label: &str,
) -> Result<(), String> {
    if previous != Some(target) && target.exists() {
        return Err(format!("{label} already exists"));
    }
    if let Some(dir) = target.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    fs::write(target, text).map_err(|e| e.to_string())?;
    match previous {
        Some(previous) if previous != target => {
            fs::remove_file(previous).map_err(|e| e.to_string())
        }
        _ => Ok(()),
    }
}
