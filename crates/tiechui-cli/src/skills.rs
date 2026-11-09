use std::io::Write;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use tiechui_config::{ConfigPaths, LoadedConfig, TrustStore};
use tiechui_module_skills::SkillsModule;
use tiechui_plugin_api::{CancellationToken, Registry};

pub(crate) fn load(
    cwd: &Path,
    home: &Path,
    trusted: bool,
    registry: &Registry,
    cancel: &CancellationToken,
) -> Result<SkillsModule, String> {
    let user_home =
        tiechui_config::user_home().ok_or("set HOME or USERPROFILE for user skill discovery")?;
    SkillsModule::load(
        cwd,
        &user_home,
        home,
        trusted,
        registry.skill_roots(),
        cancel,
    )
    .map_err(|error| error.to_string())
}

/// The configuration and skills a session in `cwd` loads; plugin warnings go to `warnings`.
pub(crate) fn discover(
    cwd: &Path,
    warnings: &mut Vec<String>,
) -> Result<(LoadedConfig, SkillsModule), String> {
    let cwd = cwd.canonicalize().map_err(|error| error.to_string())?;
    let paths = ConfigPaths::discover(&cwd).map_err(|error| error.to_string())?;
    let home = paths.user.parent().ok_or("missing config directory")?;
    let cancel = CancellationToken::new();
    let trusted = TrustStore::new(home)
        .get(&cwd, &cancel)
        .map_err(|error| error.to_string())?
        == Some(true);
    let loaded = tiechui_config::load(&paths, trusted, &toml::Table::new())
        .map_err(|error| error.to_string())?;
    let registry =
        super::plugins::registry(&cwd, home, trusted, &loaded.config, &cancel, warnings)?;
    let skills = load(&cwd, home, trusted, &registry, &cancel)?;
    Ok((loaded, skills))
}

pub(crate) fn list(args: &ArgMatches) -> Result<(), String> {
    let cwd = args
        .get_one::<String>("cwd")
        .map_or_else(std::env::current_dir, |path| Ok(PathBuf::from(path)))
        .map_err(|error| error.to_string())?;
    let mut warnings = Vec::new();
    let discovered = discover(&cwd, &mut warnings);
    for warning in warnings {
        eprintln!("{warning}");
    }
    let (loaded, skills) = discovered?;
    let disabled = &loaded.config.skills.disabled;
    let mut stdout = std::io::stdout().lock();
    if args.get_flag("json") {
        let mut output = skills.to_json();
        for skill in output["skills"].as_array_mut().into_iter().flatten() {
            let off = disabled.iter().any(|name| skill["name"] == name.as_str());
            skill["disabled"] = off.into();
        }
        output["config_diagnostics"] = serde_json::json!(loaded.diagnostics.iter().map(|diagnostic| {
            serde_json::json!({"path": diagnostic.file, "key": diagnostic.key, "reason": format!("{:?}", diagnostic.reason)})
        }).collect::<Vec<_>>());
        serde_json::to_writer_pretty(&mut stdout, &output).map_err(|error| error.to_string())?;
        writeln!(stdout).map_err(|error| error.to_string())?;
    } else {
        for skill in &skills.skills {
            writeln!(
                stdout,
                "{}\t{}\n  path: {}\n  root: {} ({}){}{}",
                skill.name,
                skill.description,
                skill.path.display(),
                skill.root.path.display(),
                skill.root.source,
                if skill.disable_model_invocation {
                    "\n  disable-model-invocation: true"
                } else {
                    ""
                },
                if disabled.contains(&skill.name) {
                    "\n  disabled: true"
                } else {
                    ""
                }
            )
            .map_err(|error| error.to_string())?;
        }
        for diagnostic in &skills.diagnostics {
            writeln!(
                stdout,
                "{}: {}: {}",
                diagnostic.kind,
                diagnostic.path.display(),
                diagnostic.message
            )
            .map_err(|error| error.to_string())?;
        }
        for diagnostic in loaded.diagnostics {
            writeln!(
                stdout,
                "config: {}: {}: {:?}",
                diagnostic.file.display(),
                diagnostic.key,
                diagnostic.reason
            )
            .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}
