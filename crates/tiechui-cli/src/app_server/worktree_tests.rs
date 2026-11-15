#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};
use tiechui_model::{Content, Message, ModelId, Role};
use tiechui_protocol::app::{SessionSummary, SessionWorktree, Snapshot};
use tiechui_protocol::workspace::{Branches, WorktreeEntry, WorktreeRemoveResult};
use tiechui_session::{EntryKind, ModelRef, SessionStore};
use tokio_util::sync::CancellationToken;

use super::Server;
use super::tests::{TestClient, isolated_home_child};

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tiechui")
        .env("GIT_AUTHOR_EMAIL", "tiechui@example.com")
        .env("GIT_COMMITTER_NAME", "Tiechui")
        .env("GIT_COMMITTER_EMAIL", "tiechui@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-b", "main"]);
    std::fs::write(root.join("README.md"), "hello\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "init"]);
    git(root, &["branch", "release/v1"]);
}

fn chat_entry(role: Role, body: &str, origin: Option<ModelRef>) -> EntryKind {
    EntryKind::Message {
        message: Message {
            role,
            content: vec![Content::Text {
                text: body.into(),
                opaque: None,
            }],
            opaque: None,
        },
        usage: None,
        model: origin,
        stop_reason: None,
        reasoning: None,
        details: None,
    }
}

fn append_turn(path: &Path, prompt: &str) {
    let cancel = CancellationToken::new();
    let mut store = SessionStore::open(path, &cancel).unwrap();
    store
        .append(chat_entry(Role::User, prompt, None), &cancel)
        .unwrap();
    let origin = ModelRef {
        provider: "fixture".into(),
        model: ModelId("m1".into()),
    };
    store
        .append(chat_entry(Role::Assistant, "done", Some(origin)), &cancel)
        .unwrap();
}

#[tokio::test]
async fn worktree_rpc_and_project_session_listing_attribute_managed_worktrees_and_pass_doctor() {
    let Some(home) = isolated_home_child(
        "TC_WT_APP_SERVER_HOME",
        "app_server::worktree_tests::worktree_rpc_and_project_session_listing_attribute_managed_worktrees_and_pass_doctor",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let repo = workspace_dir.path().join("app-repo");
    let manual_wt = workspace_dir.path().join("manual-wt");
    init_repo(&repo);
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "manual/check",
            manual_wt.to_str().unwrap(),
            "main",
        ],
    );

    let mut client = TestClient::new(Server::new(home.clone()));
    let _: Value = client
        .call_ok(
            "providers/save",
            json!({
                "name": "fixture",
                "wire": "openai-completions",
                "base_url": "http://127.0.0.1:9",
                "api_key": "sk-test"
            }),
        )
        .await;
    let _: Value = client
        .call_ok(
            "providers/save_model",
            json!({
                "provider": "fixture",
                "previous": null,
                "model": {
                    "id": "m1",
                    "context_window": 128000,
                    "max_output": 4096,
                    "levels": ["off"],
                    "mapping": {},
                    "images": false
                }
            }),
        )
        .await;
    let _: Value = client
        .call_ok(
            "config/update",
            json!({ "key": "model", "value": "fixture/m1" }),
        )
        .await;

    let branches: Branches = client
        .call_ok("git/branches", json!({ "root": repo }))
        .await;
    assert!(branches.local.contains(&"main".to_owned()));
    assert!(branches.local.contains(&"release/v1".to_owned()));

    let created: WorktreeEntry = client
        .call_ok(
            "worktree/create",
            json!({ "root": repo, "base_branch": "release/v1" }),
        )
        .await;
    assert_eq!(created.branch, format!("tiechui/{}", created.name));
    assert_eq!(created.base_branch, "release/v1");
    assert!(!created.dirty);

    let listed_wt: Vec<WorktreeEntry> = client
        .call_ok("worktree/list", json!({ "root": repo }))
        .await;
    assert_eq!(listed_wt.len(), 1);
    assert_eq!(listed_wt[0].path, created.path);

    let root_snap: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": repo, "model": "fixture/m1" }),
        )
        .await;
    assert_eq!(root_snap.worktree, None);

    let wt_snap: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": created.path, "model": "fixture/m1" }),
        )
        .await;
    let expected_wt = SessionWorktree {
        path: created.path.clone(),
        name: created.name.clone(),
        branch: created.branch.clone(),
        base_branch: "release/v1".into(),
    };
    assert_eq!(wt_snap.cwd, created.path);
    assert_eq!(wt_snap.worktree, Some(expected_wt.clone()));

    let manual_snap: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": manual_wt, "model": "fixture/m1" }),
        )
        .await;
    assert_eq!(manual_snap.worktree, None);

    client.close().await;

    append_turn(Path::new(&root_snap.path), "hello from root");
    append_turn(Path::new(&wt_snap.path), "hello from managed worktree");
    append_turn(Path::new(&manual_snap.path), "hello from manual worktree");

    let mut restarted = TestClient::new(Server::new(home.clone()));
    let sessions: Vec<SessionSummary> = restarted
        .call_ok("session/list", json!({ "cwd": repo }))
        .await;
    assert_eq!(sessions.len(), 2, "sessions: {sessions:?}");

    let root_summary = sessions
        .iter()
        .find(|s| s.thread_id == root_snap.thread_id)
        .expect("missing project root session");
    assert_eq!(root_summary.worktree, None);

    let wt_summary = sessions
        .iter()
        .find(|s| s.thread_id == wt_snap.thread_id)
        .expect("missing managed worktree session");
    assert_eq!(wt_summary.cwd, created.path);
    assert_eq!(wt_summary.worktree, Some(expected_wt));

    let doctor_home = home.clone();
    tokio::task::spawn_blocking(move || {
        let matches = clap::Command::new("doctor")
            .arg(clap::Arg::new("profile").long("profile"))
            .arg(clap::Arg::new("prefix").long("prefix"))
            .get_matches_from(["doctor", "--profile", doctor_home.to_str().unwrap()]);
        crate::doctor::run(&matches).unwrap();
    })
    .await
    .unwrap();

    std::fs::write(Path::new(&created.path).join("dirty.txt"), "dirty\n").unwrap();
    let err = restarted
        .call("worktree/remove", json!({ "path": created.path }))
        .await
        .unwrap_err();
    assert!(err["message"].as_str().is_some_and(|m| !m.is_empty()));

    let removed: WorktreeRemoveResult = restarted
        .call_ok(
            "worktree/remove",
            json!({ "path": created.path, "force": true }),
        )
        .await;
    assert_eq!(removed.branch, created.branch);
    assert!(!Path::new(&created.path).exists());

    restarted.close().await;
}
