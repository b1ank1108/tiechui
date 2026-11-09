#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;

use tempfile::TempDir;
use tiechui_module_context_files::{ContextFilesModule, MAX_BYTES};
use tiechui_plugin_api::{
    CancellationToken, ContextProvider, Module, ModuleConfig, Registry, TurnInput, serde_json,
};

fn write(root: &Path, name: &str, content: impl AsRef<[u8]>) {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

async fn render(module: &ContextFilesModule, cwd: &Path) -> String {
    module
        .fragments(
            &TurnInput {
                cwd: cwd.into(),
                content: vec![],
            },
            CancellationToken::new(),
        )
        .await
        .unwrap()
        .join("\n\n")
}

fn fixture_audit(module: &ContextFilesModule, root: &Path) -> serde_json::Value {
    let mut audit = module.entry_data();
    audit["files"]
        .as_array_mut()
        .unwrap()
        .retain(|file| Path::new(file["path"].as_str().unwrap()).starts_with(root));
    audit
}

async fn render_fixture(module: &ContextFilesModule, cwd: &Path, root: &Path) -> String {
    let mut text = render(module, cwd).await;
    // Repository-local fixtures inherit real ancestor instructions, outside the golden data.
    for file in module.entry_data()["files"].as_array().unwrap() {
        let path = file["path"].as_str().unwrap();
        if !Path::new(path).starts_with(root) {
            let marker = format!("\n\n<project_instructions path=\"{path}\">\n");
            let start = text.find(&marker).unwrap();
            let end = start
                + text[start..].find("\n</project_instructions>").unwrap()
                + "\n</project_instructions>".len();
            text.replace_range(start..end, "");
        }
    }
    text.replace(
        "<project_context>\nProject-specific instructions and guidelines:\n</project_context>\n\n",
        "",
    )
}

#[tokio::test]
async fn golden_discovery_and_rendering() {
    let golden: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/golden.json")).unwrap();
    for case in golden["cases"].as_array().unwrap() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let cwd = root.join(case["cwd"].as_str().unwrap());
        let home = root.join(case["home"].as_str().unwrap());
        fs::create_dir_all(&cwd).unwrap();
        fs::create_dir_all(&home).unwrap();
        if let Some(dirs) = case["dirs"].as_array() {
            for dir in dirs {
                fs::create_dir_all(root.join(dir.as_str().unwrap())).unwrap();
            }
        }
        for (name, content) in case["files"].as_object().unwrap() {
            write(&root, name, content.as_str().unwrap());
        }
        #[cfg(unix)]
        if let Some(links) = case["symlinks"].as_object() {
            for (name, target) in links {
                std::os::unix::fs::symlink(root.join(target.as_str().unwrap()), root.join(name))
                    .unwrap();
            }
        }
        #[cfg(not(unix))]
        if case.get("symlinks").is_some() {
            continue;
        }
        let (module, diagnostics) =
            ContextFilesModule::load(&cwd, &home, &CancellationToken::new()).unwrap();
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            render_fixture(&module, &cwd, &root)
                .await
                .replace(std::path::MAIN_SEPARATOR, "/")
                .replace(
                    &root
                        .to_str()
                        .unwrap()
                        .replace(std::path::MAIN_SEPARATOR, "/"),
                    "{{ROOT}}",
                ),
            case["fragment"].as_str().unwrap(),
            "{}",
            case["name"]
        );
    }
}

#[tokio::test]
#[cfg(unix)]
async fn discovery_and_audit_preserve_lexical_paths_through_directory_symlinks() {
    let temp = TempDir::new().unwrap();
    write(temp.path(), "real/home/AGENTS.md", "global");
    write(temp.path(), "real/project/AGENTS.md", "project");
    std::os::unix::fs::symlink(temp.path().join("real"), temp.path().join("alias")).unwrap();
    let root = temp.path().join("alias");
    let cwd = root.join("project/../project");
    let home = root.join("home/../home");
    let (module, diagnostics) =
        ContextFilesModule::load(&cwd, &home, &CancellationToken::new()).unwrap();
    assert!(diagnostics.is_empty());
    assert_eq!(
        fixture_audit(&module, &root)["files"],
        serde_json::json!([
            { "path": root.join("home/AGENTS.md"), "bytes": 6 },
            { "path": root.join("project/AGENTS.md"), "bytes": 7 },
        ])
    );
    let text = render(&module, &cwd).await;
    for file in fixture_audit(&module, &root)["files"].as_array().unwrap() {
        assert!(text.contains(file["path"].as_str().unwrap()));
    }
}

