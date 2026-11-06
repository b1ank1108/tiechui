#![allow(clippy::unwrap_used)]
use std::{fs, path::Path, process::Command};
use tiechui_module_plugins::{content_hash, discover, marketplace};
use tiechui_plugin_api::{CancellationToken, ModuleConfig, serde_json::json};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn commit(root: &Path, parent: Option<&str>) -> String {
    // Plumbing keeps fixture commits independent of user hooks, signing and identity.
    for path in [
        "marketplace.json",
        "package/.codex-plugin/plugin.json",
        "package/value.txt",
    ] {
        let blob = git(root, &["hash-object", "-w", path]);
        git(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{path}"),
            ],
        );
    }
    let tree = git(root, &["write-tree"]);
    let mut args = vec!["commit-tree", &tree, "-m", "fixture"];
    if let Some(parent) = parent {
        args.extend(["-p", parent]);
    }
    let commit = git(root, &args);
    git(root, &["update-ref", "refs/heads/main", &commit]);
    commit
}

fn fixture() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/marketplace-tests");
    fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(root).unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir_all(repo.join("package/.codex-plugin")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("marketplace.json"), json!({"name":"fixture-market", "plugins":[{"name":"demo", "source":{"source":"local","path":"./package"}}]}).to_string()).unwrap();
    fs::write(
        repo.join("package/.codex-plugin/plugin.json"),
        r#"{"name":"demo","version":"1"}"#,
    )
    .unwrap();
    fs::write(repo.join("package/value.txt"), "one").unwrap();
    temp
}

#[tokio::test]
async fn pins_commits_moved_tags_explicit_diffs_revokes_grants_and_removes_marketplace() {
    let temp = fixture();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let cancel = CancellationToken::new();
    let first = commit(&repo, None);
    git(&repo, &["tag", "stable", &first]);
    let market = marketplace::add(
        repo.to_str().unwrap(),
        Some("stable"),
        temp.path(),
        &home,
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(market.commit, first);
    assert_eq!(marketplace::list(&home, &cancel).unwrap().len(), 1);
    fs::write(repo.join("package/value.txt"), "two").unwrap();
    let second = commit(&repo, Some(&first));
    git(&repo, &["tag", "-f", "stable", &second]);
    let installed = marketplace::install("demo@fixture-market", &home, false, &cancel)
        .await
        .unwrap();
    assert_eq!(installed.new_commit, first);
    assert_eq!(
        fs::read_to_string(home.join("plugins/demo/value.txt")).unwrap(),
        "one"
    );
    assert!(!home.join("plugins/demo/.git").exists());
    let record = discover(temp.path(), &home, false, &ModuleConfig::default(), &cancel)
        .unwrap()
        .plugins
        .remove(0)
        .installation
        .unwrap();
    assert_eq!(
        record.content_hash,
        content_hash(&home.join("plugins/demo"), &cancel).unwrap()
    );
    assert_eq!(record.marketplace.unwrap().commit, first);
    fs::create_dir_all(home.join("plugin-grants")).unwrap();
    fs::write(home.join("plugin-grants/demo.json"), "old grant").unwrap();
    let changes = marketplace::upgrade(Some("demo"), &home, &cancel)
        .await
        .unwrap();
    assert_eq!(changes[0].old_commit, first);
    assert_eq!(changes[0].new_commit, second);
    assert_ne!(changes[0].old_hash, changes[0].new_hash);
    assert_eq!(changes[0].changes, ["M value.txt"]);
    assert!(!home.join("plugin-grants/demo.json").exists());
    assert_eq!(
        fs::read_to_string(home.join("plugins/demo/value.txt")).unwrap(),
        "two"
    );
    let unchanged = marketplace::upgrade(None, &home, &cancel).await.unwrap();
    assert!(unchanged[0].changes.is_empty());
    assert_eq!(unchanged[0].old_hash, unchanged[0].new_hash);
    marketplace::remove(repo.to_str().unwrap(), temp.path(), &home, &cancel).unwrap();
    assert!(marketplace::list(&home, &cancel).unwrap().is_empty());
    assert!(repo.join(".git").exists());
    assert!(home.join("plugins/demo/value.txt").exists());
    assert!(marketplace::upgrade(None, &home, &cancel).await.is_err());
    tiechui_module_plugins::remove("demo", &home, &cancel).unwrap();
    assert!(!home.join("plugins/.records/demo.json").exists());
}

#[tokio::test]
async fn codex_layout_string_and_git_subdir_sources_and_pinned_sha() {
    let temp = fixture();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let cancel = CancellationToken::new();
    let first = commit(&repo, None);
    let catalog_repo = temp.path().join("catalog");
    fs::create_dir_all(catalog_repo.join(".agents/plugins")).unwrap();
    git(&catalog_repo, &["init", "-q", "-b", "main"]);
    let path = ".agents/plugins/marketplace.json";
    fs::write(catalog_repo.join(path), json!({"name":"external", "plugins":[{"name":"demo", "source":{"source":"git-subdir", "url":repo,"path":"package","sha":first}}]}).to_string()).unwrap();
    let blob = git(&catalog_repo, &["hash-object", "-w", path]);
    git(
        &catalog_repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},{path}"),
        ],
    );
    let tree = git(&catalog_repo, &["write-tree"]);
    let head = git(&catalog_repo, &["commit-tree", &tree, "-m", "fixture"]);
    git(&catalog_repo, &["update-ref", "refs/heads/main", &head]);
    marketplace::add(
        catalog_repo.to_str().unwrap(),
        None,
        temp.path(),
        &home,
        &cancel,
    )
    .await
    .unwrap();
    let installed = marketplace::install("demo@external", &home, false, &cancel)
        .await
        .unwrap();
    assert_eq!(installed.new_commit, first);
    fs::write(repo.join("package/value.txt"), "new").unwrap();
    commit(&repo, Some(&first));
    assert_eq!(
        marketplace::upgrade(None, &home, &cancel).await.unwrap()[0].new_commit,
        first
    );
    // The legacy path string accepted by Codex uses the same repository-relative rules.
    fs::write(
        repo.join("marketplace.json"),
        r#"{"name":"strings","plugins":[{"name":"demo","source":"./package"},{"name":"npm-only","source":{"source":"npm","package":"ignored"}},{"name":"fallback","version":"2","source":"./raw"}]}"#,
    )
    .unwrap();
    fs::create_dir(repo.join("raw")).unwrap();
    fs::write(repo.join("raw/README.md"), "declarative fixture").unwrap();
    let blob = git(&repo, &["hash-object", "-w", "raw/README.md"]);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},raw/README.md"),
        ],
    );
    commit(&repo, None);
    marketplace::add(repo.to_str().unwrap(), None, temp.path(), &home, &cancel)
        .await
        .unwrap();
    tiechui_module_plugins::remove("demo", &home, &cancel).unwrap();
    marketplace::install("demo@strings", &home, false, &cancel)
        .await
        .unwrap();
    marketplace::install("fallback@strings", &home, false, &cancel)
        .await
        .unwrap();
    assert!(
        home.join("plugins/fallback/.codex-plugin/plugin.json")
            .exists()
    );
    assert!(
        marketplace::install("npm-only@strings", &home, false, &cancel)
            .await
            .unwrap_err()
            .0
            .contains("unsupported")
    );
}

