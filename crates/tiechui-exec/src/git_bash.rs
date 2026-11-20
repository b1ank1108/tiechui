use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

pub const GIT_BASH_ENV: &str = "TIECHUI_GIT_BASH_PATH";

const GIT_SUBBUILD_DIRS: &[&str] = &["mingw64", "mingw32", "ucrt64", "clangarm64"];

/// Locate Git for Windows `bash.exe`, skipping WSL launchers (`System32\bash.exe`, `WindowsApps\bash.exe`).
pub fn resolve_git_bash() -> io::Result<PathBuf> {
    resolve_git_bash_with_env(&BTreeMap::new())
}

/// Locate Git for Windows `bash.exe` using case-insensitive `env` overrides before the process environment.
pub fn resolve_git_bash_with_env(env: &BTreeMap<String, String>) -> io::Result<PathBuf> {
    let lookup = |name: &str| -> Option<OsString> {
        find_env_case_insensitive(env, name)
            .map(OsString::from)
            .or_else(|| std::env::var_os(name))
    };
    resolve_git_bash_from(lookup, Path::is_file).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "Git Bash was not found. Install Git for Windows (https://git-scm.com/downloads/win) or set {GIT_BASH_ENV} to bash.exe"
            ),
        )
    })
}

/// Case-insensitive lookup for an environment variable stored in a `BTreeMap`.
pub fn find_env_case_insensitive<'a>(
    env: &'a BTreeMap<String, String>,
    name: &str,
) -> Option<&'a str> {
    env.get(name).map(String::as_str).or_else(|| {
        env.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    })
}

pub(crate) fn resolve_git_bash_from(
    lookup: impl Fn(&str) -> Option<OsString>,
    is_file: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(explicit) = lookup(GIT_BASH_ENV).filter(|v| !v.is_empty()) {
        let candidate = trim_os_quotes(&explicit);
        return (!is_wsl_launcher(&candidate) && is_file(&candidate)).then_some(candidate);
    }
    let directories = lookup("PATH")
        .as_deref()
        .map(split_search_paths)
        .unwrap_or_default();
    if let Some(bash) = find_direct_bash_on_path(&directories, &is_file) {
        return Some(bash);
    }
    if let Some(bash) = find_bash_from_git_on_path(&directories, &is_file) {
        return Some(bash);
    }
    standard_git_roots(&lookup)
        .into_iter()
        .flat_map(|root| bash_under_git_root(&root))
        .find(|candidate| !is_wsl_launcher(candidate) && is_file(candidate))
}

fn find_direct_bash_on_path(
    directories: &[PathBuf],
    is_file: &impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    directories
        .iter()
        .map(|dir| dir.join("bash.exe"))
        .find(|candidate| !is_wsl_launcher(candidate) && is_file(candidate))
}

fn find_bash_from_git_on_path(
    directories: &[PathBuf],
    is_file: &impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    for dir in directories {
        let git = dir.join("git.exe");
        if is_wsl_launcher(&git) || !is_file(&git) {
            continue;
        }
        if let Some(found) = git_exe_bash_candidates(&git)
            .into_iter()
            .find(|candidate| !is_wsl_launcher(candidate) && is_file(candidate))
        {
            return Some(found);
        }
    }
    None
}

fn git_exe_bash_candidates(git_exe: &Path) -> Vec<PathBuf> {
    let Some((launcher_dir, _)) = split_parent(git_exe) else {
        return Vec::new();
    };
    let Some((install_root, folder_name)) = split_parent(&launcher_dir) else {
        return Vec::new();
    };
    let mut out = bash_under_git_root(&install_root);
    if folder_name.eq_ignore_ascii_case("shims") {
        let scoop_git = install_root.join("apps").join("git").join("current");
        out.extend(bash_under_git_root(&scoop_git));
    } else if folder_name.eq_ignore_ascii_case("bin")
        && let Some((outer_root, subbuild)) = split_parent(&install_root)
        && GIT_SUBBUILD_DIRS
            .iter()
            .any(|name| subbuild.eq_ignore_ascii_case(name))
    {
        out.extend(bash_under_git_root(&outer_root));
    }
    out
}

