//! `agents/*`: subagent definitions, Markdown files in the user and project agent
//! directories that each dispatch reads afresh.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tiechui_module_subagents::definitions::{Definition, directories, discover, render};
use tiechui_plugin_api::parse_child_thinking;
use tiechui_protocol::app::*;

use super::prompts::{file_name, project, replace};
use super::{Reply, blocking, parse, value};

pub(super) fn handles(method: &str) -> bool {
    matches!(method, "agents/list" | "agents/save" | "agents/delete")
}

pub(super) async fn handle(home: &Path, method: &str, params: Value) -> Reply {
    let home = home.to_owned();
    match method {
        "agents/save" => {
            let p: AgentSaveParams = parse(params)?;
            blocking(move |_| save(&home, p)).await.and_then(value)
        }
        "agents/delete" => {
            let p: AgentDeleteParams = parse(params)?;
            blocking(move |_| {
                let path = PathBuf::from(p.path);
                scope(&home, &project(&home, p.cwd)?, &path).ok_or("not an agent file")?;
                fs::remove_file(path).map_err(|e| e.to_string())
            })
            .await
            .and_then(value)
        }
        _ => list(home, parse(params)?).await,
    }
}

async fn list(home: PathBuf, p: AgentsListParams) -> Reply {
    let dir = home.join("agents").display().to_string();
    let agents = blocking(move |cancel| {
        let scope = if p.cwd.is_some() { "both" } else { "user" };
        discover(&home, &project(&home, p.cwd)?, scope, cancel).map_err(|e| e.0)
    })
    .await?;
    let agents = agents
        .into_values()
        .map(|agent| AgentEntry {
            source: if agent.project { "project" } else { "user" }.into(),
            path: agent.path.display().to_string(),
            name: agent.name,
            description: agent.description,
            tools: agent.tools,
            model: agent.model,
            thinking: agent.thinking.map(|t| t.as_str().to_owned()),
            background: agent.background,
            prompt: agent.prompt,
        })
        .collect();
    value(AgentList { dir, agents })
}

/// Whether an agent file lies directly in a project (`true`) or user directory.
fn scope(home: &Path, cwd: &Path, path: &Path) -> Option<bool> {
    let md = path.extension().is_some_and(|ext| ext == "md");
    directories(home, cwd)
        .into_iter()
        .find(|(dir, _)| md && path.parent() == Some(dir))
        .map(|(_, project)| project)
}

fn resolve_save_target(
    home: &Path,
    cwd: &Path,
    in_project: bool,
    name: &str,
    previous: Option<&Path>,
) -> Result<(PathBuf, String), String> {
    let base = if in_project {
        cwd.join(".agents/agents")
    } else {
        home.join("agents")
    };
    let mut target = base.join(format!("{name}.md"));
    let Some(prev) = previous else {
        return Ok((target, String::new()));
    };
    let kept = scope(home, cwd, prev).ok_or("not an agent file")?;
    let existing = fs::read_to_string(prev).map_err(|e| e.to_string())?;
    if kept == in_project && prev.file_stem().is_some_and(|stem| stem == name) {
        target.clone_from(&prev.to_path_buf());
    }
    Ok((target, existing))
}

fn save(home: &Path, p: AgentSaveParams) -> Result<(), String> {
    let name = file_name(&p.name)?;
    let thinking = p
        .thinking
        .as_deref()
        .map(parse_child_thinking)
        .transpose()
        .map_err(|e| e.0)?;
    let in_project = match (p.scope.as_str(), &p.cwd) {
        ("user", _) => false,
        ("project", Some(_)) => true,
        _ => return Err("the scope is `user`, or `project` with a cwd".into()),
    };
    let cwd = project(home, p.cwd)?;
    let previous = p.previous.map(PathBuf::from);
    let (target, existing) =
        resolve_save_target(home, &cwd, in_project, name, previous.as_deref())?;
    let definition = Definition {
        name: name.into(),
        description: p.description,
        prompt: p.prompt,
        tools: p.tools.filter(|tools| !tools.is_empty()),
        model: p.model,
        thinking,
        background: p.background,
        project: in_project,
        path: target.clone(),
    };
    replace(
        &target,
        previous.as_deref(),
        &render(&existing, &definition),
        name,
        (!in_project).then_some(home),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn agents_save_and_list_roundtrip_background_and_preserve_frontmatter() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let work = root.path().join("work");
        fs::create_dir_all(home.join("agents")).unwrap();
        fs::create_dir_all(&work).unwrap();

        let existing_path = home.join("agents/bg-worker.md");
        fs::write(
            &existing_path,
            "---\nname: bg-worker\ndescription: initial\npermission:\n  mode: rules\n# preserve comment\n---\nInitial prompt\n",
        )
        .unwrap();
        fs::write(
            home.join("agents/bad-bg.md"),
            "---\nname: bad-bg\ndescription: invalid\nbackground: \"yes\"\n---\nBroken\n",
        )
        .unwrap();

        handle(
            &home,
            "agents/save",
            json!({
                "cwd": work,
                "scope": "user",
                "name": "bg-worker",
                "description": "Runs async",
                "tools": ["read"],
                "model": "alpha/fast",
                "thinking": "low",
                "background": true,
                "prompt": "Background body",
                "previous": existing_path,
            }),
        )
        .await
        .unwrap();

        let text = fs::read_to_string(&existing_path).unwrap();
        assert!(text.contains("background: true\n"), "{text}");
        assert!(text.contains("# preserve comment\n"), "{text}");
        assert!(text.contains("permission:\n  mode: rules\n"), "{text}");

        let listed = handle(&home, "agents/list", json!({"cwd": null}))
            .await
            .unwrap();
        let agents = listed["agents"].as_array().unwrap();
        assert!(agents.iter().all(|a| a["name"] != "bad-bg"));
        let worker = agents.iter().find(|a| a["name"] == "bg-worker").unwrap();
        assert_eq!(worker["background"], json!(true));

        handle(
            &home,
            "agents/save",
            json!({
                "cwd": work,
                "scope": "user",
                "name": "bg-worker",
                "description": "Runs sync",
                "tools": ["read"],
                "model": "alpha/fast",
                "thinking": "low",
                "background": false,
                "prompt": "Background body",
                "previous": existing_path,
            }),
        )
        .await
        .unwrap();
        let listed = handle(&home, "agents/list", json!({"cwd": null}))
            .await
            .unwrap();
        let worker = listed["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "bg-worker")
            .unwrap();
        assert_eq!(worker["background"], json!(false));

        handle(
            &home,
            "agents/save",
            json!({
                "cwd": work,
                "scope": "user",
                "name": "bg-worker",
                "description": "Default mode",
                "tools": ["read"],
                "model": "alpha/fast",
                "thinking": "low",
                "prompt": "Background body",
                "previous": existing_path,
            }),
        )
        .await
        .unwrap();
        let cleared = fs::read_to_string(&existing_path).unwrap();
        assert!(!cleared.contains("background:"), "{cleared}");
        let listed = handle(&home, "agents/list", json!({"cwd": null}))
            .await
            .unwrap();
        let worker = listed["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "bg-worker")
            .unwrap();
        assert!(worker.get("background").is_none(), "{worker}");
    }
}
