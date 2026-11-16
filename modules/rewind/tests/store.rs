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

#[test]
fn allows_working_subdirectories_under_agent_dirs_while_blocking_metadata() {
    let (_root, store, cancel) = fixture();
    let allowed = [
        ".tiechui/worktrees/x/a.txt",
        ".tiechui/plans/plan.md",
        ".tiechui/state/cache.json",
        ".claude/worktrees/x/a.txt",
        ".claude/plans/plan.md",
        ".claude/state/cache.json",
        ".agents/worktrees/x/a.txt",
        ".agents/plans/plan.md",
        ".agents/state/cache.json",
    ];
    for file in allowed {
        let full = store.root.join(file);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        store.before("one", file, &cancel).unwrap();
        fs::write(&full, "created\n").unwrap();
        store.after(file, &cancel).unwrap();
    }
    let plan = store.plan(&["one".into()], &cancel).unwrap();
    assert_eq!(plan.target.len(), allowed.len());
    let _lock = store.restore(&plan, &cancel).unwrap();
    store.finish(&plan.target).unwrap();
    for file in allowed {
        assert!(!store.root.join(file).exists(), "{file}");
    }

    for blocked in [
        ".tiechui/config.toml",
        ".tiechui/worktrees",
        ".tiechui/plans",
        ".tiechui/state",
        ".claude/settings.json",
        ".claude/agents/reviewer.md",
        ".agents/config.toml",
        ".agents/agents/reviewer.md",
        ".tiechui/worktrees/x/.git/config",
        ".tiechui/worktrees/x/.claude/settings.json",
        ".tiechui/worktrees/x/.agents/config.toml",
        ".tiechui/worktrees/x/.tiechui/config.toml",
    ] {
        assert!(store.before("two", blocked, &cancel).is_err(), "{blocked}");
    }
}

#[test]
fn snapshot_records_persist_and_inherit_across_forked_sessions_without_file_checkpoints() {
    use tiechui_module_rewind::store::SnapshotRecord;

    let (root, parent, cancel) = fixture();
    let ready = SnapshotRecord::Ready {
        reference: "refs/tiechui/checkpoints/session/turn-1".into(),
        tree: "0123456789abcdef0123456789abcdef01234567".into(),
    };
    let missing = SnapshotRecord::Missing {
        reason: "git snapshot timed out after 100ms".into(),
    };
    parent
        .record_snapshot("turn-1", ready.clone(), &cancel)
        .unwrap();
    parent
        .record_snapshot("turn-2", missing.clone(), &cancel)
        .unwrap();

    let reloaded = Store::new(&root.path().join("state"), &parent.root, "session").unwrap();
    assert_eq!(reloaded.snapshot("turn-1").unwrap(), Some(ready.clone()));
    assert_eq!(reloaded.snapshot("turn-2").unwrap(), Some(missing));
    assert_eq!(reloaded.snapshot("turn-3").unwrap(), None);
    assert_eq!(reloaded.snapshots().unwrap().len(), 2);

    let child = Store::new(&root.path().join("state"), &parent.root, "forked").unwrap();
    child
        .inherit(&reloaded, &["turn-1".into()], &cancel)
        .unwrap();
    assert_eq!(child.snapshot("turn-1").unwrap(), Some(ready));
    assert_eq!(child.snapshot("turn-2").unwrap(), None);
}

#[test]
fn tiechui_home_resolution_handles_production_and_test_rewind_directories() {
    use tiechui_module_rewind::tiechui_home_from_rewind_dir;

    let temp = tempfile::tempdir().unwrap();
    let prod_home = temp.path().join("prod-home");
    assert_eq!(
        tiechui_home_from_rewind_dir(&prod_home.join("state/rewind")),
        Some(prod_home)
    );

    let test_home = temp.path().join("test-home");
    assert_eq!(
        tiechui_home_from_rewind_dir(&test_home.join("rewind")),
        Some(test_home)
    );

    let state_named_home = temp.path().join("state");
    fs::create_dir_all(state_named_home.join("worktrees")).unwrap();
    fs::write(state_named_home.join("worktrees/registry.json"), "{}").unwrap();
    assert_eq!(
        tiechui_home_from_rewind_dir(&state_named_home.join("rewind")),
        Some(state_named_home)
    );
}

