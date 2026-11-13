use std::cell::Cell;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use clap::ArgMatches;
use tiechui_core::SessionHandle;
use tiechui_protocol::SessionEpoch;
use tiechui_tui::{Client, Opened, Target};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::exec::{Failure, Open, Options, Prepared, locations, prepare, startup};
use tiechui_module_loop::Schedules;

/// The session the TUI shows.
struct Live {
    shutdown: CancellationToken,
    task: JoinHandle<Result<(), String>>,
    cwd: PathBuf,
    path: PathBuf,
}

impl Live {
    /// Ends the session, with its failure if it had one.
    async fn end(self) -> Option<String> {
        self.shutdown.cancel();
        match self.task.await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(error) => Some(error.to_string()),
        }
    }
}

/// The session and what the TUI needs of it; problems preparing it go to `warnings`.
async fn open(
    args: &ArgMatches,
    open: Open,
    epoch: SessionEpoch,
    schedules: &Schedules,
    warnings: &mut Vec<String>,
) -> Result<(Live, Opened), Failure> {
    let shutdown = CancellationToken::new();
    let Prepared {
        handle:
            SessionHandle {
                submissions,
                events,
                task,
            },
        tui,
        cwd,
        path,
        ..
    } = prepare(
        &Options::from_args(args),
        shutdown.clone(),
        true,
        open,
        epoch,
        schedules,
        warnings,
    )
    .await?;
    let live = Live {
        shutdown,
        task,
        cwd: cwd.clone(),
        path,
    };
    let opened = Opened {
        client: Client {
            submissions,
            events,
        },
        cwd,
        status_line: tui.status_line,
        copy_on_select: tui.copy_on_select,
        syntax_theme: tui.theme,
        warnings: Vec::new(),
    };
    Ok((live, opened))
}

/// `chui` without a subcommand: the same session setup as `chui exec`, driven by the TUI.
/// `/new`, `/resume` and `/reload` end the session and prepare the next one from scratch,
/// so a session from another directory gets that directory's trust, configuration and tools.
pub(crate) async fn run(args: &ArgMatches) -> i32 {
    let startup = match startup(args) {
        Ok(ops) => ops,
        Err((code, message)) => {
            eprintln!("chui: {message}");
            return code;
        }
    };
    let (cwd, canonical, paths, home) = match locations(&Options::from_args(args)) {
        Ok(locations) => locations,
        Err((code, message)) => {
            eprintln!("chui: {message}");
            return code;
        }
    };
    // `--resume` without an id opens the picker before any session exists.
    let pick = args
        .get_one::<String>("resume")
        .is_some_and(String::is_empty);
    let terminal = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if terminal && crate::setup::needed(&paths) {
        match crate::setup::run(&paths).await {
            Ok(Some(_)) => {}
            Ok(None) => return 0,
            Err(message) => {
                eprintln!("chui: {message}");
                return 2;
            }
        }
    }
    // The picker's session can belong to another directory, not known yet.
    if !pick && terminal {
        match crate::trust::consent(&canonical, &paths, &home) {
            Ok(true) => {}
            Ok(false) => return 0,
            Err(message) => {
                eprintln!("chui: {message}");
                return 2;
            }
        }
    }
    let live = Rc::new(Cell::new(None::<Live>));
    let schedules = Schedules::default();
    let (first, status_line, syntax_theme, copy_on_select) = if pick {
        (None, Default::default(), None, true)
    } else {
        let mut warnings = Vec::new();
        let result = open(args, Open::Args, SessionEpoch(0), &schedules, &mut warnings).await;
        for warning in warnings {
            eprintln!("{warning}");
        }
        match result {
            Ok((session, opened)) => {
                live.set(Some(session));
                (
                    Some(opened.client),
                    opened.status_line,
                    opened.syntax_theme,
                    opened.copy_on_select,
                )
            }
            Err((code, message)) => {
                eprintln!("chui: {message}");
                return code;
            }
        }
    };
    let switch: tiechui_tui::Switch = {
        let (args, live, start) = (Rc::new(args.clone()), live.clone(), cwd.clone());
        Box::new(move |target, epoch| {
            let (args, live, start) = (args.clone(), live.clone(), start.clone());
            let schedules = schedules.clone();
            Box::pin(async move {
                let mut warnings = Vec::new();
                let previous = live.take();
                let (cwd, path) = previous.as_ref().map_or((start, None), |live| {
                    (live.cwd.clone(), Some(live.path.clone()))
                });
                if let Some(previous) = previous
                    && let Some(error) = previous.end().await
                {
                    warnings.push(error);
                }
                let request = match (target, &path) {
                    (Target::Resume(path), _) => Open::Path(path),
                    (Target::Reload, Some(path)) => Open::Path(path.clone()),
                    (Target::New | Target::Reload, _) => Open::New(cwd),
                };
                let (session, mut opened) =
                    match open(&args, request, epoch, &schedules, &mut warnings).await {
                        Ok(pair) => pair,
                        // Back to the session the user left, saying why.
                        Err((_, message)) => {
                            let path = path.ok_or_else(|| message.clone())?;
                            warnings.push(message);
                            open(&args, Open::Path(path), epoch, &schedules, &mut warnings)
                                .await
                                .map_err(|(_, message)| message)?
                        }
                    };
                live.set(Some(session));
                opened.warnings = warnings;
                Ok(opened)
            })
        })
    };
    let result = tiechui_tui::run(
        first,
        tiechui_tui::Host {
            switch,
            services: Arc::new(Services {
                home: home.clone(),
                config: paths.user.clone(),
            }),
        },
        tiechui_tui::Options {
            no_alt_screen: args.get_flag("no-alt-screen"),
            syntax_theme,
            themes_dir: paths.user.parent().map(|path| path.join("themes")),
            startup,
            cwd,
            history: Some(home.join("state").join("input-history.jsonl")),
            queue_hint: None,
            home: tiechui_config::user_home(),
            hostname: None,
            status_line,
            copy_on_select,
            pick,
            banner: true,
        },
    )
    .await;
    let failure = match live.take() {
        Some(live) => live.end().await,
        None => None,
    };
    if let Err(error) = result {
        eprintln!("chui: {error}");
        return 1;
    }
    match failure {
        Some(error) => {
            eprintln!("chui: {error}");
            1
        }
        None => 0,
    }
}