#[tokio::test]
async fn every_load_rediscovers_files_and_audit_contains_only_paths_and_byte_counts() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("project");
    write(temp.path(), "home/AGENTS.md", "global");
    write(temp.path(), "project/AGENTS.md", "original project");
    let cancel = CancellationToken::new();
    let (module, _) = ContextFilesModule::load(&cwd, &home, &cancel).unwrap();
    assert_eq!(
        fixture_audit(&module, temp.path()),
        serde_json::json!({
            "version": 2,
            "files": [
                { "path": home.join("AGENTS.md"), "bytes": 6 },
                { "path": cwd.join("AGENTS.md"), "bytes": 16 },
            ],
            "truncated": false,
        })
    );
    write(temp.path(), "project/AGENTS.md", "modified project");
    let (modified, _) = ContextFilesModule::load(&cwd, &home, &cancel).unwrap();
    assert!(render(&modified, &cwd).await.contains("modified project"));
    assert_eq!(module.entry_data(), modified.entry_data());
    write(temp.path(), "project/AGENTS.override.md", "new project");
    fs::remove_file(cwd.join("AGENTS.md")).unwrap();
    let (reloaded, _) = ContextFilesModule::load(&cwd, &home, &cancel).unwrap();
    assert!(render(&reloaded, &cwd).await.contains("new project"));
    assert!(!render(&reloaded, &cwd).await.contains("original project"));
    assert_ne!(module.entry_data(), reloaded.entry_data());
    fs::remove_file(cwd.join("AGENTS.override.md")).unwrap();
    let (removed, _) = ContextFilesModule::load(&cwd, &home, &cancel).unwrap();
    assert_eq!(
        fixture_audit(&removed, temp.path())["files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut registry = Registry::default();
    reloaded
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    assert_eq!(registry.context_providers().len(), 1);
    assert!(registry.tools().specs().is_empty());
    assert!(registry.session_observers().is_empty());
}

#[tokio::test]
async fn aggregate_limit_is_utf8_safe_and_invalid_files_are_diagnosed() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("project/sub");
    write(temp.path(), "home/AGENTS.md", "g".repeat(MAX_BYTES - 2));
    write(temp.path(), "project/AGENTS.md", "中文😀");
    write(temp.path(), "project/sub/AGENTS.md", "omitted");
    let cancel = CancellationToken::new();
    let (module, diagnostics) = ContextFilesModule::load(&cwd, &home, &cancel).unwrap();
    assert!(diagnostics.is_empty());
    let text = render(&module, &cwd).await;
    assert!(text.contains("[Context files truncated: 32768 byte limit reached.]"));
    assert!(!text.contains("omitted"));
    assert!(!text.contains('\u{fffd}'));
    let data = module.entry_data();
    let bytes: usize = data["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["bytes"].as_u64().unwrap() as usize)
        .sum();
    assert!(bytes <= MAX_BYTES);

    // Invalid data beyond the retained prefix must still cause the whole file to be skipped.
    let mut invalid = vec![b'x'; MAX_BYTES + 20];
    invalid.push(255);
    write(temp.path(), "home/AGENTS.md", invalid);
    let (module, diagnostics) = ContextFilesModule::load(&cwd, &home, &cancel).unwrap();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].path, home.join("AGENTS.md"));
    assert!(diagnostics[0].message.contains("non-UTF-8"));
    assert!(render(&module, &cwd).await.contains("中文😀"));
    assert!(render(&module, &cwd).await.contains("omitted"));
}

#[tokio::test]
async fn chunk_boundaries_preserve_bom_crlf_and_unicode_and_cancellation() {
    let temp = TempDir::new().unwrap();
    let cwd = temp.path().join("project");
    let home = temp.path().join("home");
    let text = format!("\u{feff}{}😀\r\n", "x".repeat(8190));
    write(temp.path(), "project/CLAUDE.MD", &text);
    let cancel = CancellationToken::new();
    let (module, _) = ContextFilesModule::load(&cwd, &home, &cancel).unwrap();
    assert!(render(&module, &cwd).await.contains(&text[3..]));
    cancel.cancel();
    assert!(ContextFilesModule::load(&cwd, &home, &cancel).is_err());
    assert!(
        module
            .fragments(
                &TurnInput {
                    cwd,
                    content: vec![]
                },
                cancel
            )
            .await
            .is_err()
    );
}
