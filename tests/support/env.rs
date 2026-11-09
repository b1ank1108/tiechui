pub fn windows_env() -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    #[cfg(windows)]
    {
        [
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
        ]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(|value| (key.into(), value)))
        .collect()
    }
    #[cfg(not(windows))]
    Vec::new()
}