fn standard_git_roots(lookup: &impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(val) = lookup(var).filter(|v| !v.is_empty()) {
            roots.push(Path::new(&val).join("Git"));
        }
    }
    if let Some(local) = lookup("LOCALAPPDATA").filter(|v| !v.is_empty()) {
        roots.push(Path::new(&local).join("Programs").join("Git"));
    }
    if let Some(scoop) = lookup("SCOOP").filter(|v| !v.is_empty()) {
        roots.push(Path::new(&scoop).join("apps").join("git").join("current"));
    }
    if let Some(profile) = lookup("USERPROFILE").filter(|v| !v.is_empty()) {
        roots.push(
            Path::new(&profile)
                .join("scoop")
                .join("apps")
                .join("git")
                .join("current"),
        );
    }
    roots
}

fn bash_under_git_root(root: &Path) -> Vec<PathBuf> {
    vec![
        root.join("bin").join("bash.exe"),
        root.join("usr").join("bin").join("bash.exe"),
    ]
}

fn trim_os_quotes(raw: &OsStr) -> PathBuf {
    match raw.to_str() {
        Some(text) => PathBuf::from(text.trim().trim_matches('"')),
        None => PathBuf::from(raw),
    }
}

/// Split a `PATH`-like `OsStr`, handling semicolon-delimited Windows lists on non-Windows hosts.
pub fn split_search_paths(raw: &OsStr) -> Vec<PathBuf> {
    if let Some(text) = raw.to_str()
        && looks_like_windows_path_list(text)
    {
        return split_windows_path_entries(text);
    }
    std::env::split_paths(raw)
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// Split a UTF-8 `PATH` string, forcing semicolon splitting when `windows` is true.
pub fn split_windows_path_str(raw: &str, windows: bool) -> Vec<PathBuf> {
    if windows || looks_like_windows_path_list(raw) {
        return split_windows_path_entries(raw);
    }
    std::env::split_paths(raw)
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

fn split_windows_path_entries(text: &str) -> Vec<PathBuf> {
    text.split(';')
        .map(|entry| entry.trim().trim_matches('"'))
        .filter(|entry| !entry.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn looks_like_windows_path_list(text: &str) -> bool {
    let trimmed = text.trim().trim_matches('"');
    let bytes = trimmed.as_bytes();
    text.contains(';')
        || text.contains('\\')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

fn split_parent(path: &Path) -> Option<(PathBuf, String)> {
    #[cfg(not(windows))]
    if let Some(raw) = path.to_str()
        && raw.contains('\\')
    {
        let trimmed = raw.trim_end_matches(['/', '\\']);
        let idx = trimmed.rfind(['/', '\\'])?;
        let parent = &trimmed[..idx];
        let name = &trimmed[idx + 1..];
        if parent.is_empty() || name.is_empty() {
            return None;
        }
        return Some((PathBuf::from(parent), name.to_owned()));
    }
    let parent = path.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    let name = path.file_name()?.to_string_lossy().into_owned();
    Some((parent.to_path_buf(), name))
}

/// Return `true` when `path` points into WSL launcher system directories rather than Git for Windows.
pub fn is_wsl_launcher(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    normalized.contains("/system32/")
        || normalized.contains("/sysnative/")
        || normalized.contains("/syswow64/")
        || normalized.contains("/windowsapps/")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn env_lookup<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name: &str| {
            vars.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| OsString::from(v))
        }
    }

    fn file_set<'a>(files: &'a [&'a str]) -> impl Fn(&Path) -> bool + 'a {
        let normalized: HashSet<String> = files
            .iter()
            .map(|p| p.replace('\\', "/").to_ascii_lowercase())
            .collect();
        move |path: &Path| {
            let key = path
                .to_string_lossy()
                .replace('\\', "/")
                .to_ascii_lowercase();
            normalized.contains(&key)
        }
    }

    #[test]
    fn rejects_wsl_system32_sysnative_and_windowsapps_even_with_forward_slashes() {
        let files = [
            "C:/Windows/System32/bash.exe",
            "C:/Windows/Sysnative/bash.exe",
            "C:/Windows/SysWOW64/bash.exe",
            "C:/Users/alice/AppData/Local/Microsoft/WindowsApps/bash.exe",
            "C:/Users/alice/AppData/Local/Programs/Git/bin/bash.exe",
        ];
        let path_val = [
            r"C:\Windows\System32",
            "C:/Windows/Sysnative",
            r"C:\Windows\SysWOW64",
            r"C:\Users\alice\AppData\Local\Microsoft\WindowsApps",
        ]
        .join(";");
        let found = resolve_git_bash_from(
            env_lookup(&[
                ("Path", &path_val),
                ("LOCALAPPDATA", r"C:\Users\alice\AppData\Local"),
            ]),
            file_set(&files),
        );
        assert_eq!(
            found.map(|p| p.to_string_lossy().replace('\\', "/")),
            Some("C:/Users/alice/AppData/Local/Programs/Git/bin/bash.exe".into())
        );

        let wsl_override = resolve_git_bash_from(
            env_lookup(&[(GIT_BASH_ENV, r"C:\Windows\System32\bash.exe")]),
            file_set(&files),
        );
        assert_eq!(wsl_override, None);
    }

    #[test]
    fn resolves_scoop_shim_and_falls_through_broken_shim_to_later_git_on_path() {
        let files = [
            r"C:\Users\alice\scoop\shims\git.exe",
            r"C:\Users\alice\scoop\apps\git\current\bin\bash.exe",
        ];
        let found_scoop = resolve_git_bash_from(
            env_lookup(&[("Path", r"C:\Users\alice\scoop\shims")]),
            file_set(&files),
        );
        assert_eq!(
            found_scoop.map(|p| p.to_string_lossy().replace('\\', "/")),
            Some("C:/Users/alice/scoop/apps/git/current/bin/bash.exe".into())
        );

        let fallback_files = [
            r"C:\Tools\shims\git.exe",
            r"D:\Git\cmd\git.exe",
            r"D:\Git\usr\bin\bash.exe",
        ];
        let found_later = resolve_git_bash_from(
            env_lookup(&[("Path", r#""C:\Tools\shims";D:\Git\cmd"#)]),
            file_set(&fallback_files),
        );
        assert_eq!(
            found_later.map(|p| p.to_string_lossy().replace('\\', "/")),
            Some("D:/Git/usr/bin/bash.exe".into())
        );
    }

    #[test]
    fn covers_programw6432_x86_localappdata_and_userprofile_scoop_installs() {
        for (env_key, env_val, bash_file, expected) in [
            (
                "ProgramW6432",
                r"C:\Program Files",
                r"C:\Program Files\Git\bin\bash.exe",
                "C:/Program Files/Git/bin/bash.exe",
            ),
            (
                "ProgramFiles(x86)",
                r"C:\Program Files (x86)",
                r"C:\Program Files (x86)\Git\usr\bin\bash.exe",
                "C:/Program Files (x86)/Git/usr/bin/bash.exe",
            ),
            (
                "LOCALAPPDATA",
                r"C:\Users\alice\AppData\Local",
                r"C:\Users\alice\AppData\Local\Programs\Git\bin\bash.exe",
                "C:/Users/alice/AppData/Local/Programs/Git/bin/bash.exe",
            ),
            (
                "USERPROFILE",
                r"C:\Users\alice",
                r"C:\Users\alice\scoop\apps\git\current\usr\bin\bash.exe",
                "C:/Users/alice/scoop/apps/git/current/usr/bin/bash.exe",
            ),
        ] {
            let found =
                resolve_git_bash_from(env_lookup(&[(env_key, env_val)]), file_set(&[bash_file]));
            assert_eq!(
                found.map(|p| p.to_string_lossy().replace('\\', "/")),
                Some(expected.into()),
                "failed for {env_key}"
            );
        }
    }

    #[test]
    fn with_env_reads_case_insensitive_path_from_map() {
        let temp = tempfile::tempdir().unwrap();
        let bash = temp.path().join("Git").join("bin").join("bash.exe");
        let git = temp.path().join("Git").join("cmd").join("git.exe");
        std::fs::create_dir_all(bash.parent().unwrap()).unwrap();
        std::fs::create_dir_all(git.parent().unwrap()).unwrap();
        std::fs::write(&bash, "").unwrap();
        std::fs::write(&git, "").unwrap();

        let mut env = BTreeMap::new();
        env.insert(GIT_BASH_ENV.into(), String::new());
        env.insert("Path".into(), git.parent().unwrap().display().to_string());
        assert_eq!(resolve_git_bash_with_env(&env).unwrap(), bash);
    }
}
