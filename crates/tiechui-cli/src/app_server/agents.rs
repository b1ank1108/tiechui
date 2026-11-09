//! `agents/*`: subagent definitions, Markdown files in the user and project agent
//! directories that each dispatch reads afresh.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use tiechui_module_subagents::definitions::{Definition, directories, discover, render};
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
        _ => {
            let p: AgentsListParams = parse(params)?;
            let dir = home.join("agents").display().to_string();
            let agents = blocking(move |cancel| {
                let scope = if p.cwd.is_some() { "both" } else { "user" };
                discover(&home, &project(&home, p.cwd)?, scope, cancel).map_err(|e| e.0)
            })
            .await?;
            let mut agents: Vec<_> = agents
                .into_values()
                .map(|agent| AgentEntry {
                    source: match (agent.project, agent.path.as_os_str().is_empty()) {
                        (true, _) => "project",
                        (false, true) => "builtin",
                        (false, false) => "user",
                    }
                    .into(),
                    path: (!agent.path.as_os_str().is_empty())
                        .then(|| agent.path.display().to_string()),
                    name: agent.name,
                    description: agent.description,
                    tools: agent.tools,
                    model: agent.model,
                    prompt: agent.prompt,
                })
                .collect();
            agents.sort_by_key(|agent| agent.source != "builtin");
            value(AgentList { dir, agents })
        }
    }
}

/// Whether an agent file lies directly in a project (`true`) or user directory.
fn scope(home: &Path, cwd: &Path, path: &Path) -> Option<bool> {
    let md = path.extension().is_some_and(|ext| ext == "md");
    directories(home, cwd)
        .into_iter()
        .find(|(dir, _)| md && path.parent() == Some(dir))
        .map(|(_, project)| project)
}

fn save(home: &Path, p: AgentSaveParams) -> Result<(), String> {
    let name = file_name(&p.name)?;
    let in_project = match (p.scope.as_str(), &p.cwd) {
        ("user", _) => false,
        ("project", Some(_)) => true,
        _ => return Err("the scope is `user`, or `project` with a cwd".into()),
    };
    let cwd = project(home, p.cwd)?;
    let previous = p.previous.map(PathBuf::from);
    let mut target = if in_project {
        cwd.join(".agents/agents")
    } else {
        home.join("agents")
    }
    .join(format!("{name}.md"));
    let mut existing = String::new();
    if let Some(previous) = &previous {
        let kept = scope(home, &cwd, previous).ok_or("not an agent file")?;
        existing = fs::read_to_string(previous).map_err(|e| e.to_string())?;
        if kept == in_project && previous.file_stem().is_some_and(|stem| stem == name) {
            target.clone_from(previous);
        }
    }
    let definition = Definition {
        name: name.into(),
        description: p.description,
        prompt: p.prompt,
        tools: p.tools.filter(|tools| !tools.is_empty()),
        model: p.model,
        project: in_project,
        path: target.clone(),
    };
    replace(
        &target,
        previous.as_deref(),
        &render(&existing, &definition),
        name,
    )
}
