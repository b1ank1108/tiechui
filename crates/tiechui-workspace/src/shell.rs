use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use tiechui_protocol::workspace::{TerminalShellEntry, TerminalShellsResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShellPlatform {
    Windows,
    Posix,
}

const HOST_PLATFORM: ShellPlatform = if cfg!(windows) {
    ShellPlatform::Windows
} else {
    ShellPlatform::Posix
};

const POSIX_LOGIN_SHELLS: &[&str] = &["zsh", "bash", "fish", "sh", "dash", "ksh"];

const POSIX_STANDARD_SHELLS: &[(&str, &[&str])] = &[
    (
        "zsh",
        &[
            "/bin/zsh",
            "/usr/bin/zsh",
            "/opt/homebrew/bin/zsh",
            "/usr/local/bin/zsh",
        ],
    ),
    (
        "bash",
        &[
            "/bin/bash",
            "/usr/bin/bash",
            "/opt/homebrew/bin/bash",
            "/usr/local/bin/bash",
        ],
    ),
    (
        "fish",
        &[
            "/opt/homebrew/bin/fish",
            "/usr/local/bin/fish",
            "/usr/bin/fish",
            "/bin/fish",
        ],
    ),
    ("sh", &["/bin/sh", "/usr/bin/sh"]),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedShell {
    pub name: String,
    pub path: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

pub(crate) fn discover_shells() -> TerminalShellsResult {
    discover_for_platform(
        HOST_PLATFORM,
        |name| std::env::var_os(name),
        is_host_executable,
        probe_host_shell,
    )
}

pub(crate) fn resolve_shell(configured: Option<&str>) -> (ResolvedShell, ResolvedShell) {
    resolve_for_platform(
        HOST_PLATFORM,
        configured,
        |name| std::env::var_os(name),
        is_host_executable,
        probe_host_shell,
    )
}

/// Git Bash on Windows; elsewhere `$SHELL`, then the passwd login shell, then `/bin/sh`.
fn probe_host_shell() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        tiechui_exec::resolve_git_bash().ok()
    }
    #[cfg(unix)]
    {
        Some(PathBuf::from(
            portable_pty::CommandBuilder::new_default_prog().get_shell(),
        ))
    }
}

fn is_host_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && (meta.permissions().mode() & 0o111) != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

fn discover_for_platform(
    platform: ShellPlatform,
    lookup: impl Fn(&str) -> Option<OsString>,
    is_executable: impl Fn(&Path) -> bool,
    probed_shell: impl FnOnce() -> Option<PathBuf>,
) -> TerminalShellsResult {
    match platform {
        ShellPlatform::Windows => discover_windows_shells(&lookup, &is_executable, probed_shell()),
        ShellPlatform::Posix => {
            discover_posix_shells(lookup("SHELL").as_deref(), probed_shell(), &is_executable)
        }
    }
}

fn resolve_for_platform(
    platform: ShellPlatform,
    configured: Option<&str>,
    lookup: impl Fn(&str) -> Option<OsString>,
    is_executable: impl Fn(&Path) -> bool,
    probed_shell: impl FnOnce() -> Option<PathBuf>,
) -> (ResolvedShell, ResolvedShell) {
    let discovered = discover_for_platform(platform, &lookup, &is_executable, probed_shell);
    let fallback = default_resolved_shell(platform, &discovered);
    let Some(requested) = normalize_requested_shell(configured) else {
        return (fallback.clone(), fallback);
    };
    let primary = match platform {
        ShellPlatform::Windows => {
            resolve_windows_configured(&requested, &discovered, &lookup, &is_executable)
        }
        ShellPlatform::Posix => {
            resolve_posix_configured(&requested, &discovered, &lookup, &is_executable)
        }
    }
    .unwrap_or_else(|| fallback.clone());
    (primary, fallback)
}

fn normalize_requested_shell(raw: Option<&str>) -> Option<String> {
    let trimmed = raw?.trim().trim_matches('"');
    (!trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("auto")).then(|| trimmed.to_owned())
}

fn default_resolved_shell(
    platform: ShellPlatform,
    discovered: &TerminalShellsResult,
) -> ResolvedShell {
    if let Some(entry) = discovered
        .shells
        .iter()
        .find(|entry| entry.id == discovered.default_shell)
        .or_else(|| discovered.shells.first())
    {
        return entry_to_resolved(platform, entry);
    }
    match platform {
        ShellPlatform::Windows => ResolvedShell {
            name: "cmd".into(),
            path: PathBuf::from("cmd.exe"),
            args: Vec::new(),
            env: Vec::new(),
        },
        ShellPlatform::Posix => ResolvedShell {
            name: "sh".into(),
            path: PathBuf::from("/bin/sh"),
            args: vec!["-l".into()],
            env: vec![("SHELL".into(), "/bin/sh".into())],
        },
    }
}

fn entry_to_resolved(platform: ShellPlatform, entry: &TerminalShellEntry) -> ResolvedShell {
    let path = PathBuf::from(&entry.path);
    match platform {
        ShellPlatform::Windows => windows_spec_for(&entry.id, path),
        ShellPlatform::Posix => posix_spec_for(&entry.id, path),
    }
}

fn windows_spec_for(id_or_stem: &str, path: PathBuf) -> ResolvedShell {
    let lower = id_or_stem.to_ascii_lowercase();
    match lower.as_str() {
        "pwsh" => ResolvedShell {
            name: "pwsh".into(),
            path,
            args: vec!["-NoLogo".into()],
            env: Vec::new(),
        },
        "powershell" => ResolvedShell {
            name: "powershell".into(),
            path,
            args: vec!["-NoLogo".into()],
            env: Vec::new(),
        },
        "git-bash" | "bash" => ResolvedShell {
            name: "git-bash".into(),
            path,
            args: vec!["--login".into(), "-i".into()],
            env: vec![("CHERE_INVOKING".into(), "1".into())],
        },
        "cmd" => ResolvedShell {
            name: "cmd".into(),
            path,
            args: Vec::new(),
            env: Vec::new(),
        },
        other => ResolvedShell {
            name: other.to_owned(),
            path,
            args: Vec::new(),
            env: Vec::new(),
        },
    }
}

fn posix_spec_for(stem: &str, path: PathBuf) -> ResolvedShell {
    let args = if POSIX_LOGIN_SHELLS.contains(&stem) {
        vec!["-l".into()]
    } else {
        Vec::new()
    };
    let shell_env = path.display().to_string();
    ResolvedShell {
        name: stem.to_owned(),
        path,
        args,
        env: vec![("SHELL".into(), shell_env)],
    }
}

fn discover_windows_shells(
    lookup: &impl Fn(&str) -> Option<OsString>,
    is_file: &impl Fn(&Path) -> bool,
    git_bash: Option<PathBuf>,
) -> TerminalShellsResult {
    let search_dirs = lookup("PATH")
        .as_deref()
        .map(tiechui_exec::split_search_paths)
        .unwrap_or_default();
    let mut shells = Vec::new();

    if let Some(pwsh) = find_windows_pwsh(&search_dirs, lookup, is_file) {
        shells.push(TerminalShellEntry {
            id: "pwsh".into(),
            label: "PowerShell 7 (pwsh)".into(),
            path: pwsh.to_string_lossy().into_owned(),
        });
    }
    if let Some(ps) = find_windows_powershell(&search_dirs, lookup, is_file) {
        shells.push(TerminalShellEntry {
            id: "powershell".into(),
            label: "Windows PowerShell".into(),
            path: ps.to_string_lossy().into_owned(),
        });
    }
    if let Some(bash) = git_bash.filter(|p| !is_wsl_bash_path(p) && is_file(p)) {
        shells.push(TerminalShellEntry {
            id: "git-bash".into(),
            label: "Git Bash".into(),
            path: bash.to_string_lossy().into_owned(),
        });
    }
    if let Some(cmd) = find_windows_cmd(&search_dirs, lookup, is_file) {
        shells.push(TerminalShellEntry {
            id: "cmd".into(),
            label: "Command Prompt (cmd)".into(),
            path: cmd.to_string_lossy().into_owned(),
        });
    }

    let default_shell = shells
        .first()
        .map(|s| s.id.clone())
        .unwrap_or_else(|| "cmd".into());
    TerminalShellsResult {
        shells,
        default_shell,
    }
}

fn find_windows_pwsh(
    search_dirs: &[PathBuf],
    lookup: &impl Fn(&str) -> Option<OsString>,
    is_file: &impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(found) = find_in_windows_dirs(search_dirs, "pwsh.exe", is_file) {
        return Some(found);
    }
    let mut candidates = Vec::new();
    for var in ["ProgramW6432", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(base) = non_empty_var(lookup, var) {
            candidates.push(join_windows_path(&base, r"PowerShell\7\pwsh.exe"));
            candidates.push(join_windows_path(&base, r"PowerShell\7-preview\pwsh.exe"));
        }
    }
    if let Some(local) = non_empty_var(lookup, "LOCALAPPDATA") {
        candidates.push(join_windows_path(&local, r"Microsoft\WindowsApps\pwsh.exe"));
    }
    if let Some(scoop) = non_empty_var(lookup, "SCOOP") {
        candidates.push(join_windows_path(&scoop, r"apps\pwsh\current\pwsh.exe"));
    }
    if let Some(profile) = non_empty_var(lookup, "USERPROFILE") {
        candidates.push(join_windows_path(
            &profile,
            r"scoop\apps\pwsh\current\pwsh.exe",
        ));
        candidates.push(join_windows_path(&profile, r".dotnet\tools\pwsh.exe"));
    }
    candidates.into_iter().find(|p| is_file(p))
}

fn find_windows_powershell(
    search_dirs: &[PathBuf],
    lookup: &impl Fn(&str) -> Option<OsString>,
    is_file: &impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(found) = find_in_windows_dirs(search_dirs, "powershell.exe", is_file) {
        return Some(found);
    }
    let sysroot = windows_system_root(lookup);
    let candidate = join_windows_path(&sysroot, r"System32\WindowsPowerShell\v1.0\powershell.exe");
    is_file(&candidate).then_some(candidate)
}

fn find_windows_cmd(
    search_dirs: &[PathBuf],
    lookup: &impl Fn(&str) -> Option<OsString>,
    is_file: &impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    if let Some(comspec) = non_empty_var(lookup, "COMSPEC") {
        let candidate = PathBuf::from(comspec.trim_matches('"'));
        if is_file(&candidate) {
            return Some(candidate);
        }
    }
    if let Some(found) = find_in_windows_dirs(search_dirs, "cmd.exe", is_file) {
        return Some(found);
    }
    let sysroot = windows_system_root(lookup);
    let candidate = join_windows_path(&sysroot, r"System32\cmd.exe");
    is_file(&candidate).then_some(candidate)
}

fn windows_system_root(lookup: &impl Fn(&str) -> Option<OsString>) -> String {
    non_empty_var(lookup, "SystemRoot")
        .or_else(|| non_empty_var(lookup, "windir"))
        .unwrap_or_else(|| r"C:\Windows".into())
}

fn non_empty_var(lookup: &impl Fn(&str) -> Option<OsString>, name: &str) -> Option<String> {
    let val = lookup(name)?;
    let trimmed = val.to_string_lossy().trim().trim_matches('"').to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn find_in_windows_dirs(
    dirs: &[PathBuf],
    exe_name: &str,
    is_file: &impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| join_windows_path(&dir.to_string_lossy(), exe_name))
        .find(|candidate| is_file(candidate))
}

fn join_windows_path(base: &str, rel: &str) -> PathBuf {
    let looks_win = cfg!(windows)
        || base.contains('\\')
        || (base.len() >= 2
            && base.as_bytes()[0].is_ascii_alphabetic()
            && base.as_bytes()[1] == b':');
    if looks_win {
        let left = base.trim_end_matches(['\\', '/']);
        let right = rel.trim_start_matches(['\\', '/']).replace('/', "\\");
        PathBuf::from(format!(r"{left}\{right}"))
    } else {
        Path::new(base).join(rel.replace('\\', "/"))
    }
}

const WINDOWS_SHELL_ALIASES: &[(&str, &str)] = &[
    ("pwsh", "pwsh"),
    ("pwsh.exe", "pwsh"),
    ("powershell", "powershell"),
    ("powershell.exe", "powershell"),
    ("git-bash", "git-bash"),
    ("gitbash", "git-bash"),
    ("bash", "git-bash"),
    ("bash.exe", "git-bash"),
    ("cmd", "cmd"),
    ("cmd.exe", "cmd"),
];

fn is_wsl_bash_path(path: &Path) -> bool {
    let lower = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    lower.ends_with("/bash.exe") && tiechui_exec::is_wsl_launcher(path)
}

fn find_windows_discovered_entry<'a>(
    requested: &str,
    discovered: &'a TerminalShellsResult,
) -> Option<&'a TerminalShellEntry> {
    let lower = requested.to_ascii_lowercase();
    if let Some((_, target_id)) = WINDOWS_SHELL_ALIASES
        .iter()
        .find(|&&(alias, _)| alias == lower)
        && let Some(entry) = discovered.shells.iter().find(|s| s.id == *target_id)
    {
        return Some(entry);
    }
    discovered
        .shells
        .iter()
        .find(|s| s.path.eq_ignore_ascii_case(requested))
}

