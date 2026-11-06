#![cfg(unix)]
#![allow(clippy::unwrap_used)]
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
use tiechui_module_rewind::store::{Store, line_stats};
use tiechui_plugin_api::{CancellationToken, protocol::LineStats};

fn fixture() -> (tempfile::TempDir, Store, CancellationToken) {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("work")).unwrap();
    let store = Store::new(
        &root.path().join("state"),
        &root.path().join("work"),
        "session",
    )
    .unwrap();
    (root, store, CancellationToken::new())
}
fn edit(store: &Store, id: &str, file: &str, text: &str, cancel: &CancellationToken) {
    store.before(id, file, cancel).unwrap();
    fs::write(store.root.join(file), text).unwrap();
    store.after(file, cancel).unwrap();
}
#[test]
fn repeated_edits_reload_new_files_and_private_hashed_backups() {
    let (_root, store, cancel) = fixture();
    fs::write(store.root.join("a"), "original\n").unwrap();
    for (id, text) in [("one", "first"), ("one", "second"), ("two", "third")] {
        edit(&store, id, "a", text, &cancel);
    }
    edit(&store, "two", "new", "new", &cancel);
    let reload = store.clone();
    let plan = reload.plan(&["one".into(), "two".into()], &cancel).unwrap();
    let blob = store
        .directory
        .join("blobs")
        .join(&plan.target["a"].as_ref().unwrap().hash);
    assert_eq!(
        fs::metadata(blob).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&store.directory).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let _guard = reload.restore(&plan, &cancel).unwrap();
    reload.finish(&plan.target).unwrap();
    assert_eq!(
        fs::read_to_string(store.root.join("a")).unwrap(),
        "original\n"
    );
    assert!(!store.root.join("new").exists());
    assert!(!reload.pending().unwrap());
}
#[test]
fn refuses_traversal_metadata_links_and_oversized_files() {
    let (root, store, cancel) = fixture();
    fs::write(store.root.join("a"), "original").unwrap();
    symlink(store.root.join("a"), store.root.join("sym")).unwrap();
    fs::hard_link(store.root.join("a"), store.root.join("hard")).unwrap();
    symlink(root.path(), store.root.join("parent")).unwrap();
    for file in [
        "../outside",
        "/",
        "",
        ".git/config",
        ".pi/auth.json",
        ".agents/config.toml",
        "sym",
        "hard",
        "parent/outside",
    ] {
        assert!(store.before("one", file, &cancel).is_err(), "{file}");
    }
    fs::File::create(store.root.join("big"))
        .unwrap()
        .set_len(20 * 1024 * 1024 + 1)
        .unwrap();
    assert!(
        store
            .before("one", "big", &cancel)
            .unwrap_err()
            .0
            .contains("20 MiB")
    );
    assert!(!store.directory.join("manifest.json").exists());
}

#[test]
fn external_files_restore_across_reload_fork_and_recovery() {
    let (root, store, cancel) = fixture();
    let outside = store.root.parent().unwrap().join("outside");
    fs::create_dir(&outside).unwrap();
    let existing = outside.join("existing").to_str().unwrap().to_owned();
    let deleted = outside.join("deleted").to_str().unwrap().to_owned();
    let created = outside.join("nested/new").to_str().unwrap().to_owned();
    fs::write(&existing, "original").unwrap();
    fs::write(&deleted, "removed").unwrap();
    edit(&store, "one", &existing, "first", &cancel);
    edit(&store, "one", &existing, "second", &cancel);
    store.before("one", &deleted, &cancel).unwrap();
    fs::remove_file(&deleted).unwrap();
    store.after(&deleted, &cancel).unwrap();
    store.before("one", &created, &cancel).unwrap();
    fs::create_dir(outside.join("nested")).unwrap();
    fs::write(&created, "created").unwrap();
    store.after(&created, &cancel).unwrap();

    let reload = Store::new(&root.path().join("state"), &store.root, "session").unwrap();
    let fork = Store::new(&root.path().join("state"), &store.root, "fork").unwrap();
    fork.inherit(&reload, &["one".into()], &cancel).unwrap();
    fs::remove_dir_all(&store.directory).unwrap();
    let plan = fork.plan(&["one".into()], &cancel).unwrap();
    assert_eq!(plan.stats.files.len(), 3);
    assert!(plan.stats.files.contains_key(&existing));
    drop(fork.restore(&plan, &cancel).unwrap());
    assert_eq!(fs::read_to_string(&existing).unwrap(), "original");
    assert_eq!(fs::read_to_string(&deleted).unwrap(), "removed");
    assert!(!std::path::Path::new(&created).exists());

    let restarted = Store::new(&root.path().join("state"), &store.root, "fork").unwrap();
    restarted.recover(&cancel).unwrap();
    assert_eq!(fs::read_to_string(&existing).unwrap(), "second");
    assert!(!std::path::Path::new(&deleted).exists());
    assert_eq!(fs::read_to_string(&created).unwrap(), "created");
    assert!(!restarted.pending().unwrap());
    let plan = restarted.plan(&["one".into()], &cancel).unwrap();
    let _lock = restarted.restore(&plan, &cancel).unwrap();
    restarted.finish(&plan.target).unwrap();
    assert_eq!(fs::read_to_string(&existing).unwrap(), "original");
    assert_eq!(fs::read_to_string(&deleted).unwrap(), "removed");
    assert!(!std::path::Path::new(&created).exists());
}

