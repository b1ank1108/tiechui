//! Paths that every mode protects, and the bash allow rules that auto mode ignores.

use std::path::{Component, Path};

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

pub(crate) fn protected(access: &Access) -> bool {
    access.direction != "read"
        && [&access.lexical, &access.canonical]
            .iter()
            .any(|p| matches(p))
}

fn matches(path: &Path) -> bool {
    let parts: Vec<String> = path
        .components()
        .filter_map(|part| match part {
            Component::Normal(part) => Some(part.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    let directory = parts.iter().enumerate().any(|(at, part)| {
        DIRECTORIES.contains(&part.as_str())
            && !(part == ".claude" && parts.get(at + 1).is_some_and(|next| next == "worktrees"))
    });
    directory
        || parts.last().is_some_and(|name| {
            FILES.contains(&name.as_str())
                || name.starts_with(".env.")
                    && !matches!(
                        name.as_str(),
                        ".env.example" | ".env.sample" | ".env.template"
                    )
        })
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
            "/w/.env.example",
            "/w/.github/workflows/ci.yml",
            "/w/env",
        ] {
            assert!(!matches(Path::new(path)), "{path}");
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