#[tokio::test]
async fn invalid_catalog_paths_duplicate_names_and_cancellation_fail_without_installing() {
    for source in [
        json!("../outside"),
        json!({"source":"local","path":"/tmp"}),
        json!({"source":"npm","package":"unwanted"}),
    ] {
        let temp = fixture();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        let cancel = CancellationToken::new();
        fs::write(
            repo.join("marketplace.json"),
            json!({"name":"bad","plugins":[{"name":"demo","source":source}]}).to_string(),
        )
        .unwrap();
        commit(&repo, None);
        if marketplace::add(repo.to_str().unwrap(), None, temp.path(), &home, &cancel)
            .await
            .is_ok()
        {
            assert!(
                marketplace::install("demo@bad", &home, false, &cancel)
                    .await
                    .is_err()
            );
        }
        assert!(!home.join("plugins/demo").exists());
    }
    let temp = fixture();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let cancel = CancellationToken::new();
    fs::write(repo.join("marketplace.json"), r#"{"name":"bad","plugins":[{"name":"demo","source":"./package"},{"name":"demo","source":"./package"}]}"#).unwrap();
    commit(&repo, None);
    assert!(
        marketplace::add(repo.to_str().unwrap(), None, temp.path(), &home, &cancel)
            .await
            .is_err()
    );
    cancel.cancel();
    assert!(
        marketplace::add(repo.to_str().unwrap(), None, temp.path(), &home, &cancel)
            .await
            .is_err()
    );
    assert!(
        marketplace::list(&home, &CancellationToken::new())
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn fallback_manifest_cannot_write_through_a_repository_symlink() {
    let temp = fixture();
    let repo = temp.path().join("repo");
    let home = temp.path().join("home");
    let first = commit(&repo, None);
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::remove_dir_all(repo.join("package/.codex-plugin")).unwrap();
    git(
        &repo,
        &[
            "update-index",
            "--force-remove",
            "package/.codex-plugin/plugin.json",
        ],
    );
    std::os::unix::fs::symlink(&outside, repo.join("package/.codex-plugin")).unwrap();
    let target = repo.join("link-target");
    fs::write(&target, outside.to_str().unwrap()).unwrap();
    let blob = git(&repo, &["hash-object", "-w", "link-target"]);
    git(
        &repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{blob},package/.codex-plugin"),
        ],
    );
    let tree = git(&repo, &["write-tree"]);
    let head = git(
        &repo,
        &["commit-tree", &tree, "-p", &first, "-m", "symlink fixture"],
    );
    git(&repo, &["update-ref", "refs/heads/main", &head]);
    let cancel = CancellationToken::new();
    marketplace::add(repo.to_str().unwrap(), None, temp.path(), &home, &cancel)
        .await
        .unwrap();
    assert!(
        marketplace::install("demo@fixture-market", &home, false, &cancel)
            .await
            .unwrap_err()
            .0
            .contains("links")
    );
    assert!(!outside.join("plugin.json").exists());
    assert!(!home.join("plugins/demo").exists());
}
