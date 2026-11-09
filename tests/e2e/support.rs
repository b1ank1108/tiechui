use tokio::process::Command;

pub fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_chui"));
    command.env_clear();
    #[cfg(not(windows))]
    command.env("PATH", "/usr/bin:/bin");
    #[cfg(windows)]
    for key in [
        "PATH",
        "PATHEXT",
        "SYSTEMROOT",
        "WINDIR",
        "COMSPEC",
        "TEMP",
        "TMP",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "TIECHUI_GIT_BASH_PATH",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
}