fn find_windows_executable_in_path(
    expanded: &str,
    lookup: &impl Fn(&str) -> Option<OsString>,
    is_file: &impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let search_dirs = lookup("PATH")
        .as_deref()
        .map(tiechui_exec::split_search_paths)
        .unwrap_or_default();
    let names = if expanded.to_ascii_lowercase().ends_with(".exe") {
        vec![expanded.to_owned()]
    } else {
        vec![format!("{expanded}.exe"), expanded.to_owned()]
    };
    names.into_iter().find_map(|exe_name| {
        find_in_windows_dirs(&search_dirs, &exe_name, is_file)
            .filter(|found| !is_wsl_bash_path(found))
    })
}

fn resolve_windows_configured(
    requested: &str,
    discovered: &TerminalShellsResult,
    lookup: &impl Fn(&str) -> Option<OsString>,
    is_file: &impl Fn(&Path) -> bool,
) -> Option<ResolvedShell> {
    if let Some(entry) = find_windows_discovered_entry(requested, discovered) {
        return Some(entry_to_resolved(ShellPlatform::Windows, entry));
    }

    let expanded = expand_tilde_shell(requested, lookup);
    if expanded.contains(['\\', '/', ':']) {
        let path = PathBuf::from(&expanded);
        if !is_wsl_bash_path(&path) && is_file(&path) {
            return Some(windows_spec_for(&windows_stem(&expanded), path));
        }
        return None;
    }

    let found = find_windows_executable_in_path(&expanded, lookup, is_file)?;
    let stem = windows_stem(&found.to_string_lossy());
    Some(windows_spec_for(&stem, found))
}

