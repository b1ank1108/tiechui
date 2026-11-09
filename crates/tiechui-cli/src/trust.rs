use std::path::{Path, PathBuf};

use clap::ArgMatches;
use tiechui_config::{ConfigPaths, TrustStore};
use tiechui_tui::consent::{Answer, Question, ask};
use tokio_util::sync::CancellationToken;

pub(crate) fn run(args: &ArgMatches) -> Result<(), String> {
    let cwd = args
        .get_one::<String>("cwd")
        .map_or_else(std::env::current_dir, |path| Ok(PathBuf::from(path)))
        .and_then(|path| path.canonicalize())
        .map_err(|e| e.to_string())?;
    let paths = ConfigPaths::discover(&cwd).map_err(|e| e.to_string())?;
    let home = paths.user.parent().ok_or("missing config directory")?;
    let store = TrustStore::new(home);
    let cancel = CancellationToken::new();
    match args
        .get_one::<String>("decision")
        .map(String::as_str)
        .unwrap_or("status")
    {
        "allow" => store.set(&cwd, Some(true), &cancel),
        "deny" => store.set(&cwd, Some(false), &cancel),
        _ => Ok(()),
    }
    .map_err(|e| e.to_string())?;
    let entry = store.get_entry(&cwd, &cancel).map_err(|e| e.to_string())?;
    let mut stdout = std::io::stdout().lock();
    use std::io::Write;
    match entry {
        Some(entry) => writeln!(
            stdout,
            "{}: {} (from {})",
            if entry.decision { "allowed" } else { "denied" },
            cwd.display(),
            entry.path.display()
        ),
        None => writeln!(stdout, "unknown: {}", cwd.display()),
    }
    .map_err(|e| e.to_string())
}

/// What `chui` settles before its first session: trust for a project with no decision yet, then
/// any new or changed hooks. `false` means the user left.
pub(crate) fn consent(cwd: &Path, paths: &ConfigPaths, home: &Path) -> Result<bool, String> {
    let store = TrustStore::new(home);
    let cancel = CancellationToken::new();
    let mut decision = store.get(cwd, &cancel).map_err(|e| e.to_string())?;
    let user_home = tiechui_config::user_home().map(|path| path.canonicalize().unwrap_or(path));
    if decision.is_none()
        && (paths.project.is_some() || has_project_resources(cwd, home))
        && let Some(target) = target(cwd, user_home.as_deref())
    {
        let allow = match ask(&question(cwd, &target)).map_err(|e| e.to_string())? {
            Answer::Choice(0) => true,
            Answer::Choice(1) => false,
            _ => return Ok(false),
        };
        store
            .set(&target, Some(allow), &cancel)
            .map_err(|e| e.to_string())?;
        decision = Some(allow);
    }
    super::hooks::review(cwd, paths, home, decision == Some(true), &cancel)
}

/// Where a decision for `cwd` is saved: its Git root, as project files are found up to there.
/// `None` at the user's home or above it, whose trust every project below would inherit.
fn target(cwd: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let shared = |dir: &Path| home.is_some_and(|home| home.starts_with(dir));
    (!shared(cwd)).then(|| {
        cwd.ancestors()
            .find(|dir| dir.join(".git").exists() && !shared(dir))
            .unwrap_or(cwd)
            .to_owned()
    })
}

fn question(cwd: &Path, target: &Path) -> Question {
    let mut body = vec![
        cwd.display().to_string(),
        String::new(),
        "Project settings, hooks, skills, MCP servers and plugins stay off until you trust this \
         folder. They can run code automatically, so trust it only if you trust its files."
            .to_owned(),
    ];
    if target != cwd {
        body.push(format!(
            "The decision applies to the Git root: {}",
            target.display()
        ));
    }
    body.push("It is saved; /trust changes it.".to_owned());
    Question {
        title: "Trust this folder?".to_owned(),
        body,
        choices: vec![
            "Trust and continue".to_owned(),
            "Continue without project settings".to_owned(),
            "Quit".to_owned(),
        ],
        dismiss: "quit",
        input: None,
        multiple: false,
    }
}

