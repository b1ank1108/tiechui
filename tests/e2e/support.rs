use tokio::process::Command;

#[path = "../support/env.rs"]
mod env;

pub fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chui"));
    command.env_clear().envs(env::windows_env());
    #[cfg(not(windows))]
    command.env("PATH", "/usr/bin:/bin");
    command
}