fn windows_stem(raw_path: &str) -> String {
    let base = raw_path
        .trim_end_matches(['\\', '/'])
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(raw_path);
    for suffix in [".exe", ".cmd", ".bat"] {
        if base.len() > suffix.len()
            && base[base.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
        {
            return base[..base.len() - suffix.len()].to_ascii_lowercase();
        }
    }
    base.to_ascii_lowercase()
}

fn discover_posix_shells(
    shell_env: Option<&OsStr>,
    login_shell: Option<PathBuf>,
    is_executable: &impl Fn(&Path) -> bool,
) -> TerminalShellsResult {
    let mut shells = Vec::new();
    let mut seen_ids = HashSet::new();
    let mut preferred_id = None;

    let env_shell = shell_env
        .and_then(OsStr::to_str)
        .map(|s| s.trim().trim_matches('"'))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    for candidate in env_shell.into_iter().chain(login_shell) {
        if !is_executable(&candidate) {
            continue;
        }
        let Some(stem) = posix_stem(&candidate) else {
            continue;
        };
        if seen_ids.insert(stem.clone()) {
            preferred_id.get_or_insert_with(|| stem.clone());
            shells.push(TerminalShellEntry {
                id: stem.clone(),
                label: stem,
                path: candidate.display().to_string(),
            });
        }
    }

    for &(id, paths) in POSIX_STANDARD_SHELLS {
        if seen_ids.contains(id) {
            continue;
        }
        for &raw_path in paths {
            let candidate = Path::new(raw_path);
            if !is_executable(candidate) {
                continue;
            }
            seen_ids.insert(id.to_owned());
            shells.push(TerminalShellEntry {
                id: id.to_owned(),
                label: id.to_owned(),
                path: raw_path.to_owned(),
            });
            break;
        }
    }

    let default_shell = preferred_id
        .or_else(|| shells.iter().find(|s| s.id == "sh").map(|s| s.id.clone()))
        .or_else(|| shells.first().map(|s| s.id.clone()))
        .unwrap_or_else(|| "sh".into());

    TerminalShellsResult {
        shells,
        default_shell,
    }
}

fn find_posix_executable_in_path(
    expanded: &str,
    lookup: &impl Fn(&str) -> Option<OsString>,
    is_executable: &impl Fn(&Path) -> bool,
) -> Option<(String, PathBuf)> {
    let path_var = lookup("PATH")?;
    std::env::split_paths(&path_var)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| {
            let candidate = dir.join(expanded);
            is_executable(&candidate)
                .then(|| posix_stem(&candidate).map(|stem| (stem, candidate)))
                .flatten()
        })
}