struct Services {
    home: PathBuf,
    config: PathBuf,
}

impl tiechui_tui::Services for Services {
    fn read(
        &self,
        cwd: &Path,
        read: tiechui_tui::resume::Read,
        cancel: &CancellationToken,
    ) -> tiechui_tui::resume::Loaded {
        use tiechui_tui::resume::{Loaded, Part, Read, Summary};
        match read {
            Read::List { all } => Loaded::List {
                all,
                sessions: tiechui_session::summaries(&self.home, (!all).then_some(cwd), cancel)
                    .map(|(sessions, _)| {
                        sessions
                            .into_iter()
                            .map(|session| Summary {
                                path: session.path,
                                id: session.header.id,
                                cwd: session.header.cwd,
                                created_at: session.header.created_at,
                                modified: session.modified,
                                // A title set in the desktop app replaces the opening message.
                                first_message: session.meta.title.unwrap_or(session.first_message),
                                text: session.text,
                            })
                            .collect()
                    })
                    .map_err(|error| error.to_string()),
            },
            Read::Preview(path) => Loaded::Preview {
                parts: tiechui_session::preview(&path, cancel)
                    .map(|parts| {
                        parts
                            .into_iter()
                            .map(|part| match part {
                                tiechui_session::Part::User(text) => Part::User(text),
                                tiechui_session::Part::Assistant(text) => Part::Assistant(text),
                                tiechui_session::Part::Call { name, arguments } => {
                                    Part::Call { name, arguments }
                                }
                                tiechui_session::Part::Output(text) => Part::Output(text),
                                tiechui_session::Part::Compaction(text) => Part::Compaction(text),
                                tiechui_session::Part::Notice(text) => Part::Notice(text),
                            })
                            .collect()
                    })
                    .map_err(|error| error.to_string()),
                path,
            },
        }
    }

    fn save_status_line(&self, line: &tiechui_tui::status::StatusLineConfig) -> Result<(), String> {
        tiechui_config::save_status_line(&self.config, line).map_err(|error| error.to_string())
    }

    fn save_theme(&self, name: &str) -> Result<(), String> {
        tiechui_config::save_preference(&self.config, "tui.theme", name.into())
            .map_err(|error| error.to_string())
    }

    fn save_model(&self, model: &str) -> Result<(), String> {
        tiechui_config::save_preference(&self.config, "model", model.into())
            .map_err(|error| error.to_string())
    }

    fn save_thinking(&self, level: &str) -> Result<(), String> {
        tiechui_config::save_preference(&self.config, "thinking", level.into())
            .map_err(|error| error.to_string())
    }

    fn save_permission_mode(&self, mode: &str) -> Result<(), String> {
        tiechui_config::save_preference(&self.config, "permissions.mode", mode.into())
            .map_err(|error| error.to_string())
    }

    fn trust(&self, cwd: &Path, arguments: &str) -> String {
        trust(&self.home, cwd, arguments)
    }
}

/// `/trust [allow | deny]`, reported as `chui trust` does.
fn trust(home: &Path, cwd: &Path, arguments: &str) -> String {
    let decision = match arguments {
        "" => None,
        "allow" => Some(true),
        "deny" => Some(false),
        _ => return "Usage: /trust [allow | deny]".into(),
    };
    let store = tiechui_config::TrustStore::new(home);
    let cancel = tokio_util::sync::CancellationToken::new();
    if let Some(decision) = decision
        && let Err(error) = store.set(cwd, Some(decision), &cancel)
    {
        return error.to_string();
    }
    let later = if decision.is_some() {
        "\nApplies from the next session."
    } else {
        ""
    };
    match store.get_entry(cwd, &cancel) {
        Ok(Some(entry)) => format!(
            "{}: {} (from {}){later}",
            if entry.decision { "allowed" } else { "denied" },
            cwd.display(),
            entry.path.display()
        ),
        Ok(None) => format!("unknown: {}", cwd.display()),
        Err(error) => error.to_string(),
    }
}
