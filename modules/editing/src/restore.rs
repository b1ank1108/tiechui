//! Working files a checkpoint summarizes away, restored from disk after the summary.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};
use tiechui_plugin_api::model::{Content, Message};
use tiechui_plugin_api::{
    CancellationToken, Compactor, RestoreContext, RestoredContext, ToolError, async_trait,
};

use crate::{EditingModule, check, resolve};

const SOURCE: &str = "editing";
const MAX_FILES: usize = 5;
/// Larger files are skipped rather than read only to be truncated.
const MAX_BYTES: u64 = 1 << 20;

/// Re-reads files the summarized history read, wrote or edited: modified files first, then
/// latest access first, at most five, within half the restore budget so other contributors
/// keep room. Files the retained history read or wrote in full need no restoring.
#[async_trait]
impl Compactor for EditingModule {
    async fn restore(
        &self,
        context: &RestoreContext,
        cancel: CancellationToken,
    ) -> Result<Vec<RestoredContext>, ToolError> {
        let cwd = &context.lifecycle.cwd;
        let Ok(root) = std::fs::canonicalize(cwd) else {
            return Ok(Vec::new());
        };
        let retained: HashSet<_> = accesses(&context.retained, cwd)
            .into_iter()
            .filter(|(_, name)| *name != "edit")
            .map(|(path, _)| path)
            .collect();
        let mut budget = context.total_tokens / 2;
        let mut items = Vec::new();
        for (path, modified) in candidates(context, cwd) {
            check(&cancel)?;
            if items.len() == MAX_FILES {
                break;
            }
            if retained.contains(&path) {
                continue;
            }
            let Some(body) = text_file(&path, &root) else {
                continue;
            };
            let display = path
                .strip_prefix(cwd)
                .unwrap_or(&path)
                .display()
                .to_string();
            let limit = context.item_tokens.min(budget);
            if let Some(text) = fit(&display, modified, &body, limit, context.estimate) {
                budget -= (context.estimate)(&text);
                items.push(RestoredContext {
                    source: SOURCE.into(),
                    key: path.to_string_lossy().into_owned(),
                    version: format!("sha256:{:x}", Sha256::digest(&body)),
                    text,
                });
            }
        }
        Ok(items)
    }
}

/// Files the summarized history touched, modified first, then latest access first, followed
/// by the files the previous checkpoint restored. True marks a modified file.
fn candidates(context: &RestoreContext, cwd: &Path) -> Vec<(PathBuf, bool)> {
    let mut order: Vec<(PathBuf, bool)> = Vec::new();
    for (path, name) in accesses(&context.summarized, cwd).into_iter().rev() {
        let modified = name != "read";
        match order.iter_mut().find(|(seen, _)| *seen == path) {
            Some(entry) => entry.1 |= modified,
            None => order.push((path, modified)),
        }
    }
    order.sort_by_key(|(_, modified)| !modified);
    for item in context.previous.iter().filter(|item| item.source == SOURCE) {
        let path = PathBuf::from(&item.key);
        if !order.iter().any(|(seen, _)| *seen == path) {
            order.push((path, false));
        }
    }
    order
}

/// Successful `read`, `write` and `edit` calls as resolved path and tool name, oldest first.
fn accesses<'a>(messages: &'a [Message], cwd: &Path) -> Vec<(PathBuf, &'a str)> {
    let succeeded: HashSet<_> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            Content::ToolResult(result) if !result.is_error => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect();
    messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            Content::ToolCall(call)
                if matches!(call.name.as_str(), "read" | "write" | "edit")
                    && succeeded.contains(call.id.as_str()) =>
            {
                let path = resolve(call.arguments["path"].as_str()?, cwd).ok()?;
                Some((path, call.name.as_str()))
            }
            _ => None,
        })
        .collect()
}

/// Current UTF-8 content of a regular, non-secret file inside the workspace.
fn text_file(path: &Path, root: &Path) -> Option<String> {
    let real = std::fs::canonicalize(path).ok()?;
    if !eligible(&real, root) {
        return None;
    }
    let text = String::from_utf8(std::fs::read(&real).ok()?).ok()?;
    (!text.trim().is_empty() && !text.contains('\0')).then_some(text)
}

