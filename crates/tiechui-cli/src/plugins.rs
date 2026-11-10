use std::io::Write;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use tiechui_config::{ConfigPaths, TrustStore};
use tiechui_plugin_api::{CancellationToken, ModuleConfig, Registry};

/// `registry`, printing what it reports, for the inspection commands.
pub(crate) fn load(
    cwd: &Path,
    home: &Path,
    trusted: bool,
    config: &ModuleConfig,
    cancel: &CancellationToken,
) -> Result<Registry, String> {
    let mut warnings = Vec::new();
    let registry = registry(cwd, home, trusted, config, cancel, &mut warnings);
    for warning in warnings {
        eprintln!("{warning}");
    }
    registry
}

pub(crate) fn registry(
    cwd: &Path,
    home: &Path,
    trusted: bool,
    config: &ModuleConfig,
    cancel: &CancellationToken,
    warnings: &mut Vec<String>,
) -> Result<Registry, String> {
    discovered(cwd, home, trusted, config, cancel, warnings)?
        .registry(config)
        .map_err(|e| e.to_string())
}

pub(crate) fn discovered(
    cwd: &Path,
    home: &Path,
    trusted: bool,
    config: &ModuleConfig,
    cancel: &CancellationToken,
    warnings: &mut Vec<String>,
) -> Result<tiechui_module_plugins::Plugins, String> {
    let plugins = tiechui_module_plugins::discover(cwd, home, trusted, config, cancel)
        .map_err(|e| e.to_string())?;
    for diagnostic in &plugins.diagnostics {
        warnings.push(format!("plugins: {diagnostic}"));
    }
    for plugin in &plugins.plugins {
        for diagnostic in &plugin.diagnostics {
            warnings.push(format!("plugin {}: {diagnostic}", plugin.name));
        }
    }
    Ok(plugins)
}

pub(crate) async fn run(
    command: &str,
    args: &ArgMatches,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let (market_command, args) = if command == "marketplace" {
        let (command, args) = args.subcommand().ok_or("missing marketplace command")?;
        (Some(command), args)
    } else {
        (None, args)
    };
    let cwd = args
        .get_one::<String>("cwd")
        .map_or_else(std::env::current_dir, |path| Ok(PathBuf::from(path)))
        .and_then(|path| path.canonicalize())
        .map_err(|e| e.to_string())?;
    let paths = ConfigPaths::discover(&cwd).map_err(|e| e.to_string())?;
    let home = paths.user.parent().ok_or("missing config directory")?;
    let mut stdout = std::io::stdout().lock();
    if let Some(command) = market_command {
        use tiechui_module_plugins::marketplace;
        match command {
            "add" => {
                let source = args
                    .get_one::<String>("source")
                    .ok_or("missing marketplace source")?;
                let market = marketplace::add(
                    source,
                    args.get_one::<String>("ref").map(String::as_str),
                    &cwd,
                    home,
                    cancel,
                )
                .await
                .map_err(|e| e.to_string())?;
                writeln!(
                    stdout,
                    "Added marketplace {} at {}.",
                    market.name, market.commit
                )
                .map_err(|e| e.to_string())?;
            }
            "list" => {
                for market in marketplace::list(home, cancel).map_err(|e| e.to_string())? {
                    writeln!(
                        stdout,
                        "{}\t{}\t{}",
                        market.name,
                        marketplace::display_path(&market.source),
                        market.commit
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            "remove" => {
                let source = args
                    .get_one::<String>("source")
                    .ok_or("missing marketplace name or source")?;
                let name =
                    marketplace::remove(source, &cwd, home, cancel).map_err(|e| e.to_string())?;
                writeln!(stdout, "Removed marketplace {name}.").map_err(|e| e.to_string())?;
            }
            _ => return Err("unknown marketplace command".into()),
        }
        return Ok(());
    }
    match command {
        "add" => {
            let path = args
                .get_one::<String>("path")
                .ok_or("missing plugin path")?;
            let name = if path.contains('@') && !cwd.join(path).exists() {
                tiechui_module_plugins::marketplace::install(path, home, false, cancel)
                    .await
                    .map_err(|e| e.to_string())?
                    .name
            } else {
                tiechui_module_plugins::add(&cwd.join(path), home, cancel)
                    .map_err(|e| e.to_string())?
            };
            writeln!(stdout, "Installed plugin {name}.").map_err(|e| e.to_string())?;
        }
        "upgrade" => {
            for change in tiechui_module_plugins::marketplace::upgrade(
                args.get_one::<String>("name").map(String::as_str),
                home,
                cancel,
            )
            .await
            .map_err(|e| e.to_string())?
            {
                writeln!(
                    stdout,
                    "{}: {} -> {}\n  {} -> {}",
                    change.name,
                    change.old_commit,
                    change.new_commit,
                    change.old_hash,
                    change.new_hash
                )
                .map_err(|e| e.to_string())?;
                for file in change.changes {
                    writeln!(stdout, "  {file}").map_err(|e| e.to_string())?;
                }
                if change.old_hash != change.new_hash && change.has_capabilities {
                    writeln!(
                        stdout,
                        "  Content changed; capabilities require approval on next start."
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
        }
        "remove" => {
            let name = args
                .get_one::<String>("name")
                .ok_or("missing plugin name")?;
            tiechui_module_plugins::remove(name, home, cancel).map_err(|e| e.to_string())?;
            writeln!(stdout, "Removed plugin {name}.").map_err(|e| e.to_string())?;
        }
        "list" => {
            let trusted = TrustStore::new(home)
                .get(&cwd, cancel)
                .map_err(|e| e.to_string())?
                == Some(true);
            let loaded = tiechui_config::load(&paths, trusted, &toml::Table::new())
                .map_err(|e| e.to_string())?;
            let plugins =
                tiechui_module_plugins::discover(&cwd, home, trusted, &loaded.config, cancel)
                    .map_err(|e| e.to_string())?;
            if args.get_flag("json") {
                serde_json::to_writer_pretty(&mut stdout, &plugins).map_err(|e| e.to_string())?;
                writeln!(stdout).map_err(|e| e.to_string())?;
            } else {
                for plugin in plugins.plugins {
                    writeln!(
                        stdout,
                        "{}\t{}\t{}",
                        plugin.name,
                        if plugin.enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        plugin.root.display()
                    )
                    .map_err(|e| e.to_string())?;
                    for (kind, paths) in [
                        ("skills", plugin.skills),
                        ("prompts (listed only)", plugin.prompts),
                        ("agents (listed only)", plugin.agents),
                    ] {
                        for path in paths {
                            writeln!(stdout, "  {kind}: {}", path.display())
                                .map_err(|e| e.to_string())?;
                        }
                    }
                    if let Some(installation) = plugin.installation {
                        writeln!(
                            stdout,
                            "  source: {}\n  content hash: {}",
                            tiechui_module_plugins::marketplace::display_path(
                                &installation.source.to_string_lossy()
                            ),
                            installation.content_hash
                        )
                        .map_err(|e| e.to_string())?;
                    }
                    for diagnostic in plugin.diagnostics {
                        writeln!(stdout, "  {diagnostic}").map_err(|e| e.to_string())?;
                    }
                }
                for diagnostic in plugins.diagnostics {
                    writeln!(stdout, "{diagnostic}").map_err(|e| e.to_string())?;
                }
            }
        }
        _ => return Err("unknown plugin command".into()),
    }
    Ok(())
}