#[test]
fn external_files_keep_conflict_lease_and_link_guards() {
    let (root, store, cancel) = fixture();
    let outside = store.root.parent().unwrap().join("outside");
    fs::create_dir(&outside).unwrap();
    let file = outside.join("file").to_str().unwrap().to_owned();
    fs::write(&file, "original").unwrap();
    edit(&store, "one", &file, "edited", &cancel);
    let plan = store.plan(&["one".into()], &cancel).unwrap();
    fs::write(&file, "manual edit").unwrap();
    assert!(
        store
            .plan(&["one".into()], &cancel)
            .unwrap_err()
            .0
            .contains("External")
    );
    assert!(
        store
            .restore(&plan, &cancel)
            .unwrap_err()
            .0
            .contains("since preview")
    );
    assert_eq!(fs::read_to_string(&file).unwrap(), "manual edit");
    fs::write(&file, "edited").unwrap();
    let other = Store::new(&root.path().join("state"), &store.root, "other").unwrap();
    other.before("two", &file, &cancel).unwrap();
    assert!(store.before("two", &file, &cancel).is_err());
    assert!(
        store
            .restore(&plan, &cancel)
            .unwrap_err()
            .0
            .contains("Another edit")
    );
    other.after(&file, &cancel).unwrap();

    symlink(&file, outside.join("sym")).unwrap();
    fs::hard_link(&file, outside.join("hard")).unwrap();
    for name in [
        "sym",
        "hard",
        ".git/config",
        ".pi/auth.json",
        ".agents/config.toml",
        ".claude/settings.json",
        ".tiechui/config.toml",
    ] {
        assert!(
            store
                .before("two", outside.join(name).to_str().unwrap(), &cancel)
                .is_err()
        );
    }
    fs::remove_file(outside.join("hard")).unwrap();
    let moved = outside.with_file_name("moved");
    fs::rename(&outside, &moved).unwrap();
    symlink(&moved, &outside).unwrap();
    assert!(
        store
            .restore(&plan, &cancel)
            .unwrap_err()
            .0
            .contains("Linked/non-directory parent")
    );
    assert_eq!(fs::read_to_string(moved.join("file")).unwrap(), "edited");
    assert!(!store.pending().unwrap());
}

#[test]
fn legacy_relative_checkpoints_remain_readable() {
    let (root, store, cancel) = fixture();
    fs::write(store.root.join("a"), "original").unwrap();
    edit(&store, "one", "a", "edited", &cancel);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(store.directory.join("manifest.json")).unwrap()).unwrap();
    manifest["version"] = 1.into();
    store.atomic("manifest.json", &manifest).unwrap();
    let reload = Store::new(&root.path().join("state"), &store.root, "session").unwrap();
    assert_eq!(
        reload
            .file_key(&root.path().join("work/a"), &root.path().join("work"))
            .unwrap(),
        "a"
    );
    let plan = reload.plan(&["one".into()], &cancel).unwrap();
    let _lock = reload.restore(&plan, &cancel).unwrap();
    reload.finish(&plan.target).unwrap();
    assert_eq!(
        fs::read_to_string(store.root.join("a")).unwrap(),
        "original"
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(store.directory.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["version"], 2);
}

