pub fn path(path: &std::path::Path) -> String {
    let path = path.display().to_string();
    #[cfg(windows)]
    {
        if let Some(path) = path.strip_prefix(r"\\?\UNC\") {
            format!("//{}", path.replace('\\', "/"))
        } else {
            path.strip_prefix(r"\\?\")
                .unwrap_or(&path)
                .replace('\\', "/")
        }
    }
    #[cfg(not(windows))]
    path
}
