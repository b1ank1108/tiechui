#![allow(clippy::print_stdout)]

mod packaging;
mod schema;
mod typescript;

use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build local release artifacts from an exported revision.
    Dist {
        #[arg(long, default_value = "HEAD")]
        commit: String,
        /// Build once on every core and skip the reproducibility comparison.
        #[arg(long)]
        ci: bool,
    },
    /// Verify the review candidate offline.
    VerifyRelease,
    /// Install a local tarball into an explicitly selected prefix.
    Install {
        artifact: std::path::PathBuf,
        #[arg(long)]
        prefix: std::path::PathBuf,
    },
    /// Remove owned binaries and restore the prefix's previous state.
    Uninstall {
        #[arg(long)]
        prefix: std::path::PathBuf,
    },
    /// Generate TypeScript plugin protocol types, or check for drift.
    Typescript {
        #[arg(long)]
        check: bool,
    },
    /// Generate the protocol and configuration JSON Schemas, or check that they are current.
    Schema {
        #[arg(long)]
        check: bool,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal = tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            signal_cancel.cancel();
        }
    });
    let result = run(cli, &cancel).await;
    signal.abort();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli, cancel: &CancellationToken) -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("xtask must be inside the workspace")?;
    match cli.command {
        Command::Dist { commit, ci } => {
            let mut args = vec!["dist".into(), "--commit".into(), commit.into()];
            if ci {
                args.push("--ci".into());
            }
            packaging::run(root, &args, cancel).await
        }
        Command::VerifyRelease => packaging::run(root, &["verify-release".into()], cancel).await,
        Command::Install { artifact, prefix } => {
            packaging::run(
                root,
                &[
                    "install".into(),
                    artifact.into_os_string(),
                    "--prefix".into(),
                    prefix.into_os_string(),
                ],
                cancel,
            )
            .await
        }
        Command::Uninstall { prefix } => {
            packaging::run(
                root,
                &[
                    "uninstall".into(),
                    "--prefix".into(),
                    prefix.into_os_string(),
                ],
                cancel,
            )
            .await
        }
        Command::Typescript { check } => typescript::run(root, check),
        Command::Schema { check } => schema::run(root, check),
    }
}
