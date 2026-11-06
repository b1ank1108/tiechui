use std::io::Write;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use tiechui_config::{ConfigPaths, TrustStore};
use tiechui_module_hooks::{Hook, HooksModule};
use tiechui_tui::consent::{Answer, Question, ask};
use tokio_util::sync::CancellationToken;

/// How many handlers the startup review lists, so it fits a small terminal.
const REVIEWED: usize = 6;

pub(crate) fn run(command: &str, args: &ArgMatches) -> Result<(), String> {
    let cwd = args
        .get_one::<String>("cwd")
        .map_or_else(std::env::current_dir, |path| Ok(PathBuf::from(path)))
        .and_then(|path| path.canonicalize())
        .map_err(|e| e.to_string())?;
    let paths = ConfigPaths::discover(&cwd).map_err(|e| e.to_string())?;
    let home = paths.user.parent().ok_or("missing config directory")?;
    let cancel = CancellationToken::new();
    let trusted = TrustStore::new(home)
        .get(&cwd, &cancel)
        .map_err(|e| e.to_string())?
        == Some(true);
    let mut stdout = std::io::stdout().lock();
    let loaded =
        tiechui_config::load(&paths, trusted, &toml::Table::new()).map_err(|e| e.to_string())?;
    let registry = super::plugins::load(&cwd, home, trusted, &loaded.config, &cancel)?;
    if command == "trust" {
        if !trusted && registry.hook_sources().is_empty() {
            return Err("Trust the project first with tc trust allow".into());
        }
        let count = tiechui_module_hooks::trust_with_sources(
            &cwd,
            home,
            trusted,
            registry.hook_sources(),
            &cancel,
        )
        .map_err(|e| e.to_string())?;
        writeln!(stdout, "Approved {count} project/plugin hook handler(s).")
            .map_err(|e| e.to_string())?;
    } else {
        let hooks = HooksModule::load_with_sources(
            &cwd,
            home,
            trusted,
            Default::default(),
            registry.hook_sources(),
            &cancel,
        )
        .map_err(|e| e.to_string())?;
        if args.get_flag("json") {
            serde_json::to_writer_pretty(
                &mut stdout,
                &serde_json::json!({"hooks": hooks.hooks(), "diagnostics": hooks.diagnostics}),
            )
            .map_err(|e| e.to_string())?;
            writeln!(stdout).map_err(|e| e.to_string())?;
        } else {
            for hook in hooks.hooks() {
                writeln!(
                    stdout,
                    "{}\t{}\t{}\t{}\n  {}",
                    hook.event.wire(),
                    if hook.enabled { "enabled" } else { "disabled" },
                    hook.trust,
                    hook.path.display(),
                    hook.command
                )
                .map_err(|e| e.to_string())?;
            }
            for diagnostic in &hooks.diagnostics {
                writeln!(stdout, "{diagnostic}").map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

/// Asks about handlers the session would otherwise skip as new or changed. Their load problems
/// are left for the session to report. `false` means the user left.
pub(crate) fn review(
    cwd: &Path,
    paths: &ConfigPaths,
    home: &Path,
    trusted: bool,
    cancel: &CancellationToken,
) -> Result<bool, String> {
    let loaded =
        tiechui_config::load(paths, trusted, &toml::Table::new()).map_err(|e| e.to_string())?;
    if loaded.config.modules.get("hooks") == Some(&false) {
        return Ok(true);
    }
    let registry =
        super::plugins::registry(cwd, home, trusted, &loaded.config, cancel, &mut Vec::new())?;
    let hooks = HooksModule::load_with_sources(
        cwd,
        home,
        trusted,
        Default::default(),
        registry.hook_sources(),
        cancel,
    )
    .map_err(|e| e.to_string())?;
    let pending: Vec<_> = hooks
        .hooks()
        .iter()
        .filter(|hook| matches!(hook.trust.as_str(), "unapproved" | "changed"))
        .cloned()
        .collect();
    if pending.is_empty() {
        return Ok(true);
    }
    match ask(&question(&pending)).map_err(|e| e.to_string())? {
        Answer::Choice(0) => {
            tiechui_module_hooks::approve(home, &pending, cancel).map_err(|e| e.to_string())?
        }
        Answer::Interrupted => return Ok(false),
        Answer::Choice(_) | Answer::Choices(_) | Answer::Text(_) | Answer::Dismissed => {}
    }
    Ok(true)
}

fn question(pending: &[Hook]) -> Question {
    let mut body = vec![
        match pending.len() {
            1 => "1 hook is new or changed.".to_owned(),
            count => format!("{count} hooks are new or changed."),
        },
        "Approved hooks run automatically with your user rights.".to_owned(),
        String::new(),
    ];
    body.extend(pending.iter().take(REVIEWED).map(|hook| {
        format!(
            "{}: {} ({})",
            hook.event.wire(),
            hook.command,
            hook.path.display()
        )
    }));
    if pending.len() > REVIEWED {
        body.push(format!(
            "and {} more; tc hooks list shows them all",
            pending.len() - REVIEWED
        ));
    }
    Question {
        title: "Hooks need review".to_owned(),
        body,
        choices: vec![
            "Approve all and continue".to_owned(),
            "Continue without approving (they won't run)".to_owned(),
        ],
        dismiss: "skip",
        input: None,
        multiple: false,
    }
}
