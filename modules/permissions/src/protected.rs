use std::path::{Component, Path, PathBuf};

use tiechui_plugin_api::PermissionAction as Action;

use crate::path::Access;
use crate::policy::Rule;

const DIRECTORIES: [&str; 6] = [".git", ".agents", ".claude", ".tiechui", ".vscode", ".idea"];
const FILES: [&str; 10] = [
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".mcp.json",
    ".ripgreprc",
    ".env",
];
const INTERPRETERS: [&str; 21] = [
    "python", "python3", "node", "deno", "tsx", "ruby", "perl", "php", "lua", "npx", "bunx",
    "bash", "sh", "zsh", "fish", "ssh", "eval", "exec", "env", "xargs", "sudo",
];

pub(crate) fn protected(access: &Access, root: &Path, cwd: &Path) -> bool {
    if access.direction == "read" {
        return false;
    }
    let inside_root = access.canonical.starts_with(root);
    [&access.lexical, &access.canonical]
        .into_iter()
        .any(|candidate| matches(&scoped(candidate, inside_root, root, cwd)))
}

fn scoped(path: &Path, inside_root: bool, root: &Path, cwd: &Path) -> PathBuf {
    if !isolatable(root) {
        return path.to_owned();
    }
    if let Ok(rel) = path.strip_prefix(root) {
        return rel.to_owned();
    }
    if inside_root && let Some(rel) = symlinked_relative(path, root, cwd) {
        return rel;
    }
    path.to_owned()
}

fn symlinked_relative(path: &Path, root: &Path, cwd: &Path) -> Option<PathBuf> {
    let cwd = crate::path::normalize(cwd);
    let real = crate::path::canonical(&cwd).ok()?;
    let cwd_rel = real.strip_prefix(root).ok()?;
    let lexical_root = strip_suffix(&cwd, cwd_rel)?;
    if crate::path::canonical(&lexical_root).ok().as_deref() != Some(root)
        || !isolatable(&lexical_root)
    {
        return None;
    }
    path.strip_prefix(&lexical_root).ok().map(Path::to_owned)
}

fn strip_suffix(path: &Path, suffix: &Path) -> Option<PathBuf> {
    let mut parts = path.components();
    for tail in suffix.components().rev() {
        if parts.next_back() != Some(tail) {
            return None;
        }
    }
    Some(parts.as_path().to_owned())
}

fn isolatable(root: &Path) -> bool {
    let parts = normal_parts(root);
    !parts
        .iter()
        .any(|part| matches!(part.as_str(), ".git" | ".vscode" | ".idea"))
        && !parts
            .last()
            .is_some_and(|last| DIRECTORIES.contains(&last.as_str()))
}