/// A regular, non-secret file inside `root`, small enough to restore.
fn eligible(real: &Path, root: &Path) -> bool {
    real.strip_prefix(root)
        .is_ok_and(|relative| !secret(relative))
        && std::fs::metadata(real)
            .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= MAX_BYTES)
}

/// Credentials stay out of automatic restores even when the model read them.
fn secret(relative: &Path) -> bool {
    let parts: Vec<String> = relative
        .components()
        .filter_map(|part| match part {
            Component::Normal(part) => Some(part.to_string_lossy().to_lowercase()),
            _ => None,
        })
        .collect();
    let directory = parts[..parts.len().saturating_sub(1)]
        .iter()
        .any(|part| matches!(part.as_str(), ".git" | ".ssh" | ".gnupg" | ".aws"));
    directory
        || parts.last().is_some_and(|name| {
            matches!(
                name.as_str(),
                ".env"
                    | ".netrc"
                    | ".npmrc"
                    | ".pypirc"
                    | ".pgpass"
                    | "credentials"
                    | "credentials.json"
            ) || name.starts_with(".env.")
                && !matches!(
                    name.as_str(),
                    ".env.example" | ".env.sample" | ".env.template"
                )
                || ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"]
                    .iter()
                    .any(|key| name.starts_with(key))
                || [".pem", ".key", ".p12", ".pfx"]
                    .iter()
                    .any(|extension| name.ends_with(extension))
        })
}