pub(crate) fn has_project_resources(cwd: &Path, home: &Path) -> bool {
    let user_plugins = home.join("plugins");
    let user_plugins = user_plugins.canonicalize().unwrap_or(user_plugins);
    if tiechui_module_plugins::project_locations(cwd)
        .iter()
        .any(|path| {
            path.exists() && path.canonicalize().unwrap_or_else(|_| path.clone()) != user_plugins
        })
    {
        return true;
    }
    let home = home.canonicalize().unwrap_or_else(|_| home.to_owned());
    if [".agents", ".claude"].iter().any(|name| {
        let directory = cwd.join(name);
        let directory = directory.canonicalize().unwrap_or(directory);
        directory != home
            && [
                "config.toml",
                "settings.json",
                "settings.local.json",
                "mcp.toml",
                "mcp.json",
                "skills",
                "hooks",
                "hooks.json",
                "plugins",
                "prompts",
                "commands",
                "agents",
            ]
            .iter()
            .any(|name| directory.join(name).exists())
    }) {
        return true;
    }
    // Everything `trusted` gates loads from here; above lie unrelated dirs like the user's own.
    let root = cwd
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd);
    let dirs = || cwd.ancestors().take_while(|dir| dir.starts_with(root));
    if dirs().any(|dir| {
        dir.join(".codex/hooks.json").is_file()
            || [".agents", ".claude"].iter().any(|name| {
                let directory = dir.join(name);
                directory.join("hooks.json").is_file()
                    || directory.join("scheduled_tasks.json").is_file()
                    || directory.join("settings.json").is_file()
                    || directory.join("settings.local.json").is_file()
                    || directory.join("agents").is_dir()
            })
    }) {
        return true;
    }
    let user_skills: Vec<_> = tiechui_config::user_home()
        .into_iter()
        .flat_map(|home| [".agents/skills", ".claude/skills"].map(|name| home.join(name)))
        .map(|path| path.canonicalize().unwrap_or(path))
        .collect();
    dirs().any(|ancestor| {
        [".agents/skills", ".claude/skills"].iter().any(|name| {
            let skills = ancestor.join(name);
            let skills = skills.canonicalize().unwrap_or(skills);
            !user_skills.contains(&skills) && skills.exists()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_go_to_the_git_root_but_never_to_home_or_above() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let home = root.join("home");
        let repository = home.join("repository");
        let nested = repository.join("crates/app");
        let plain = home.join("plain");
        std::fs::create_dir_all(repository.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        let home_path = Some(home.as_path());
        assert_eq!(target(&nested, home_path), Some(repository.clone()));
        assert_eq!(target(&plain, home_path), Some(plain.clone()));
        assert_eq!(target(&home, home_path), None);
        assert_eq!(target(&root, home_path), None);
        assert_eq!(target(&root, None), Some(root.clone()));
        // A dotfiles repository at home must not stand in for the projects below it.
        std::fs::create_dir(home.join(".git")).unwrap();
        assert_eq!(target(&plain, home_path), Some(plain));
    }

    #[test]
    fn resources_count_only_up_to_the_git_root() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let home = root.join("tiechui");
        let repository = root.join("repository");
        let nested = repository.join("crates/app");
        let plain = root.join("plain");
        std::fs::create_dir_all(repository.join(".git")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::create_dir_all(root.join(".claude/skills")).unwrap();
        std::fs::create_dir_all(root.join(".claude/agents")).unwrap();
        std::fs::write(root.join(".claude/settings.json"), "{}").unwrap();
        assert!(!has_project_resources(&nested, &home));
        assert!(!has_project_resources(&plain, &home));
        std::fs::create_dir_all(repository.join(".claude")).unwrap();
        std::fs::write(repository.join(".claude/settings.json"), "{}").unwrap();
        assert!(has_project_resources(&nested, &home));
        // Durable scheduled prompts run on their own, so they need trust too.
        std::fs::create_dir_all(plain.join(".agents")).unwrap();
        std::fs::write(plain.join(".agents/scheduled_tasks.json"), "{}").unwrap();
        assert!(has_project_resources(&plain, &home));
    }

    #[test]
    fn the_question_names_a_git_root_that_differs_from_the_folder() {
        let same = question(Path::new("/work"), Path::new("/work"));
        assert!(!same.body.iter().any(|line| line.contains("Git root")));
        let nested = question(Path::new("/work/sub"), Path::new("/work"));
        assert!(nested.body.iter().any(|line| line.ends_with("root: /work")));
    }
}