#[cfg(target_os = "macos")]
#[test]
fn system_directory_aliases_use_the_same_checkpoint_key() {
    let (_root, store, _cancel) = fixture();
    for alias in ["/tmp", "/var", "/etc"] {
        let path = std::path::Path::new(alias).join("tiechui-rewind-fixture/new.txt");
        let canonical = fs::canonicalize(alias)
            .unwrap()
            .join("tiechui-rewind-fixture/new.txt");
        let key = store.file_key(&path, &store.root).unwrap();
        assert_eq!(key, store.file_key(&canonical, &store.root).unwrap());
        assert_eq!(store.safe_path(&key).unwrap(), canonical);
    }
}
#[test]
fn conflicts_preview_revalidation_corruption_and_leases_refuse_overwrites() {
    let (root, store, cancel) = fixture();
    fs::write(store.root.join("a"), "original").unwrap();
    edit(&store, "one", "a", "new", &cancel);
    let plan = store.plan(&["one".into()], &cancel).unwrap();
    fs::write(store.root.join("a"), "external").unwrap();
    assert!(
        store
            .plan(&["one".into()], &cancel)
            .unwrap_err()
            .0
            .contains("External")
    );
    assert!(
        store
            .restore(&plan, &cancel)
            .unwrap_err()
            .0
            .contains("since preview")
    );
    fs::write(store.root.join("a"), "new").unwrap();
    let other = Store::new(&root.path().join("state"), &store.root, "other").unwrap();
    other.before("other", "a", &cancel).unwrap();
    assert!(store.before("two", "a", &cancel).is_err());
    assert!(
        store
            .restore(&plan, &cancel)
            .unwrap_err()
            .0
            .contains("Another edit")
    );
    other.after("a", &cancel).unwrap();
    let blob = store
        .directory
        .join("blobs")
        .join(&plan.target["a"].as_ref().unwrap().hash);
    fs::write(blob, "corrupt").unwrap();
    assert!(
        store
            .restore(&plan, &cancel)
            .unwrap_err()
            .0
            .contains("Corrupt")
    );
    assert_eq!(fs::read_to_string(store.root.join("a")).unwrap(), "new");
}
#[test]
fn interrupted_restore_after_restart_recovers_only_known_versions() {
    let (_root, store, cancel) = fixture();
    fs::write(store.root.join("a"), "original").unwrap();
    edit(&store, "one", "a", "new", &cancel);
    let plan = store.plan(&["one".into()], &cancel).unwrap();
    drop(store.restore(&plan, &cancel).unwrap());
    let restarted = store.clone();
    assert!(restarted.pending().unwrap());
    assert!(restarted.before("two", "a", &cancel).is_err());
    fs::write(store.root.join("a"), "external").unwrap();
    assert!(
        restarted
            .recover(&cancel)
            .unwrap_err()
            .0
            .contains("after interrupted")
    );
    assert!(restarted.pending().unwrap());
    fs::write(store.root.join("a"), "original").unwrap();
    restarted.recover(&cancel).unwrap();
    assert_eq!(fs::read_to_string(store.root.join("a")).unwrap(), "new");
    assert!(!restarted.pending().unwrap());
}
#[test]
fn fork_copies_only_retained_checkpoints_independently_and_cancel_stops() {
    let (root, store, cancel) = fixture();
    fs::write(store.root.join("a"), "original").unwrap();
    edit(&store, "one", "a", "first", &cancel);
    edit(&store, "two", "a", "second", &cancel);
    let fork = Store::new(&root.path().join("state"), &store.root, "fork").unwrap();
    fork.inherit(&store, &["one".into()], &cancel).unwrap();
    fs::remove_dir_all(&store.directory).unwrap();
    assert!(
        fork.plan(&["two".into()], &cancel)
            .unwrap()
            .target
            .is_empty()
    );
    let plan = fork.plan(&["one".into()], &cancel).unwrap();
    let _lock = fork.restore(&plan, &cancel).unwrap();
    fork.finish(&plan.target).unwrap();
    assert_eq!(
        fs::read_to_string(store.root.join("a")).unwrap(),
        "original"
    );
    cancel.cancel();
    assert!(fork.capture("a", &cancel).is_err());
}
#[test]
fn line_counts_are_checkpoint_to_current_and_unavailable_is_not_zero() {
    assert_eq!(
        line_stats(b"a\nb\nc\n", b"a\nx\ny\nc\n"),
        Some(LineStats {
            added: 2,
            removed: 1
        })
    );
    assert_eq!(
        line_stats(b"", b"last line"),
        Some(LineStats {
            added: 1,
            removed: 0
        })
    );
    for bytes in [b"\0abc".to_vec(), vec![255], vec![b'a'; 1024 * 1024 + 1]] {
        assert_eq!(line_stats(&bytes, b""), None);
    }
}
