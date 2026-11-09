//! `hooks/*`: the command hooks a session loads, edited in `hooks.json` files and approved
//! like `chui hooks trust`.

use std::path::Path;

use serde_json::Value;
use tiechui_config::{ConfigPaths, TrustStore};
use tiechui_module_hooks::{Event, Handler, HooksModule, approve, edit};
use tiechui_protocol::app::*;
use tokio_util::sync::CancellationToken;

use super::prompts::project;
use super::{Reply, blocking, parse, value};

pub(super) fn handles(method: &str) -> bool {
    matches!(
        method,
        "hooks/list" | "hooks/save" | "hooks/delete" | "hooks/approve"
    )
}

pub(super) async fn handle(home: &Path, method: &str, params: Value) -> Reply {
    let home = home.to_owned();
    match method {
        "hooks/save" => {
            let p: HookSaveParams = parse(params)?;
            blocking(move |cancel| {
                let event = Event::from_wire(&p.event)
                    .ok_or_else(|| format!("{} is not a hook event", p.event))?;
                let in_project = match (p.scope.as_str(), &p.cwd) {
                    ("user", _) => false,
                    ("project", Some(_)) => true,
                    _ => return Err("the scope is `user`, or `project` with a cwd".into()),
                };
                let handler = Handler {
                    project: in_project,
                    event,
                    matcher: p.matcher,
                    command: p.command,
                    timeout: p.timeout,
                };
                let cwd = project(&home, p.cwd)?;
                edit(&cwd, &home, p.previous.as_deref(), Some(handler), cancel).map_err(|e| e.0)
            })
            .await
            .and_then(value)
        }
        "hooks/delete" => {
            let p: HookDeleteParams = parse(params)?;
            blocking(move |cancel| {
                edit(&project(&home, p.cwd)?, &home, Some(&p.key), None, cancel).map_err(|e| e.0)
            })
            .await
            .and_then(value)
        }
        "hooks/approve" => {
            let p: HookApproveParams = parse(params)?;
            blocking(move |cancel| {
                let (hooks, _) = load(&home, &project(&home, p.cwd)?, cancel)?;
                let pending: Vec<_> = hooks
                    .hooks()
                    .iter()
                    .filter(|hook| {
                        p.keys.contains(&hook.key)
                            && matches!(hook.trust.as_str(), "unapproved" | "changed")
                    })
                    .cloned()
                    .collect();
                approve(&home, &pending, cancel).map_err(|e| e.0)
            })
            .await
            .and_then(value)
        }
        _ => {
            let p: HooksListParams = parse(params)?;
            let path = home.join("hooks.json").display().to_string();
            let (module, mut diagnostics) =
                blocking(move |cancel| load(&home, &project(&home, p.cwd)?, cancel)).await?;
            let hooks = module
                .hooks()
                .iter()
                .map(|hook| HookEntry {
                    key: hook.key.clone(),
                    event: hook.event.wire().into(),
                    matcher: hook.matcher.clone(),
                    command: hook.command.clone(),
                    timeout: hook.timeout,
                    path: hook.path.display().to_string(),
                    source: hook.source().into(),
                    trust: hook.trust.clone(),
                    editable: hook.editable(),
                })
                .collect();
            diagnostics.extend(module.diagnostics);
            value(HookList {
                path,
                hooks,
                diagnostics,
            })
        }
    }
}

/// The hooks a session in `cwd` loads, plugins' included, with plugin warnings.
fn load(
    home: &Path,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<(HooksModule, Vec<String>), String> {
    let paths = ConfigPaths::discover(cwd).map_err(|e| e.to_string())?;
    let trusted = TrustStore::new(home)
        .get(cwd, cancel)
        .map_err(|e| e.to_string())?
        == Some(true);
    let loaded =
        tiechui_config::load(&paths, trusted, &toml::Table::new()).map_err(|e| e.to_string())?;
    let mut warnings = Vec::new();
    let registry =
        crate::plugins::registry(cwd, home, trusted, &loaded.config, cancel, &mut warnings)?;
    let hooks = HooksModule::load_with_sources(
        cwd,
        home,
        trusted,
        Default::default(),
        registry.hook_sources(),
        cancel,
    )
    .map_err(|e| e.0)?;
    Ok((hooks, warnings))
}