fn resolve_posix_configured(
    requested: &str,
    discovered: &TerminalShellsResult,
    lookup: &impl Fn(&str) -> Option<OsString>,
    is_executable: &impl Fn(&Path) -> bool,
) -> Option<ResolvedShell> {
    if let Some(entry) = discovered
        .shells
        .iter()
        .find(|s| s.id == requested || s.path == requested)
    {
        return Some(entry_to_resolved(ShellPlatform::Posix, entry));
    }

    let expanded = expand_tilde_shell(requested, lookup);
    if expanded.contains('/') {
        let path = PathBuf::from(&expanded);
        return is_executable(&path)
            .then(|| posix_stem(&path).map(|stem| posix_spec_for(&stem, path)))
            .flatten();
    }

    let (stem, candidate) = find_posix_executable_in_path(&expanded, lookup, is_executable)?;
    Some(posix_spec_for(&stem, candidate))
}

fn posix_stem(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn expand_tilde_shell(raw: &str, lookup: &impl Fn(&str) -> Option<OsString>) -> String {
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix(r"~\"))
        && let Some(home) =
            non_empty_var(lookup, "HOME").or_else(|| non_empty_var(lookup, "USERPROFILE"))
    {
        return join_windows_path(&home, rest)
            .to_string_lossy()
            .into_owned();
    }
    raw.to_owned()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn env_from<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name: &str| {
            vars.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| OsString::from(v))
        }
    }

    fn win_files<'a>(files: &'a [&'a str]) -> impl Fn(&Path) -> bool + 'a {
        let set: HashSet<String> = files
            .iter()
            .map(|f| f.replace('\\', "/").to_ascii_lowercase())
            .collect();
        move |p: &Path| set.contains(&p.to_string_lossy().replace('\\', "/").to_ascii_lowercase())
    }

    #[test]
    fn windows_shell_discovery_orders_pwsh_powershell_git_bash_cmd_and_falls_back() {
        let all_files = [
            r"C:\Program Files\PowerShell\7\pwsh.exe",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Windows\System32\cmd.exe",
        ];
        let base_env = [
            ("ProgramFiles", r"C:\Program Files"),
            ("SystemRoot", r"C:\Windows"),
            ("COMSPEC", r"C:\Windows\System32\cmd.exe"),
        ];

        let full = discover_for_platform(
            ShellPlatform::Windows,
            env_from(&base_env),
            win_files(&all_files),
            || Some(PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")),
        );
        let ids: Vec<&str> = full.shells.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["pwsh", "powershell", "git-bash", "cmd"]);
        assert_eq!(full.default_shell, "pwsh");

        let without_pwsh = discover_for_platform(
            ShellPlatform::Windows,
            env_from(&base_env),
            win_files(&all_files[1..]),
            || Some(PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")),
        );
        let ids_no_pwsh: Vec<&str> = without_pwsh.shells.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids_no_pwsh, vec!["powershell", "git-bash", "cmd"]);
        assert_eq!(without_pwsh.default_shell, "powershell");

        let only_bash_and_cmd = discover_for_platform(
            ShellPlatform::Windows,
            env_from(&base_env),
            win_files(&all_files[2..]),
            || Some(PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")),
        );
        assert_eq!(only_bash_and_cmd.default_shell, "git-bash");

        let only_cmd = discover_for_platform(
            ShellPlatform::Windows,
            env_from(&base_env),
            win_files(&all_files[3..]),
            || None,
        );
        let ids_cmd: Vec<&str> = only_cmd.shells.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids_cmd, vec!["cmd"]);
        assert_eq!(only_cmd.default_shell, "cmd");
    }

    #[test]
    fn windows_shell_resolution_sets_nologo_git_bash_login_flags_and_falls_back() {
        let files = [
            r"C:\Program Files\PowerShell\7\pwsh.exe",
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe",
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Windows\System32\bash.exe",
            r"C:\Windows\System32\cmd.exe",
        ];
        let env = [
            ("ProgramFiles", r"C:\Program Files"),
            ("SystemRoot", r"C:\Windows"),
        ];
        let resolve = |configured: Option<&str>| {
            resolve_for_platform(
                ShellPlatform::Windows,
                configured,
                env_from(&env),
                win_files(&files),
                || Some(PathBuf::from(r"C:\Program Files\Git\bin\bash.exe")),
            )
            .0
        };

        let auto = resolve(None);
        assert_eq!(auto.name, "pwsh");
        assert_eq!(auto.args, vec!["-NoLogo"]);

        let ps = resolve(Some("powershell"));
        assert_eq!(ps.name, "powershell");
        assert_eq!(ps.args, vec!["-NoLogo"]);

        let git_bash = resolve(Some("git-bash"));
        assert_eq!(git_bash.name, "git-bash");
        assert_eq!(git_bash.args, vec!["--login", "-i"]);
        assert_eq!(git_bash.env, vec![("CHERE_INVOKING".into(), "1".into())]);

        let explicit_bash = resolve(Some(r"C:\Program Files\Git\bin\bash.exe"));
        assert_eq!(explicit_bash.name, "git-bash");
        assert_eq!(explicit_bash.args, vec!["--login", "-i"]);

        let wsl_launcher_fallback = resolve(Some(r"C:\Windows\System32\bash.exe"));
        assert_eq!(wsl_launcher_fallback.name, "pwsh");

        let missing = resolve(Some(r"C:\Missing\shell.exe"));
        assert_eq!(missing.name, "pwsh");
    }

    #[test]
    fn posix_shell_discovery_prefers_valid_env_shell_deduplicates_and_falls_back_to_sh() {
        let exec_set = |paths: &'static [&'static str]| {
            move |p: &Path| paths.iter().any(|item| Path::new(item) == p)
        };

        let with_env = discover_for_platform(
            ShellPlatform::Posix,
            env_from(&[("SHELL", "/opt/homebrew/bin/fish")]),
            exec_set(&[
                "/opt/homebrew/bin/fish",
                "/usr/local/bin/fish",
                "/bin/zsh",
                "/bin/bash",
                "/usr/bin/bash",
                "/bin/sh",
            ]),
            || None,
        );
        let ids: Vec<&str> = with_env.shells.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["fish", "zsh", "bash", "sh"]);
        assert_eq!(with_env.shells[0].path, "/opt/homebrew/bin/fish");
        assert_eq!(with_env.default_shell, "fish");

        let unset_env = discover_for_platform(
            ShellPlatform::Posix,
            env_from(&[("SHELL", "/missing/custom-shell")]),
            exec_set(&["/bin/zsh", "/bin/bash", "/bin/sh"]),
            || None,
        );
        assert_eq!(unset_env.default_shell, "sh");

        let login_fallback = discover_for_platform(
            ShellPlatform::Posix,
            env_from(&[("SHELL", "/missing/custom-shell")]),
            exec_set(&["/bin/zsh", "/bin/bash", "/bin/sh"]),
            || Some(PathBuf::from("/bin/zsh")),
        );
        assert_eq!(login_fallback.default_shell, "zsh");
        assert_eq!(login_fallback.shells[0].path, "/bin/zsh");

        let (resolved_zsh, fallback_sh) = resolve_for_platform(
            ShellPlatform::Posix,
            Some("zsh"),
            env_from(&[]),
            exec_set(&["/bin/zsh", "/bin/sh"]),
            || None,
        );
        assert_eq!(resolved_zsh.name, "zsh");
        assert_eq!(resolved_zsh.path, PathBuf::from("/bin/zsh"));
        assert_eq!(resolved_zsh.args, vec!["-l"]);
        assert_eq!(resolved_zsh.env, vec![("SHELL".into(), "/bin/zsh".into())]);
        assert_eq!(fallback_sh.name, "sh");
        assert_eq!(fallback_sh.path, PathBuf::from("/bin/sh"));
    }
}
