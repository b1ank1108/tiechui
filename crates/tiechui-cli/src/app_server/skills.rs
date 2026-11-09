//! `skills/*`: the skills a session discovers, switched on and off in the user's
//! `config.toml`.

use std::path::{Path, PathBuf};

use serde_json::Value;
use tiechui_protocol::app::*;

use super::providers::write;
use super::{Reply, blocking, parse, value};

pub(super) fn handles(method: &str) -> bool {
    matches!(method, "skills/list" | "skills/enable")
}

pub(super) async fn handle(home: &Path, method: &str, params: Value) -> Reply {
    if method == "skills/enable" {
        let p: SkillEnableParams = parse(params)?;
        let file = home.join("config.toml");
        return write(move || enable(&file, &p.name, p.enabled)).await;
    }
    let p: SkillsListParams = parse(params)?;
    let cwd = p.cwd.map_or_else(|| home.to_owned(), PathBuf::from);
    let dir = home.join("skills").display().to_string();
    let (loaded, skills) = blocking(move |_| crate::skills::discover(&cwd, &mut vec![])).await?;
    let disabled = loaded.config.skills.disabled;
    let skills = skills
        .skills
        .into_iter()
        .map(|skill| SkillEntry {
            enabled: !disabled.contains(&skill.name),
            name: skill.name,
            description: skill.description,
            path: skill.path.display().to_string(),
            root: skill.root.path.display().to_string(),
            source: skill.root.source.into(),
            manual: skill.disable_model_invocation,
        })
        .collect();
    value(SkillList { dir, skills })
}

fn enable(file: &Path, name: &str, enabled: bool) -> Result<(), String> {
    let mut disabled = crate::setup::user_config(file)?.skills.disabled;
    disabled.retain(|other| other != name);
    if !enabled {
        disabled.push(name.into());
    }
    if disabled.is_empty() {
        tiechui_config::remove_preference(file, "skills.disabled")
    } else {
        let names = disabled.iter().map(String::as_str).collect();
        tiechui_config::save_preference(file, "skills.disabled", names)
    }
    .map_err(|e| e.to_string())
}
