//! Commands that only read, judged from their decoded words.

use std::path::Path;

pub(crate) fn read_only(argv: &[String]) -> bool {
    let Some((head, args)) = argv.split_first() else {
        return false;
    };
    match Path::new(head).file_name().and_then(|name| name.to_str()) {
        Some(
            "cat" | "head" | "wc" | "stat" | "ls" | "pwd" | "which" | "type" | "uname" | "whoami"
            | "id" | "echo" | "printf" | "realpath" | "readlink" | "basename" | "dirname" | "true"
            | "false" | "grep" | "egrep" | "fgrep" | "jq" | "cut" | "tr" | "diff" | "md5"
            | "md5sum" | "sha1sum" | "sha224sum" | "sha256sum" | "sha384sum" | "sha512sum"
            | "shasum",
        ) => true,
        Some("tail") => !flagged(args, "fF", &["--follow", "--retry"]),
        Some("rg") => !flagged(args, "z", &["--pre", "--pre-glob", "--search-zip"]),
        Some("sort") => !flagged(args, "o", &["--output"]),
        Some("tree") => !flagged(args, "oR", &[]),
        Some("file") => !flagged(args, "C", &["--compile"]),
        Some("uniq") => operands(args) <= 1,
        Some("date") => {
            !flagged(args, "s", &["--set"])
                && args
                    .iter()
                    .all(|a| a.starts_with('-') || a.starts_with('+'))
        }
        Some("find") => !args.iter().any(|a| {
            matches!(
                a.as_str(),
                "-delete"
                    | "-exec"
                    | "-execdir"
                    | "-ok"
                    | "-okdir"
                    | "-fls"
                    | "-fprint"
                    | "-fprint0"
                    | "-fprintf"
            )
        }),
        Some("git") => git(args),
        _ => false,
    }
}

fn git(args: &[String]) -> bool {
    // Global options such as -c and -C come before the subcommand and are never read-only.
    let Some((command, args)) = args.split_first() else {
        return false;
    };
    let unsafe_output = flagged(args, "", &["--output", "--ext-diff", "--textconv"]);
    match command.as_str() {
        "status" | "diff" | "log" | "show" | "blame" | "ls-files" | "rev-parse" | "rev-list"
        | "describe" | "cat-file" | "for-each-ref" | "merge-base" | "shortlog" => !unsafe_output,
        "grep" => !unsafe_output && !flagged(args, "O", &["--open-files-in-pager"]),
        "worktree" => args.first().is_some_and(|a| a == "list"),
        "stash" => args.first().is_some_and(|a| a == "list" || a == "show") && !unsafe_output,
        "config" => args.first().is_some_and(|a| {
            matches!(
                a.as_str(),
                "--get" | "--get-all" | "--get-regexp" | "--list" | "-l"
            )
        }),
        _ => false,
    }
}

/// Whether a short flag (also inside a cluster) or a long flag, including any
/// abbreviation and `=value` form, appears before `--`.
fn flagged(args: &[String], short: &str, long: &[&str]) -> bool {
    args.iter().take_while(|a| *a != "--").any(|arg| {
        if let Some(name) = arg.strip_prefix("--") {
            let name = name.split('=').next().unwrap_or(name);
            !name.is_empty() && long.iter().any(|flag| flag[2..].starts_with(name))
        } else if let Some(cluster) = arg.strip_prefix('-') {
            cluster.chars().any(|c| short.contains(c))
        } else {
            false
        }
    })
}

fn operands(args: &[String]) -> usize {
    args.iter().filter(|a| !a.starts_with('-')).count()
}

#[cfg(test)]
mod tests {
    use super::read_only;

    fn check(command: &str) -> bool {
        read_only(
            &command
                .split_whitespace()
                .map(String::from)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn dangerous_flags_and_forms_are_not_read_only() {
        for command in [
            "cat src/lib.rs",
            "/usr/bin/head -n 5 file",
            "tail -n 20 log",
            "rg -n needle src",
            "sort -u names",
            "uniq names",
            "date +%s",
            "find . -name *.rs",
            "git status --short",
            "git diff HEAD~1",
            "git log --oneline -5",
            "git config --get user.name",
            "git stash list",
        ] {
            assert!(check(command), "{command}");
        }
        for command in [
            "tail -f log",
            "tail -nf 5 log",
            "tail --follow=name log",
            "rg --pre=cat needle",
            "rg --pre-glob *.gz needle",
            "sort -o out names",
            "sort -uo out names",
            "sort --out=out names",
            "uniq names out",
            "date 010100002030",
            "date -s tomorrow",
            "find . -delete",
            "find . -exec rm {} ;",
            "git -c core.pager=sh log",
            "git -C .. status",
            "git diff --output=patch",
            "git log --ext-diff",
            "git grep -O needle",
            "git config user.name x",
            "git stash drop",
            "git commit -m x",
            "tree -o out",
            "python3 -c print(1)",
        ] {
            assert!(!check(command), "{command}");
        }
    }
}