fn normal_parts(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|part| match part {
            Component::Normal(part) => Some(part.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect()
}

fn exempted(parent: &str, child: Option<&String>) -> bool {
    matches!(parent, ".claude" | ".agents" | ".tiechui")
        && child.is_some_and(|next| matches!(next.as_str(), "worktrees" | "plans" | "state"))
}

fn sensitive_file(name: &str) -> bool {
    FILES.contains(&name)
        || name.starts_with(".env.")
            && !matches!(name, ".env.example" | ".env.sample" | ".env.template")
}

fn matches(path: &Path) -> bool {
    let parts = normal_parts(path);
    let directory = parts.iter().enumerate().any(|(at, part)| {
        DIRECTORIES.contains(&part.as_str()) && !exempted(part, parts.get(at + 1))
    });
    directory || parts.last().is_some_and(|name| sensitive_file(name))
}

/// An allow rule that would let auto mode run arbitrary code without review.
pub(crate) fn broad_bash_allow(rule: &Rule) -> bool {
    if rule.action != Action::Allow {
        return false;
    }
    if rule.surface == "*" || rule.pattern == "*" {
        return true;
    }
    let mut words = rule.pattern.trim_end_matches('*').split_whitespace();
    let head = words.next().unwrap_or("");
    INTERPRETERS.contains(&head)
        || matches!(head, "npm" | "yarn" | "pnpm" | "bun") && words.next() == Some("run")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_paths_match_components_and_names() {
        for path in [
            "/w/.git/hooks/pre-commit",
            "/w/.GIT/config",
            "/w/.claude/settings.json",
            "/home/u/.bashrc",
            "/w/.env",
            "/w/.env.local",
            "/w/.vscode",
        ] {
            assert!(matches(Path::new(path)), "{path}");
        }
        for path in [
            "/w/src/main.rs",
            "/w/.claude/worktrees/feature/src/lib.rs",
            "/w/.claude/plans/plan.md",
            "/w/.claude/state/cache.json",
            "/w/.agents/worktrees/feature/src/lib.rs",
            "/w/.agents/plans/plan.md",
            "/w/.agents/state/cache.json",
            "/w/.tiechui/worktrees/feature/src/lib.rs",
            "/w/.tiechui/plans/plan.md",
            "/w/.tiechui/state/rewind/snapshot.json",
            "/w/.env.example",
            "/w/.github/workflows/ci.yml",
            "/w/env",
        ] {
            assert!(!matches(Path::new(path)), "{path}");
        }
        assert!(matches(Path::new("/w/.claude/worktrees/feature/.env")));
    }

    #[test]
    fn workspace_ancestor_isolation_spares_interior_files_only_when_root_is_safe() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().canonicalize().unwrap();
        let skill = home.join(".claude/skills/demo");
        std::fs::create_dir_all(&skill).unwrap();
        let check = |raw: &str, root: &Path, direction: &'static str| {
            let access = Access::new(raw, root, &home, Some("write"), direction).unwrap();
            protected(&access, root, root)
        };
        assert!(!check("SKILL.md", &skill, "write"));
        assert!(!check(".env", &skill, "read"));
        assert!(check(".env", &skill, "write"));
        assert!(check(".git/hooks/pre-commit", &skill, "write"));
        assert!(check("../../settings.json", &skill, "write"));

        let claude_root = home.join(".claude");
        assert!(check("settings.json", &claude_root, "write"));

        let git_sub = home.join("repo/.git/modules/sub");
        std::fs::create_dir_all(&git_sub).unwrap();
        assert!(check("config", &git_sub, "write"));

        for ide_sub in [
            home.join("repo/.vscode/extensions"),
            home.join("repo/.idea/runConfigurations"),
        ] {
            std::fs::create_dir_all(&ide_sub).unwrap();
            assert!(check("settings.json", &ide_sub, "write"));
        }

        #[cfg(unix)]
        {
            let dotfiles = home.join("dotfiles");
            let target = dotfiles.join("claude");
            std::fs::create_dir_all(&target).unwrap();
            let linked_claude = home.join("alt-home/.claude");
            std::fs::create_dir_all(linked_claude.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&target, &linked_claude).unwrap();
            let access = Access::new(
                "settings.json",
                &linked_claude,
                &home,
                Some("write"),
                "write",
            )
            .unwrap();
            assert!(protected(&access, &dotfiles, &linked_claude));

            let direct_claude = home.join("direct-home/.claude");
            std::fs::create_dir_all(direct_claude.parent().unwrap()).unwrap();
            std::fs::create_dir_all(dotfiles.join("sub")).unwrap();
            std::os::unix::fs::symlink(&dotfiles, &direct_claude).unwrap();
            let sub_cwd = direct_claude.join("sub");
            let access = Access::new("note.json", &sub_cwd, &home, Some("write"), "write").unwrap();
            assert!(protected(&access, &dotfiles, &sub_cwd));
        }
    }

    #[test]
    fn broad_bash_allows_cover_interpreters_and_wildcards() {
        let rule =
            |surface: &str, pattern: &str| Rule::new(surface, pattern, Action::Allow, "user");
        for (surface, pattern) in [
            ("*", "*"),
            ("bash", "*"),
            ("bash", "python3 *"),
            ("bash", "python*"),
            ("bash", "npm run *"),
            ("bash", "sudo"),
        ] {
            assert!(broad_bash_allow(&rule(surface, pattern)), "{pattern}");
        }
        for pattern in ["cargo test *", "npm test", "git status"] {
            assert!(!broad_bash_allow(&rule("bash", pattern)), "{pattern}");
        }
        assert!(!broad_bash_allow(&Rule::new(
            "bash",
            "*",
            Action::Ask,
            "user"
        )));
    }
}
