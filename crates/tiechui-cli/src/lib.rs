//! Shared command-line entry point for the `tiechui` and `chui` binaries.

mod app_server;
mod approvals;
mod diagnostics;
mod doctor;
mod exec;
mod hooks;
mod interactive;
mod mcp;
mod plugins;
mod setup;
mod skills;
mod subagents;
mod trust;

use clap::{Arg, Command};

fn mode() -> Arg {
    Arg::new("mode")
        .long("mode")
        .value_parser(["rules", "auto", "yolo"])
        .help("permission mode; overrides permissions.mode")
}

fn thinking() -> Arg {
    Arg::new("thinking")
        .long("thinking")
        .value_parser(["off", "minimal", "low", "medium", "high", "xhigh", "max"])
        .help("thinking level, clamped to what the model accepts")
}

pub fn run(name: &'static str) {
    let matches = Command::new(name)
        .version(env!("CARGO_PKG_VERSION"))
        .about("Tiechui, a Rust terminal coding agent")
        .subcommand(Command::new("doctor")
            .about("Check installation and profile integrity without changing files")
            .arg(Arg::new("prefix").long("prefix").value_name("DIRECTORY"))
            .arg(Arg::new("profile").long("profile").value_name("DIRECTORY")))
        .arg(Arg::new("model").long("model").value_name("PROVIDER/MODEL"))
        .arg(mode())
        .arg(thinking())
        .arg(Arg::new("no-alt-screen")
            .long("no-alt-screen")
            .action(clap::ArgAction::SetTrue)
            .help("Use inline mode with native terminal scrollback"))
        .arg(Arg::new("cwd").long("cwd").value_name("DIRECTORY"))
        .arg(
            Arg::new("resume")
                .long("resume")
                .value_name("ID")
                .num_args(0..=1)
                .default_missing_value("")
                .help("resume a session by id, or choose one"),
        )
        .subcommand(
            Command::new("plugin")
                .about("Manage plugin packages and Git marketplaces")
                .subcommand_required(true)
                .subcommand(Command::new("add").arg(Arg::new("path").required(true))
                    .arg(Arg::new("cwd").long("cwd").value_name("DIRECTORY")))
                .subcommand(Command::new("remove").arg(Arg::new("name").required(true))
                    .arg(Arg::new("cwd").long("cwd").value_name("DIRECTORY")))
                .subcommand(Command::new("upgrade").arg(Arg::new("name"))
                    .arg(Arg::new("cwd").long("cwd").value_name("DIRECTORY")))
                .subcommand(Command::new("marketplace").subcommand_required(true)
                    .subcommand(Command::new("add").arg(Arg::new("source").required(true))
                        .arg(Arg::new("ref").long("ref"))
                        .arg(Arg::new("cwd").long("cwd")))
                    .subcommand(Command::new("list").arg(Arg::new("cwd").long("cwd")))
                    .subcommand(Command::new("remove").arg(Arg::new("source").required(true))
                        .arg(Arg::new("cwd").long("cwd"))))
                .subcommand(Command::new("list")
                    .arg(Arg::new("cwd").long("cwd").value_name("DIRECTORY"))
                    .arg(Arg::new("json").long("json").action(clap::ArgAction::SetTrue))),
        )
        .subcommand(
            Command::new("hooks")
                .about("Inspect and approve command hooks")
                .subcommand_required(true)
                .subcommand(Command::new("list")
                    .arg(Arg::new("cwd").long("cwd").value_name("DIRECTORY"))
                    .arg(Arg::new("json").long("json").action(clap::ArgAction::SetTrue)))
                .subcommand(Command::new("trust")
                    .arg(Arg::new("cwd").long("cwd").value_name("DIRECTORY"))),
        )
        .subcommand(
            Command::new("mcp")
                .about("Inspect MCP servers")
                .subcommand_required(true)
                .subcommand(Command::new("login").arg(Arg::new("server").required(true))
                    .arg(Arg::new("scopes").long("scopes").value_delimiter(','))
                    .arg(Arg::new("cwd").long("cwd")))
                .subcommand(Command::new("logout").arg(Arg::new("server").required(true))
                    .arg(Arg::new("cwd").long("cwd")))
                .subcommand(
                    Command::new("list")
                        .arg(Arg::new("cwd").long("cwd").value_name("DIRECTORY"))
                        .arg(
                            Arg::new("json")
                                .long("json")
                                .action(clap::ArgAction::SetTrue),
                        ),
                ),
        )
        .subcommand(
            Command::new("skills")
                .about("Inspect resolved skills")
                .subcommand_required(true)
                .subcommand(
                    Command::new("list")
                        .arg(Arg::new("cwd").long("cwd").value_name("DIRECTORY"))
                        .arg(
                            Arg::new("json")
                                .long("json")
                                .action(clap::ArgAction::SetTrue),
                        ),
                ),
        )
        .subcommand(
            Command::new("setup")
                .about("Choose a model service and model, saved to the user profile"),
        )
        .subcommand(
            Command::new("trust")
                .about("Manage project trust without prompting")
                .arg(Arg::new("cwd").long("cwd").value_name("DIRECTORY"))
                .arg(
                    Arg::new("decision")
                        .value_parser(["allow", "deny", "status"])
                        .default_value("status"),
                ),
        )
        .subcommand(
            Command::new("exec")
                .about("Run a prompt in a persisted session")
                .arg(Arg::new("prompt").required(true))
                .arg(Arg::new("model").long("model").value_name("PROVIDER/MODEL"))
                .arg(thinking())
                .arg(mode())
                .arg(Arg::new("cwd").long("cwd").value_name("DIRECTORY"))
                .arg(
                    Arg::new("approval")
                        .long("approval")
                        .value_parser(["deny", "allow-listed", "yolo"])
                        .default_value("allow-listed")
                        .help("deny all calls, deny asks the mode leaves open, or use --mode yolo when --mode is absent"),
                )
                .arg(
                    Arg::new("json")
                        .long("json")
                        .action(clap::ArgAction::SetTrue),
                )
                .arg(Arg::new("resume").long("resume").value_name("ID")),
        )
        .subcommand(
            Command::new("app-server")
                .about("Serve sessions to desktop clients over JSON-RPC on stdio"),
        )
        .get_matches();
    if let Some(("doctor", args)) = matches.subcommand() {
        if let Err(error) = doctor::run(args) {
            eprintln!("{name}: {error}");
            std::process::exit(2);
        }
        return;
    }
    if let Some(("hooks", args)) = matches.subcommand() {
        if let Some((command, args)) = args.subcommand()
            && let Err(error) = hooks::run(command, args)
        {
            eprintln!("{name}: {error}");
            std::process::exit(2);
        }
        return;
    }
    if let Some(("skills", args)) = matches.subcommand() {
        if let Some(("list", args)) = args.subcommand()
            && let Err(error) = skills::list(args)
        {
            eprintln!("{name}: {error}");
            std::process::exit(2);
        }
        return;
    }
    if let Some(("trust", args)) = matches.subcommand() {
        if let Err(error) = trust::run(args) {
            eprintln!("{name}: {error}");
            std::process::exit(2);
        }
        return;
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{name}: {error}");
            std::process::exit(1);
        }
    };
    if let Some(("mcp", args)) = matches.subcommand() {
        if let Some((command, args)) = args.subcommand()
            && let Err(error) = runtime.block_on(mcp::run(command, args))
        {
            eprintln!("{name}: {error}");
            std::process::exit(2);
        }
        return;
    }
    if let Some(("plugin", args)) = matches.subcommand() {
        let cancel = tiechui_plugin_api::CancellationToken::new();
        let result = runtime.block_on(async {
            tokio::select! {
                result = async {
                    let (command, args) = args.subcommand().ok_or("missing plugin command")?;
                    plugins::run(command, args, &cancel).await
                } => result,
                _ = tokio::signal::ctrl_c() => { cancel.cancel(); Err("Operation aborted".into()) }
            }
        });
        if let Err(error) = result {
            eprintln!("{name}: {error}");
            std::process::exit(2);
        }
        return;
    }
    let code = match matches.subcommand() {
        Some(("exec", args)) => runtime.block_on(exec::run(args)),
        Some(("setup", _)) => runtime.block_on(setup::command()),
        Some(("app-server", _)) => runtime.block_on(app_server::run()),
        None => runtime.block_on(interactive::run(&matches)),
        Some(_) => return,
    };
    std::process::exit(code);
}