/// The most whole lines of `body` whose rendering fits `limit`.
fn fit(
    display: &str,
    modified: bool,
    body: &str,
    limit: u64,
    estimate: fn(&str) -> u64,
) -> Option<String> {
    let path = escape(display);
    let render = |end: usize| {
        let lines = body[..end].lines().count();
        let note = if end < body.len() {
            format!("\n[Truncated after line {lines}; read {display} for the rest.]")
        } else {
            String::new()
        };
        let modified = if modified { " modified=\"true\"" } else { "" };
        format!(
            "<file path=\"{path}\"{modified}>\n{}{note}\n</file>",
            body[..end].trim_end_matches('\n')
        )
    };
    let ends: Vec<_> = body
        .match_indices('\n')
        .map(|(index, _)| index + 1)
        .chain((!body.ends_with('\n')).then_some(body.len()))
        .collect();
    let count = ends.partition_point(|&end| estimate(&render(end)) <= limit);
    (count > 0).then(|| render(ends[count - 1]))
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiechui_plugin_api::LifecycleContext;
    use tiechui_plugin_api::model::{Role, ToolCall, ToolResult};

    fn estimate(text: &str) -> u64 {
        text.len() as u64 / 4
    }

    /// One call and its result per access; `ok` false records a failed call.
    fn history(accesses: &[(&str, &str, bool)]) -> Vec<Message> {
        accesses
            .iter()
            .enumerate()
            .flat_map(|(index, (name, path, ok))| {
                let id = format!("call-{index}");
                [
                    Message {
                        role: Role::Assistant,
                        content: vec![Content::ToolCall(ToolCall {
                            id: id.clone(),
                            name: (*name).into(),
                            arguments: serde_json::json!({"path": path}),
                            opaque: None,
                        })],
                        opaque: None,
                    },
                    Message {
                        role: Role::Tool,
                        content: vec![Content::ToolResult(ToolResult {
                            call_id: id,
                            content: vec![],
                            is_error: !ok,
                        })],
                        opaque: None,
                    },
                ]
            })
            .collect()
    }

    fn context(
        cwd: &Path,
        summarized: Vec<Message>,
        retained: Vec<Message>,
        previous: Vec<RestoredContext>,
        tokens: (u64, u64),
    ) -> RestoreContext {
        RestoreContext {
            lifecycle: LifecycleContext {
                session_id: "session".into(),
                turn_id: "turn".into(),
                cwd: cwd.to_owned(),
                transcript_path: None,
                model: "model".into(),
            },
            summarized,
            retained,
            previous,
            item_tokens: tokens.0,
            total_tokens: tokens.1,
            estimate,
        }
    }

    async fn restored(context: &RestoreContext) -> Vec<RestoredContext> {
        EditingModule
            .restore(context, CancellationToken::new())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn restores_modified_then_recent_workspace_text_files_at_most_five() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cwd = workspace.path();
        for name in [
            "e.rs",
            "f.rs",
            "a.rs",
            "b.rs",
            "c.rs",
            "d.rs",
            "kept.rs",
            "failed.rs",
        ] {
            std::fs::write(cwd.join(name), format!("{name} before\n")).unwrap();
        }
        std::fs::write(cwd.join(".env"), "TOKEN=secret\n").unwrap();
        std::fs::write(cwd.join("bin.dat"), b"binary\0data").unwrap();
        let foreign = outside.path().join("foreign.rs");
        std::fs::write(&foreign, "foreign\n").unwrap();
        let summarized = history(&[
            ("read", "e.rs", true),
            ("read", "f.rs", true),
            ("read", "a.rs", true),
            ("edit", "b.rs", true),
            ("read", "c.rs", true),
            ("write", "d.rs", true),
            ("read", "b.rs", true),
            ("read", ".env", true),
            ("read", foreign.to_str().unwrap(), true),
            ("read", "bin.dat", true),
            ("read", "failed.rs", false),
            ("read", "kept.rs", true),
            ("read", "missing.rs", true),
        ]);
        let retained = history(&[("read", "kept.rs", true), ("edit", "c.rs", true)]);
        // The checkpoint restores what is on disk now, not what the history saw.
        std::fs::write(cwd.join("d.rs"), "d.rs after\n").unwrap();
        let items = restored(&context(cwd, summarized, retained, vec![], (1000, 10_000))).await;
        let keys: Vec<_> = items
            .iter()
            .map(|item| Path::new(&item.key).file_name().unwrap().to_str().unwrap())
            .collect();
        // b.rs was read after d.rs was written, so it is the most recent modified file.
        assert_eq!(keys, ["b.rs", "d.rs", "c.rs", "a.rs", "f.rs"]);
        assert_eq!(
            items[1].text,
            "<file path=\"d.rs\" modified=\"true\">\nd.rs after\n</file>"
        );
        assert_eq!(items[3].text, "<file path=\"a.rs\">\na.rs before\n</file>");
        assert!(
            items
                .iter()
                .all(|item| item.source == SOURCE && item.version.starts_with("sha256:"))
        );
    }

    #[tokio::test]
    async fn truncates_by_whole_lines_within_half_the_budget_and_carries_previous_items() {
        let workspace = tempfile::tempdir().unwrap();
        let cwd = workspace.path();
        let body: String = (1..=200).map(|line| format!("line {line:03}\n")).collect();
        std::fs::write(cwd.join("long.rs"), &body).unwrap();
        std::fs::write(cwd.join("old.rs"), "old\n").unwrap();
        let previous = RestoredContext {
            source: SOURCE.into(),
            key: cwd.join("old.rs").to_string_lossy().into_owned(),
            version: String::new(),
            text: String::new(),
        };
        let foreign = RestoredContext {
            source: "skills".into(),
            key: cwd.join("skill.md").to_string_lossy().into_owned(),
            ..previous.clone()
        };
        let items = restored(&context(
            cwd,
            history(&[("read", "long.rs", true)]),
            vec![],
            vec![previous, foreign],
            (100, 300),
        ))
        .await;
        assert_eq!(items.len(), 2);
        assert!(estimate(&items[0].text) <= 100);
        assert!(items[0].text.contains("line 001\n"));
        assert!(
            items[0]
                .text
                .ends_with("; read long.rs for the rest.]\n</file>")
        );
        assert!(!items[0].text.contains("line 200"));
        assert_eq!(items[1].text, "<file path=\"old.rs\">\nold\n</file>");
        let spent: u64 = items.iter().map(|item| estimate(&item.text)).sum();
        assert!(spent <= 150);
        assert!(
            restored(&context(cwd, vec![], vec![], vec![], (100, 300)))
                .await
                .is_empty()
        );
    }

    #[test]
    fn credentials_and_vcs_internals_are_secret() {
        for path in [
            ".env",
            ".env.local",
            "config/.npmrc",
            "keys/id_ed25519",
            "tls/server.pem",
            ".git/config",
            ".aws/credentials",
        ] {
            assert!(secret(Path::new(path)), "{path}");
        }
        for path in [".env.example", "src/env.rs", "docs/keys.md", "git/config"] {
            assert!(!secret(Path::new(path)), "{path}");
        }
    }
}
