use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use clap::ArgMatches;
use tiechui_config::{ConfigPaths, TrustStore};
use tiechui_module_mcp::{DeclineElicitation, McpModule};
use tiechui_plugin_api::CancellationToken;

pub(crate) async fn run(command: &str, args: &ArgMatches) -> Result<(), String> {
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
    let loaded =
        tiechui_config::load(&paths, trusted, &toml::Table::new()).map_err(|e| e.to_string())?;
    let registry = super::plugins::load(&cwd, home, trusted, &loaded.config, &cancel)?;
    if command != "list" {
        use tiechui_module_mcp::oauth::{Login, OAuthStore};
        let name = args
            .get_one::<String>("server")
            .ok_or("missing server name")?;
        let server = loaded
            .config
            .mcp_servers
            .get(name)
            .cloned()
            .or_else(|| {
                registry
                    .mcp_sources()
                    .iter()
                    .flat_map(|source| source.servers())
                    .find(|s| &s.name == name)
                    .map(|s| s.config)
            })
            .ok_or_else(|| format!("No MCP server named {name}"))?;
        let url = server
            .url
            .as_deref()
            .filter(|_| server.command.is_none())
            .ok_or("OAuth requires a Streamable HTTP server")?;
        let store = OAuthStore::new(home, name, url);
        let operation = async {
            match command {
                "login" => {
                    let scopes = args
                        .get_many::<String>("scopes")
                        .map(|s| s.cloned().collect::<Vec<_>>())
                        .unwrap_or_default();
                    let login = Login::begin(store, &scopes, &cancel)
                        .await
                        .map_err(|e| e.to_string())?;
                    {
                        let mut stdout = std::io::stdout().lock();
                        writeln!(
                            stdout,
                            "Authorize {name} by opening this URL in your browser:\n{}",
                            login.authorization_url
                        )
                        .map_err(|e| e.to_string())?;
                        stdout.flush().map_err(|e| e.to_string())?;
                    }
                    login.finish(&cancel).await.map_err(|e| e.to_string())?;
                    writeln!(std::io::stdout(), "Logged in to MCP server {name}.")
                        .map_err(|e| e.to_string())?;
                }
                "logout" => {
                    store.logout(&cancel).await.map_err(|e| e.to_string())?;
                    writeln!(std::io::stdout(), "Logged out of MCP server {name}.")
                        .map_err(|e| e.to_string())?;
                }
                _ => return Err("unknown MCP command".into()),
            }
            Ok(())
        };
        return tokio::select! {
            result = operation => result,
            _ = tokio::signal::ctrl_c() => { cancel.cancel(); Err("Operation aborted".into()) },
        };
    }
    let module = McpModule::from_config(
        &loaded.config,
        registry.mcp_sources(),
        &cwd,
        paths.project.as_deref(),
        trusted,
        cancel.clone(),
        Arc::new(DeclineElicitation),
        Some(home),
    );
    tokio::select! {
        _ = module.wait_for_startup(cancel.clone()) => {},
        _ = tokio::signal::ctrl_c() => cancel.cancel(),
    }
    let statuses = module.statuses();
    let diagnostics: Vec<_> = loaded.diagnostics.iter().map(|diagnostic| {
        tiechui_plugin_api::serde_json::json!({"path":diagnostic.file,"key":diagnostic.key,"reason":format!("{:?}",diagnostic.reason)})
    }).collect();
    let mut stdout = std::io::stdout().lock();
    if args.get_flag("json") {
        serde_json::to_writer_pretty(
            &mut stdout,
            &serde_json::json!({"servers":statuses,"config_diagnostics":diagnostics}),
        )
        .map_err(|e| e.to_string())?;
        writeln!(stdout).map_err(|e| e.to_string())?;
    } else {
        for server in statuses {
            writeln!(stdout, "{}\t{:?}", server.name, server.state).map_err(|e| e.to_string())?;
            if let Some(oauth) = server.oauth {
                writeln!(stdout, "  OAuth: {oauth}").map_err(|e| e.to_string())?;
            }
            for tool in server.tools {
                writeln!(stdout, "  {tool}").map_err(|e| e.to_string())?;
            }
            if let Some(error) = server.error {
                writeln!(stdout, "  error: {error}").map_err(|e| e.to_string())?;
            }
            for diagnostic in server.diagnostics {
                writeln!(stdout, "  diagnostic: {diagnostic}").map_err(|e| e.to_string())?;
            }
        }
        for diagnostic in diagnostics {
            writeln!(stdout, "config: {diagnostic}").map_err(|e| e.to_string())?;
        }
    }
    cancel.cancel();
    Ok(())
}