#[test]
fn settle_snapshot_restore_keeps_retained_turn_diffs_and_reconciles_discarded_head() {
    use std::collections::BTreeMap;
    use std::process::Command;
    use tiechui_module_rewind::store::SnapshotRecord;

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    for args in [
        &["init", "-b", "main"][..],
        &["config", "user.name", "Tiechui"][..],
        &["config", "user.email", "tiechui@example.com"][..],
    ] {
        assert!(
            Command::new("git")
                .current_dir(&repo)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(repo.join("a.txt"), "initial-a\n").unwrap();
    fs::write(repo.join("kept.txt"), "v0\n").unwrap();
    assert!(
        Command::new("git")
            .current_dir(&repo)
            .args(["add", "."])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .current_dir(&repo)
            .args(["commit", "-m", "init"])
            .status()
            .unwrap()
            .success()
    );

    let wt =
        tiechui_workspace::create_worktree_with_env(&home, &repo, None, &BTreeMap::new()).unwrap();
    let wt_path = std::path::PathBuf::from(&wt.path);
    let cancel = CancellationToken::new();
    let store = Store::new(&home.join("state/rewind"), &wt_path, "sess-1").unwrap();

    store.before("turn-1", "kept.txt", &cancel).unwrap();
    fs::write(wt_path.join("kept.txt"), "v1\n").unwrap();
    store.after("kept.txt", &cancel).unwrap();

    let snap2 = tiechui_workspace::capture_worktree_snapshot(&wt_path, "sess-1", "turn-2").unwrap();
    store
        .record_snapshot(
            "turn-2",
            SnapshotRecord::Ready {
                reference: snap2.reference.clone(),
                tree: snap2.tree.clone(),
            },
            &cancel,
        )
        .unwrap();

    store.before("turn-2", "kept.txt", &cancel).unwrap();
    fs::write(wt_path.join("kept.txt"), "v2\n").unwrap();
    store.after("kept.txt", &cancel).unwrap();
    store.before("turn-2", "temp.txt", &cancel).unwrap();
    fs::write(wt_path.join("temp.txt"), "created\n").unwrap();
    store.after("temp.txt", &cancel).unwrap();

    fs::remove_file(wt_path.join("a.txt")).unwrap();
    fs::write(wt_path.join("bash_only.txt"), "from-bash\n").unwrap();
    fs::write(wt_path.join("external.txt"), "manual\n").unwrap();

    let (safe, unsafe_files) = store.patch_preview(&["turn-2".into()], &cancel).unwrap();
    assert_eq!(safe, vec!["kept.txt".to_owned(), "temp.txt".to_owned()]);
    assert!(unsafe_files.is_empty());

    store.ensure_ready_for_restore(&cancel).unwrap();
    tiechui_workspace::restore_worktree_snapshot(&wt_path, &snap2.tree).unwrap();
    store
        .settle_snapshot_restore(&["turn-2".into()], &cancel)
        .unwrap();

    assert_eq!(
        fs::read_to_string(wt_path.join("a.txt")).unwrap(),
        "initial-a\n"
    );
    assert!(!wt_path.join("bash_only.txt").exists());
    assert!(!wt_path.join("external.txt").exists());
    assert!(!wt_path.join("temp.txt").exists());
    assert_eq!(
        fs::read_to_string(wt_path.join("kept.txt")).unwrap(),
        "v1\n"
    );

    let retained = store.turn(&["turn-1".into()], &[], &cancel).unwrap();
    assert!(!retained.reverted);
    assert_eq!(retained.files.len(), 1);
    let undone = store.toggle(&["turn-1".into()], &[], &cancel).unwrap();
    assert!(undone.reverted);
    assert_eq!(
        fs::read_to_string(wt_path.join("kept.txt")).unwrap(),
        "v0\n"
    );
    let redone = store.toggle(&["turn-1".into()], &[], &cancel).unwrap();
    assert!(!redone.reverted);
    assert_eq!(
        fs::read_to_string(wt_path.join("kept.txt")).unwrap(),
        "v1\n"
    );
}

#[tokio::test]
async fn rewind_command_in_managed_worktree_restores_snapshot_conversation_first_and_enforces_isolation()
 {
    use std::collections::{BTreeMap, VecDeque};
    use std::process::Command as SysCommand;
    use std::sync::{Arc, Mutex};
    use tiechui_module_rewind::{RewindModule, store::SnapshotRecord};
    use tiechui_plugin_api::{
        BranchView, CommandHost, CommandSession, Module, ModuleConfig, Registry, SessionEffect,
        ToolError, async_trait,
        protocol::{
            ApprovalDecision, ApprovalRequest, BranchEntry, InputContent, RewindPage, RewindPanel,
        },
        serde_json::json,
    };

    struct TestHost {
        answers: Mutex<VecDeque<ApprovalDecision>>,
        fail_navigate: bool,
        effects: Mutex<Vec<String>>,
        last_panel: Mutex<Option<RewindPanel>>,
        actions_ready: tokio::sync::Notify,
        wt_check_file: std::path::PathBuf,
        a_existed_at_navigate: Mutex<Option<bool>>,
    }

    #[async_trait]
    impl CommandHost for TestHost {
        async fn ask(
            &self,
            request: ApprovalRequest,
            _: CancellationToken,
        ) -> Result<ApprovalDecision, ToolError> {
            let mut wait_for_actions = false;
            if let ApprovalRequest::Rewind { panel } = request {
                wait_for_actions = panel.page == RewindPage::Checking;
                *self.last_panel.lock().unwrap() = Some(panel);
            }
            if wait_for_actions {
                self.actions_ready.notified().await;
            }
            Ok(self
                .answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(ApprovalDecision::Cancel))
        }

        async fn update(&self, request: ApprovalRequest) -> Result<(), ToolError> {
            if let ApprovalRequest::Rewind { panel } = request {
                let is_actions = panel.page == RewindPage::Actions;
                *self.last_panel.lock().unwrap() = Some(panel);
                if is_actions {
                    self.actions_ready.notify_waiters();
                }
            }
            Ok(())
        }

        async fn apply(&self, effect: SessionEffect) -> Result<(), ToolError> {
            match effect {
                SessionEffect::Navigate { entry_id } => {
                    *self.a_existed_at_navigate.lock().unwrap() = Some(self.wt_check_file.exists());
                    self.effects.lock().unwrap().push(format!("nav:{entry_id}"));
                    if self.fail_navigate {
                        return Err(ToolError("simulated navigate failure".into()));
                    }
                }
                SessionEffect::Refill { .. } => {
                    self.effects.lock().unwrap().push("refill".into());
                }
                SessionEffect::Quiesce => {
                    self.effects.lock().unwrap().push("quiesce".into());
                }
                SessionEffect::Validate { .. } => {
                    self.effects.lock().unwrap().push("validate".into());
                }
                SessionEffect::Emit { kind, .. } => {
                    self.effects.lock().unwrap().push(format!("emit:{kind}"));
                }
                _ => {}
            }
            Ok(())
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("cmd-repo");
    fs::create_dir_all(&repo).unwrap();
    for args in [
        &["init", "-b", "main"][..],
        &["config", "user.name", "Tiechui"][..],
        &["config", "user.email", "tiechui@example.com"][..],
    ] {
        assert!(
            SysCommand::new("git")
                .current_dir(&repo)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }
    fs::write(repo.join("a.txt"), "initial-a\n").unwrap();
    assert!(
        SysCommand::new("git")
            .current_dir(&repo)
            .args(["add", "."])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        SysCommand::new("git")
            .current_dir(&repo)
            .args(["commit", "-m", "init"])
            .status()
            .unwrap()
            .success()
    );

    let wt =
        tiechui_workspace::create_worktree_with_env(&home, &repo, None, &BTreeMap::new()).unwrap();
    let wt_path = std::path::PathBuf::from(&wt.path);
    let rewind_dir = home.join("state/rewind");
    let cancel = CancellationToken::new();
    let store = Store::new(&rewind_dir, &wt_path, "sess-cmd").unwrap();
    let snap = tiechui_workspace::capture_worktree_snapshot(&wt_path, "sess-cmd", "u2").unwrap();
    store
        .record_snapshot(
            "u2",
            SnapshotRecord::Ready {
                reference: snap.reference,
                tree: snap.tree,
            },
            &cancel,
        )
        .unwrap();

    fs::remove_file(wt_path.join("a.txt")).unwrap();
    fs::write(wt_path.join("b.txt"), "x\n").unwrap();

    let mut registry = Registry::default();
    RewindModule {
        directory: rewind_dir.clone(),
    }
    .register(&mut registry, &ModuleConfig::default())
    .unwrap();
    let cmd = registry.commands()[0].clone();

    let branch = BranchView {
        session_id: "sess-cmd".into(),
        cwd: wt_path.clone(),
        parent_session: None,
        leaf: "u2".into(),
        entries: vec![
            BranchEntry {
                id: "u1".into(),
                parent_id: None,
                timestamp: 1,
                content: Some(vec![InputContent::Text {
                    text: "first".into(),
                }]),
                custom: None,
            },
            BranchEntry {
                id: "u2".into(),
                parent_id: Some("u1".into()),
                timestamp: 2,
                content: Some(vec![InputContent::Text {
                    text: "second".into(),
                }]),
                custom: None,
            },
        ],
    };

    let failing_host = Arc::new(TestHost {
        answers: Mutex::new(VecDeque::from([
            ApprovalDecision::Accept {
                content: Some(
                    json!({"session_id": "sess-cmd", "entry_id": "u2", "action": "preview"}),
                ),
            },
            ApprovalDecision::Accept {
                content: Some(
                    json!({"session_id": "sess-cmd", "entry_id": "u2", "action": "both"}),
                ),
            },
        ])),
        fail_navigate: true,
        effects: Mutex::new(Vec::new()),
        last_panel: Mutex::new(None),
        actions_ready: tokio::sync::Notify::new(),
        wt_check_file: wt_path.join("a.txt"),
        a_existed_at_navigate: Mutex::new(None),
    });
    let err = cmd
        .session_run(
            "",
            CommandSession {
                branch: branch.clone(),
                host: failing_host.clone(),
            },
            cancel.clone(),
        )
        .await
        .unwrap_err();
    assert!(err.0.contains("Files: unchanged"), "unexpected err: {err}");
    assert!(!wt_path.join("a.txt").exists());
    assert!(wt_path.join("b.txt").exists());

    let peer_dir = home.join("sessions/bucket");
    fs::create_dir_all(&peer_dir).unwrap();
    let peer_file = peer_dir.join("peer.jsonl");
    fs::write(
        &peer_file,
        format!(
            "{}\n",
            json!({"format_version":1,"id":"peer","created_at":1,"cwd":wt_path})
        ),
    )
    .unwrap();
    let blocked_host = Arc::new(TestHost {
        answers: Mutex::new(VecDeque::from([
            ApprovalDecision::Accept {
                content: Some(
                    json!({"session_id": "sess-cmd", "entry_id": "u2", "action": "preview"}),
                ),
            },
            ApprovalDecision::Accept {
                content: Some(
                    json!({"session_id": "sess-cmd", "entry_id": "u2", "action": "both"}),
                ),
            },
        ])),
        fail_navigate: false,
        effects: Mutex::new(Vec::new()),
        last_panel: Mutex::new(None),
        actions_ready: tokio::sync::Notify::new(),
        wt_check_file: wt_path.join("a.txt"),
        a_existed_at_navigate: Mutex::new(None),
    });
    assert!(
        cmd.session_run(
            "",
            CommandSession {
                branch: branch.clone(),
                host: blocked_host.clone(),
            },
            cancel.clone(),
        )
        .await
        .is_err()
    );
    assert!(
        blocked_host
            .last_panel
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|p| p.preview.as_ref())
            .and_then(|s| s.error.as_ref())
            .is_some()
    );
    assert!(!wt_path.join("a.txt").exists());
    fs::remove_file(&peer_file).unwrap();

    let ok_host = Arc::new(TestHost {
        answers: Mutex::new(VecDeque::from([
            ApprovalDecision::Accept {
                content: Some(
                    json!({"session_id": "sess-cmd", "entry_id": "u2", "action": "preview"}),
                ),
            },
            ApprovalDecision::Accept {
                content: Some(
                    json!({"session_id": "sess-cmd", "entry_id": "u2", "action": "both"}),
                ),
            },
        ])),
        fail_navigate: false,
        effects: Mutex::new(Vec::new()),
        last_panel: Mutex::new(None),
        actions_ready: tokio::sync::Notify::new(),
        wt_check_file: wt_path.join("a.txt"),
        a_existed_at_navigate: Mutex::new(None),
    });
    let out = cmd
        .session_run(
            "",
            CommandSession {
                branch: branch.clone(),
                host: ok_host.clone(),
            },
            cancel.clone(),
        )
        .await
        .unwrap();
    assert!(out.contains("Files: restored"), "out: {out}");
    assert_eq!(*ok_host.a_existed_at_navigate.lock().unwrap(), Some(false));
    assert_eq!(
        fs::read_to_string(wt_path.join("a.txt")).unwrap(),
        "initial-a\n"
    );
    assert!(!wt_path.join("b.txt").exists());

    store.before("u1", "a.txt", &cancel).unwrap();
    fs::write(wt_path.join("a.txt"), "patched-u1\n").unwrap();
    store.after("a.txt", &cancel).unwrap();
    store
        .record_snapshot(
            "u1",
            SnapshotRecord::Missing {
                reason: "snapshot timed out".into(),
            },
            &cancel,
        )
        .unwrap();
    let fallback_host = Arc::new(TestHost {
        answers: Mutex::new(VecDeque::from([
            ApprovalDecision::Accept {
                content: Some(
                    json!({"session_id": "sess-cmd", "entry_id": "u1", "action": "preview"}),
                ),
            },
            ApprovalDecision::Accept {
                content: Some(
                    json!({"session_id": "sess-cmd", "entry_id": "u1", "action": "both"}),
                ),
            },
        ])),
        fail_navigate: false,
        effects: Mutex::new(Vec::new()),
        last_panel: Mutex::new(None),
        actions_ready: tokio::sync::Notify::new(),
        wt_check_file: wt_path.join("a.txt"),
        a_existed_at_navigate: Mutex::new(None),
    });
    let fallback_out = cmd
        .session_run(
            "",
            CommandSession {
                branch,
                host: fallback_host,
            },
            cancel,
        )
        .await
        .unwrap();
    assert!(
        fallback_out.contains("Used patch rewind (snapshot timed out)"),
        "fallback_out: {fallback_out}"
    );
    assert_eq!(
        fs::read_to_string(wt_path.join("a.txt")).unwrap(),
        "initial-a\n"
    );
}
