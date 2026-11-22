#![allow(clippy::unwrap_used)]

use std::collections::VecDeque;
use std::path::Path;
use std::process::Command;
use std::sync::Mutex;

use serde_json::{Value, json};
use tiechui_model::{Content, Message, ModelId, Role};
use tiechui_module_rewind::store::{SnapshotRecord, Store as RewindStore};
use tiechui_protocol::app::{
    ConfigValues, RewindCheck, RewindMode, SessionRewindResult, SessionSummary, SessionWorktree,
    Snapshot, TrashResult, TrustResult, TurnChanges,
};
use tiechui_protocol::workspace::{
    Branches, PostCreateStatus, WorktreeCleanupStatus, WorktreeEntry, WorktreeRemoveResult,
    WorktreeSkipReason,
};
use tiechui_protocol::{EventKind, Item, KnownItem, ThreadId};
use tiechui_session::{EntryKind, ModelRef, SessionStore};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::Server;
use super::tests::{TestClient, isolated_home_child, sse_chat};

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

pub(super) fn init_repo(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "core.autocrlf", "false"]);
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

pub(super) fn append_turn(path: &Path, prompt: &str) {
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

    let mut client = new_configured_client(&home).await;

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

    assert_doctor_ok(&home).await;

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

#[tokio::test]
async fn worktree_create_handles_post_create_trust_failure_and_carry_changes_without_blocking_sessions()
 {
    let Some(home) = isolated_home_child(
        "TC_WT_POST_CREATE_HOME",
        "app_server::worktree_tests::worktree_create_handles_post_create_trust_failure_and_carry_changes_without_blocking_sessions",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let repo = workspace_dir.path().join("scripted-repo");
    init_repo(&repo);
    std::fs::create_dir_all(repo.join(".agents")).unwrap();
    std::fs::write(
        repo.join(".agents/config.toml"),
        "[worktrees]\npost_create_script = 'echo rpc-setup-ok && touch .setup-done'\n",
    )
    .unwrap();
    git(&repo, &["add", ".agents/config.toml"]);
    git(&repo, &["commit", "-m", "configure post-create"]);

    let mut client = new_configured_client(&home).await;

    let untrusted: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    assert_eq!(
        untrusted.post_create.status,
        PostCreateStatus::SkippedUntrusted
    );
    assert!(!Path::new(&untrusted.path).join(".setup-done").exists());

    let trust_res: TrustResult = client
        .call_ok("trust", json!({ "cwd": repo, "allow": true }))
        .await;
    assert_eq!(trust_res.allowed, Some(true));

    std::fs::write(repo.join("README.md"), "edited in main\n").unwrap();
    std::fs::write(repo.join("carried.txt"), "new untracked\n").unwrap();
    let main_status_before = git(&repo, &["status", "--porcelain"]);
    let main_cached_before = git(&repo, &["diff", "--cached"]);

    let trusted: WorktreeEntry = client
        .call_ok(
            "worktree/create",
            json!({ "root": repo, "carry_changes": true }),
        )
        .await;
    assert_eq!(trusted.post_create.status, PostCreateStatus::Succeeded);
    assert_eq!(trusted.post_create.exit_code, Some(0));
    assert!(trusted.post_create.output.contains("rpc-setup-ok"));
    assert!(Path::new(&trusted.path).join(".setup-done").exists());
    assert_eq!(
        std::fs::read_to_string(Path::new(&trusted.path).join("README.md")).unwrap(),
        "edited in main\n"
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(&trusted.path).join("carried.txt")).unwrap(),
        "new untracked\n"
    );
    assert_eq!(git(&repo, &["status", "--porcelain"]), main_status_before);
    assert_eq!(git(&repo, &["diff", "--cached"]), main_cached_before);

    std::fs::write(
        repo.join(".agents/config.toml"),
        "[worktrees]\npost_create_script = 'echo broken-setup >&2 && exit 1'\n",
    )
    .unwrap();
    let failed: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    assert_eq!(failed.post_create.status, PostCreateStatus::Failed);
    assert_eq!(failed.post_create.exit_code, Some(1));
    assert!(failed.post_create.output.contains("broken-setup"));

    let snap: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": failed.path, "model": "fixture/m1" }),
        )
        .await;
    assert_eq!(snap.cwd, failed.path);
    assert!(snap.worktree.is_some());

    client.close().await;
}

struct SequenceSse(Mutex<VecDeque<String>>);

impl wiremock::Respond for SequenceSse {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        let body = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra model request");
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }
}

pub(super) fn sse_tool_calls(calls: &[(&str, &str, Value)]) -> String {
    let wire_calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(idx, (id, name, args))| {
            json!({
                "index": idx,
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": args.to_string(),
                }
            })
        })
        .collect();
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices": [{
                "index": 0,
                "delta": { "tool_calls": wire_calls },
                "finish_reason": "tool_calls"
            }]
        })
    )
}

pub(super) async fn setup_mock_server_and_client(
    home: &Path,
    replies: Vec<String>,
) -> (MockServer, TestClient) {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(SequenceSse(Mutex::new(replies.into())))
        .mount(&mock)
        .await;
    std::fs::write(
        home.join("providers.toml"),
        format!(
            "[fixture]\nwire = \"openai-completions\"\nbase_url = \"{}\"\napi_key = \"sk-test\"\n\
             models = [{{ id = \"m1\", context_window = 128000, max_output = 4096 }}]\n",
            mock.uri()
        ),
    )
    .unwrap();
    std::fs::write(home.join("config.toml"), "model = \"fixture/m1\"\n").unwrap();
    let client = TestClient::new(Server::new(home.to_path_buf()));
    (mock, client)
}

pub(super) async fn run_user_turn(
    client: &mut TestClient,
    thread_id: &ThreadId,
    prompt: &str,
) -> String {
    let _: Value = client
        .call_ok(
            "session/submit",
            json!({
                "thread_id": thread_id,
                "op": {
                    "kind": "user_input",
                    "delivery": "follow_up",
                    "content": [{ "kind": "text", "text": prompt }]
                }
            }),
        )
        .await;
    let events = client.wait_turn_completed(thread_id).await;
    let EventKind::TurnCompleted { turn } = events.last().unwrap() else {
        panic!("expected TurnCompleted");
    };
    turn.items
        .iter()
        .find_map(|item| match item {
            Item::Known(KnownItem::UserMessage { id, .. }) => Some(id.0.clone()),
            _ => None,
        })
        .expect("missing user message item id")
}

fn snapshot_user_item_ids(snap: &Snapshot) -> Vec<String> {
    snap.events
        .iter()
        .filter_map(|ev| match &ev.event {
            EventKind::ThreadStarted { thread, .. } => Some(thread.turns.as_slice()),
            _ => None,
        })
        .flatten()
        .flat_map(|turn| turn.items.iter())
        .filter_map(|item| match item {
            Item::Known(KnownItem::UserMessage { id, .. }) => Some(id.0.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn worktree_session_snapshot_rewind_restores_bash_and_manual_edits_while_hiding_checkpoint_refs()
 {
    let Some(home) = isolated_home_child(
        "TC_WT_SNAP_REWIND_HOME",
        "app_server::worktree_tests::worktree_session_snapshot_rewind_restores_bash_and_manual_edits_while_hiding_checkpoint_refs",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let repo = workspace_dir.path().join("snap-repo");
    init_repo(&repo);
    std::fs::write(repo.join("a.txt"), "initial-a\n").unwrap();
    std::fs::write(repo.join("keep.txt"), "v0\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "node_modules/\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "add fixture files"]);

    let (_mock, mut client) = setup_mock_server_and_client(
        &home,
        vec![
            sse_tool_calls(&[(
                "call-t1",
                "write",
                json!({ "path": "keep.txt", "content": "turn1-keep\n" }),
            )]),
            sse_chat("turn 1 done"),
            sse_tool_calls(&[(
                "call-t2",
                "bash",
                json!({ "command": "rm a.txt && printf 'x\\n' > b.txt" }),
            )]),
            sse_chat("turn 2 done"),
        ],
    )
    .await;

    let wt: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let wt_path = Path::new(&wt.path);
    std::fs::create_dir_all(wt_path.join("node_modules")).unwrap();
    std::fs::write(wt_path.join("node_modules/cache.txt"), "keep-ignored\n").unwrap();

    let branches_before = git(wt_path, &["branch", "-a"]);
    let log_before = git(wt_path, &["log", "--all", "--oneline"]);

    let (opened, turn1_item, turn2_item) = open_yolo_two_turns(&mut client, wt_path).await;

    assert!(!wt_path.join("a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(wt_path.join("b.txt")).unwrap(),
        "x\n"
    );
    std::fs::write(wt_path.join("keep.txt"), "manual-edit\n").unwrap();
    std::fs::write(wt_path.join("manual.txt"), "hand-added\n").unwrap();

    let check: RewindCheck = client
        .call_ok(
            "turn/rewind_check",
            json!({ "thread_id": opened.thread_id, "item_id": turn2_item }),
        )
        .await;
    assert_eq!(check.mode, RewindMode::Snapshot);
    assert_eq!(check.reason, None);
    assert!(
        check
            .snapshot_ref
            .as_deref()
            .is_some_and(|r| r.starts_with("refs/tiechui/checkpoints/"))
    );
    assert_eq!(
        check.commands,
        vec!["rm a.txt && printf 'x\\n' > b.txt".to_owned()]
    );

    let rewound: SessionRewindResult = client
        .call_ok(
            "session/rewind",
            json!({
                "thread_id": opened.thread_id,
                "item_id": turn2_item,
                "mode": "snapshot"
            }),
        )
        .await;
    assert_eq!(rewound.mode, RewindMode::Snapshot);
    assert!(rewound.submission_id.is_some());
    assert!(rewound.conflicts.is_empty());

    assert_eq!(
        std::fs::read_to_string(wt_path.join("a.txt")).unwrap(),
        "initial-a\n"
    );
    assert!(!wt_path.join("b.txt").exists());
    assert!(!wt_path.join("manual.txt").exists());
    assert_eq!(
        std::fs::read_to_string(wt_path.join("keep.txt")).unwrap(),
        "turn1-keep\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt_path.join("node_modules/cache.txt")).unwrap(),
        "keep-ignored\n"
    );

    let after_snap: Snapshot = client
        .call_ok("session/snapshot", json!({ "thread_id": opened.thread_id }))
        .await;
    let remaining_items = snapshot_user_item_ids(&after_snap);
    assert_eq!(remaining_items.len(), 1, "{remaining_items:?}");
    assert!(remaining_items[0].starts_with(&turn1_item));

    let undone_turn1: TurnChanges = client
        .call_ok(
            "turn/undo",
            json!({ "thread_id": opened.thread_id, "item_id": remaining_items[0] }),
        )
        .await;
    assert!(undone_turn1.reverted);
    assert_eq!(
        std::fs::read_to_string(wt_path.join("keep.txt")).unwrap(),
        "v0\n"
    );

    let status_out = git(wt_path, &["status"]);
    assert!(!status_out.contains("checkpoints"), "{status_out}");
    assert_eq!(git(wt_path, &["diff", "--cached"]), "");
    assert_eq!(git(wt_path, &["branch", "-a"]), branches_before);
    assert_eq!(git(wt_path, &["log", "--all", "--oneline"]), log_before);

    client.close().await;
}

#[tokio::test]
async fn worktree_rewind_enforces_directory_isolation_across_equal_ancestor_descendant_and_archived()
 {
    let Some(home) = isolated_home_child(
        "TC_WT_ISOLATION_HOME",
        "app_server::worktree_tests::worktree_rewind_enforces_directory_isolation_across_equal_ancestor_descendant_and_archived",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_a_txt(workspace_dir.path(), "iso-repo");

    let (_mock, mut client) = setup_mock_server_and_client(
        &home,
        vec![
            sse_chat("turn 1 ok"),
            sse_tool_calls(&[(
                "call-iso",
                "bash",
                json!({ "command": "rm a.txt && printf 'x\\n' > b.txt" }),
            )]),
            sse_chat("turn 2 ok"),
        ],
    )
    .await;

    let wt: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let wt_path = Path::new(&wt.path);
    let subdir = wt_path.join("nested/sub");
    std::fs::create_dir_all(&subdir).unwrap();

    let (opened, turn1_item, turn2_item) = open_yolo_two_turns(&mut client, wt_path).await;
    assert!(!wt_path.join("a.txt").exists());
    assert!(wt_path.join("b.txt").exists());

    let peer_equal: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": wt.path, "model": "fixture/m1" }),
        )
        .await;
    let _: Value = client
        .call_ok(
            "session/rename",
            json!({ "path": peer_equal.path, "title": "Shared Peer" }),
        )
        .await;

    let check_eq: RewindCheck = client
        .call_ok(
            "turn/rewind_check",
            json!({ "thread_id": opened.thread_id, "item_id": turn2_item }),
        )
        .await;
    assert_eq!(check_eq.mode, RewindMode::ChatOnly);
    assert!(
        check_eq
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("Shared Peer"))
    );
    assert_eq!(
        check_eq
            .conflicting_session
            .as_ref()
            .and_then(|c| c.title.as_deref()),
        Some("Shared Peer")
    );

    let err = client
        .call(
            "session/rewind",
            json!({
                "thread_id": opened.thread_id,
                "item_id": turn2_item,
                "mode": "snapshot"
            }),
        )
        .await
        .unwrap_err();
    assert!(err["message"].as_str().is_some_and(|m| !m.is_empty()));
    assert!(!wt_path.join("a.txt").exists());
    assert!(wt_path.join("b.txt").exists());
    let unchanged_snap: Snapshot = client
        .call_ok("session/snapshot", json!({ "thread_id": opened.thread_id }))
        .await;
    assert_eq!(snapshot_user_item_ids(&unchanged_snap).len(), 2);

    let _: Value = client
        .call_ok(
            "session/close",
            json!({ "thread_id": peer_equal.thread_id }),
        )
        .await;
    let _: Value = client
        .call_ok(
            "session/archive",
            json!({ "path": peer_equal.path, "archived": true }),
        )
        .await;
    let check_archived: RewindCheck = client
        .call_ok(
            "turn/rewind_check",
            json!({ "thread_id": opened.thread_id, "item_id": turn2_item }),
        )
        .await;
    assert_eq!(check_archived.mode, RewindMode::ChatOnly);
    assert_eq!(
        check_archived
            .conflicting_session
            .as_ref()
            .map(|c| c.archived),
        Some(true)
    );

    let _: Value = client
        .call_ok("session/trash", json!({ "path": peer_equal.path }))
        .await;
    let check_unblocked: RewindCheck = client
        .call_ok(
            "turn/rewind_check",
            json!({ "thread_id": opened.thread_id, "item_id": turn2_item }),
        )
        .await;
    assert_eq!(check_unblocked.mode, RewindMode::Snapshot);

    let main_repo_peer: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": repo, "model": "fixture/m1" }),
        )
        .await;
    let check_with_main_repo: RewindCheck = client
        .call_ok(
            "turn/rewind_check",
            json!({ "thread_id": opened.thread_id, "item_id": turn2_item }),
        )
        .await;
    assert_eq!(
        check_with_main_repo.mode,
        RewindMode::Snapshot,
        "main-repo session must not block in-project worktree snapshot rewind"
    );
    let _: Value = client
        .call_ok("session/trash", json!({ "path": main_repo_peer.path }))
        .await;

    let wt_container = wt_path.parent().unwrap().to_path_buf();
    for overlapping_cwd in [wt_container.as_path(), subdir.as_path()] {
        let peer: Snapshot = client
            .call_ok(
                "session/open",
                json!({ "cwd": overlapping_cwd, "model": "fixture/m1" }),
            )
            .await;
        let blocked: RewindCheck = client
            .call_ok(
                "turn/rewind_check",
                json!({ "thread_id": opened.thread_id, "item_id": turn2_item }),
            )
            .await;
        assert_eq!(blocked.mode, RewindMode::ChatOnly, "{overlapping_cwd:?}");
        assert!(
            client
                .call(
                    "session/rewind",
                    json!({
                        "thread_id": opened.thread_id,
                        "item_id": turn2_item,
                        "mode": "snapshot"
                    }),
                )
                .await
                .is_err()
        );
        let _: Value = client
            .call_ok("session/trash", json!({ "path": peer.path }))
            .await;
        let restored_mode: RewindCheck = client
            .call_ok(
                "turn/rewind_check",
                json!({ "thread_id": opened.thread_id, "item_id": turn2_item }),
            )
            .await;
        assert_eq!(restored_mode.mode, RewindMode::Snapshot);
    }

    let final_peer: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": wt.path, "model": "fixture/m1" }),
        )
        .await;
    let chat_rewound: SessionRewindResult = client
        .call_ok(
            "session/rewind",
            json!({
                "thread_id": opened.thread_id,
                "item_id": turn2_item,
                "mode": "chat_only"
            }),
        )
        .await;
    assert_eq!(chat_rewound.mode, RewindMode::ChatOnly);
    assert!(chat_rewound.submission_id.is_some());
    assert!(!wt_path.join("a.txt").exists());
    assert!(wt_path.join("b.txt").exists());
    let after_chat: Snapshot = client
        .call_ok("session/snapshot", json!({ "thread_id": opened.thread_id }))
        .await;
    let remaining = snapshot_user_item_ids(&after_chat);
    assert_eq!(remaining.len(), 1);
    assert!(remaining[0].starts_with(&turn1_item));

    let _: Value = client
        .call_ok("session/trash", json!({ "path": final_peer.path }))
        .await;
    let check_turn1: RewindCheck = client
        .call_ok(
            "turn/rewind_check",
            json!({ "thread_id": opened.thread_id, "item_id": remaining[0] }),
        )
        .await;
    assert_eq!(check_turn1.mode, RewindMode::Snapshot);

    client.close().await;
}

#[tokio::test]
async fn worktree_missing_snapshot_falls_back_to_patch_rewind_with_reason() {
    let Some(home) = isolated_home_child(
        "TC_WT_MISSING_SNAP_HOME",
        "app_server::worktree_tests::worktree_missing_snapshot_falls_back_to_patch_rewind_with_reason",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let repo = workspace_dir.path().join("fallback-repo");
    init_repo(&repo);
    std::fs::write(repo.join("tracked.txt"), "v0\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "add tracked.txt"]);

    let (_mock, mut client) = setup_mock_server_and_client(
        &home,
        vec![
            sse_chat("turn 1 ok"),
            sse_tool_calls(&[
                (
                    "call-w",
                    "write",
                    json!({ "path": "tracked.txt", "content": "v1\n" }),
                ),
                (
                    "call-b",
                    "bash",
                    json!({ "command": "printf 'bash\\n' > bash_side.txt" }),
                ),
            ]),
            sse_chat("turn 2 ok"),
        ],
    )
    .await;

    let wt: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let wt_path = Path::new(&wt.path);
    let (opened, _turn1_item, turn2_item) = open_yolo_two_turns(&mut client, wt_path).await;

    let cancel = CancellationToken::new();
    let rewind_store =
        RewindStore::new(&home.join("state/rewind"), wt_path, &opened.thread_id.0).unwrap();
    let mut manifest: Value = serde_json::from_slice(
        &std::fs::read(rewind_store.directory.join("manifest.json")).unwrap(),
    )
    .unwrap();
    manifest["snapshots"][&turn2_item] = serde_json::to_value(SnapshotRecord::Missing {
        reason: "git snapshot timed out after 80ms".into(),
    })
    .unwrap();
    rewind_store.atomic("manifest.json", &manifest).unwrap();
    drop(cancel);

    let check: RewindCheck = client
        .call_ok(
            "turn/rewind_check",
            json!({ "thread_id": opened.thread_id, "item_id": turn2_item }),
        )
        .await;
    assert_eq!(check.mode, RewindMode::Patch);
    assert!(
        check
            .reason
            .as_deref()
            .is_some_and(|r| r.contains("timed out")),
        "{check:?}"
    );
    assert_eq!(check.safe, vec!["tracked.txt".to_owned()]);
    assert_eq!(
        check.commands,
        vec!["printf 'bash\\n' > bash_side.txt".to_owned()]
    );

    assert!(
        client
            .call(
                "session/rewind",
                json!({
                    "thread_id": opened.thread_id,
                    "item_id": turn2_item,
                    "mode": "snapshot"
                }),
            )
            .await
            .is_err()
    );

    let rewound: SessionRewindResult = client
        .call_ok(
            "session/rewind",
            json!({
                "thread_id": opened.thread_id,
                "item_id": turn2_item,
                "mode": "patch"
            }),
        )
        .await;
    assert_eq!(rewound.mode, RewindMode::Patch);
    assert!(rewound.submission_id.is_some());
    assert!(rewound.conflicts.is_empty());
    assert_eq!(
        std::fs::read_to_string(wt_path.join("tracked.txt")).unwrap(),
        "v0\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt_path.join("bash_side.txt")).unwrap(),
        "bash\n"
    );

    client.close().await;
}

#[tokio::test]
async fn project_root_session_uses_patch_rewind_with_commands_and_turn_undo_stays_patch_everywhere()
{
    let Some(home) = isolated_home_child(
        "TC_WT_PROJECT_PATCH_HOME",
        "app_server::worktree_tests::project_root_session_uses_patch_rewind_with_commands_and_turn_undo_stays_patch_everywhere",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let repo = workspace_dir.path().join("project-repo");
    init_repo(&repo);
    std::fs::write(repo.join("p2.txt"), "orig-p2\n").unwrap();
    std::fs::write(repo.join("wt_tracked.txt"), "orig-wt\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "add tracked files"]);

    let (_mock, mut client) = setup_mock_server_and_client(
        &home,
        vec![
            sse_chat("root turn 1"),
            sse_tool_calls(&[
                (
                    "r-w2",
                    "write",
                    json!({ "path": "p2.txt", "content": "turn2-p2\n" }),
                ),
                (
                    "r-b2",
                    "bash",
                    json!({ "command": "printf 'c2\\n' > cmd2.txt" }),
                ),
            ]),
            sse_chat("root turn 2"),
            sse_tool_calls(&[
                (
                    "r-w3",
                    "write",
                    json!({ "path": "p3.txt", "content": "turn3-p3\n" }),
                ),
                (
                    "r-b3",
                    "bash",
                    json!({ "command": "printf 'c3\\n' > cmd3.txt" }),
                ),
            ]),
            sse_chat("root turn 3"),
            sse_tool_calls(&[
                (
                    "wt-w1",
                    "write",
                    json!({ "path": "wt_tracked.txt", "content": "wt-edited\n" }),
                ),
                (
                    "wt-b1",
                    "bash",
                    json!({ "command": "printf 'wt\\n' > wt_bash.txt" }),
                ),
            ]),
            sse_chat("wt turn 1"),
            sse_tool_calls(&[(
                "m-b1",
                "bash",
                json!({ "command": "printf 'm\\n' > manual_cmd.txt" }),
            )]),
            sse_chat("manual turn 1"),
            sse_tool_calls(&[(
                "ng-b1",
                "bash",
                json!({ "command": "printf 'ng\\n' > plain_cmd.txt" }),
            )]),
            sse_chat("plain turn 1"),
        ],
    )
    .await;

    let root_sess: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": repo, "model": "fixture/m1", "mode": "yolo" }),
        )
        .await;
    let _r_t1 = run_user_turn(&mut client, &root_sess.thread_id, "r1").await;
    let r_t2 = run_user_turn(&mut client, &root_sess.thread_id, "r2").await;
    let r_t3 = run_user_turn(&mut client, &root_sess.thread_id, "r3").await;

    let check: RewindCheck = client
        .call_ok(
            "turn/rewind_check",
            json!({ "thread_id": root_sess.thread_id, "item_id": r_t2 }),
        )
        .await;
    assert_eq!(check.mode, RewindMode::Patch);
    assert_eq!(check.reason, None);
    assert_eq!(check.snapshot_ref, None);
    assert_eq!(
        check.commands,
        vec![
            "printf 'c2\\n' > cmd2.txt".to_owned(),
            "printf 'c3\\n' > cmd3.txt".to_owned()
        ]
    );
    assert_eq!(check.safe, vec!["p2.txt".to_owned(), "p3.txt".to_owned()]);

    let root_undo: TurnChanges = client
        .call_ok(
            "turn/undo",
            json!({ "thread_id": root_sess.thread_id, "item_id": r_t3 }),
        )
        .await;
    assert!(root_undo.reverted);
    assert!(!repo.join("p3.txt").exists());
    assert!(repo.join("cmd3.txt").exists());
    let _: TurnChanges = client
        .call_ok(
            "turn/undo",
            json!({ "thread_id": root_sess.thread_id, "item_id": r_t3 }),
        )
        .await;
    assert!(repo.join("p3.txt").exists());

    let rewound_root: SessionRewindResult = client
        .call_ok(
            "session/rewind",
            json!({
                "thread_id": root_sess.thread_id,
                "item_id": r_t2,
                "mode": "patch"
            }),
        )
        .await;
    assert_eq!(rewound_root.mode, RewindMode::Patch);
    assert!(rewound_root.submission_id.is_some());
    assert_eq!(
        std::fs::read_to_string(repo.join("p2.txt")).unwrap(),
        "orig-p2\n"
    );
    assert!(!repo.join("p3.txt").exists());
    assert!(repo.join("cmd2.txt").exists());

    let wt: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let wt_path = Path::new(&wt.path);
    let wt_sess: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": wt.path, "model": "fixture/m1", "mode": "yolo" }),
        )
        .await;
    let wt_t1 = run_user_turn(&mut client, &wt_sess.thread_id, "wt1").await;
    let wt_undo: TurnChanges = client
        .call_ok(
            "turn/undo",
            json!({ "thread_id": wt_sess.thread_id, "item_id": wt_t1 }),
        )
        .await;
    assert!(wt_undo.reverted);
    assert_eq!(
        std::fs::read_to_string(wt_path.join("wt_tracked.txt")).unwrap(),
        "orig-wt\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt_path.join("wt_bash.txt")).unwrap(),
        "wt\n"
    );

    let manual_wt = workspace_dir.path().join("manual-wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "manual/patch",
            manual_wt.to_str().unwrap(),
            "main",
        ],
    );
    let plain_dir = workspace_dir.path().join("plain-dir");
    std::fs::create_dir_all(&plain_dir).unwrap();
    for (dir, expected_cmd) in [
        (manual_wt.as_path(), "printf 'm\\n' > manual_cmd.txt"),
        (plain_dir.as_path(), "printf 'ng\\n' > plain_cmd.txt"),
    ] {
        let sess: Snapshot = client
            .call_ok(
                "session/open",
                json!({ "cwd": dir, "model": "fixture/m1", "mode": "yolo" }),
            )
            .await;
        let item = run_user_turn(&mut client, &sess.thread_id, "run").await;
        let checked: RewindCheck = client
            .call_ok(
                "turn/rewind_check",
                json!({ "thread_id": sess.thread_id, "item_id": item }),
            )
            .await;
        assert_eq!(checked.mode, RewindMode::Patch);
        assert_eq!(checked.reason, None);
        assert_eq!(checked.commands, vec![expected_cmd.to_owned()]);
    }

    client.close().await;
}

#[tokio::test]
async fn session_rewind_validates_and_branches_conversation_before_touching_any_files() {
    let Some(home) = isolated_home_child(
        "TC_WT_CONV_FIRST_HOME",
        "app_server::worktree_tests::session_rewind_validates_and_branches_conversation_before_touching_any_files",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let repo = init_repo_with_a_txt(workspace_dir.path(), "order-repo");

    let (_mock, mut client) = setup_mock_server_and_client(
        &home,
        vec![
            sse_chat("turn 1 ok"),
            sse_tool_calls(&[
                (
                    "c-w",
                    "write",
                    json!({ "path": "tracked.txt", "content": "written\n" }),
                ),
                (
                    "c-b",
                    "bash",
                    json!({ "command": "rm a.txt && printf 'x\\n' > b.txt" }),
                ),
            ]),
            sse_chat("turn 2 ok"),
        ],
    )
    .await;

    let wt: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let wt_path = Path::new(&wt.path);
    let server = Server::new(home.clone());
    drop(client);
    let mut client = TestClient::new(server.clone());

    let (opened, _turn1_item, turn2_item) = open_yolo_two_turns(&mut client, wt_path).await;

    let assert_untouched = || {
        assert!(!wt_path.join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(wt_path.join("b.txt")).unwrap(),
            "x\n"
        );
        assert_eq!(
            std::fs::read_to_string(wt_path.join("tracked.txt")).unwrap(),
            "written\n"
        );
    };

    for bad_item in ["nonexistent-item", "call-c-b"] {
        for mode in ["snapshot", "patch"] {
            assert!(
                client
                    .call(
                        "session/rewind",
                        json!({
                            "thread_id": opened.thread_id,
                            "item_id": bad_item,
                            "mode": mode
                        }),
                    )
                    .await
                    .is_err()
            );
            assert_untouched();
        }
    }

    {
        let sessions = super::lock(&server.sessions);
        let live = sessions.get(&opened.thread_id).unwrap();
        live.shutdown.cancel();
    }
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    assert!(
        client
            .call(
                "session/rewind",
                json!({
                    "thread_id": opened.thread_id,
                    "item_id": turn2_item,
                    "mode": "snapshot"
                }),
            )
            .await
            .is_err()
    );
    assert_untouched();

    client.close().await;
}

fn init_repo_with_a_txt(parent: &Path, name: &str) -> std::path::PathBuf {
    let repo = parent.join(name);
    init_repo(&repo);
    std::fs::write(repo.join("a.txt"), "initial-a\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "add a.txt"]);
    repo
}

async fn open_yolo_two_turns(client: &mut TestClient, cwd: &Path) -> (Snapshot, String, String) {
    let opened: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": cwd, "model": "fixture/m1", "mode": "yolo" }),
        )
        .await;
    let turn1 = run_user_turn(client, &opened.thread_id, "turn one").await;
    let turn2 = run_user_turn(client, &opened.thread_id, "turn two").await;
    (opened, turn1, turn2)
}

pub(super) async fn assert_doctor_ok(home: &Path) {
    let doctor_home = home.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let matches = clap::Command::new("doctor")
            .arg(clap::Arg::new("profile").long("profile"))
            .arg(clap::Arg::new("prefix").long("prefix"))
            .get_matches_from(["doctor", "--profile", doctor_home.to_str().unwrap()]);
        crate::doctor::run(&matches).unwrap();
    })
    .await
    .unwrap();
}

pub(super) async fn new_configured_client(home: &Path) -> TestClient {
    let mut client = TestClient::new(Server::new(home.to_path_buf()));
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
    client
}

async fn wait_until_removed(path: &Path, label: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"));
}

#[tokio::test]
async fn session_archive_and_trash_handle_clean_dirty_and_shared_worktrees_while_preserving_branches()
 {
    let Some(home) = isolated_home_child(
        "TC_WT_TRASH_HOME",
        "app_server::worktree_tests::session_archive_and_trash_handle_clean_dirty_and_shared_worktrees_while_preserving_branches",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let repo = workspace_dir.path().join("trash-repo");
    init_repo(&repo);

    let mut client = new_configured_client(&home).await;

    let wt_arch: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let sess_arch: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": wt_arch.path, "model": "fixture/m1" }),
        )
        .await;
    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": sess_arch.thread_id }))
        .await;
    append_turn(Path::new(&sess_arch.path), "archive me");
    let _: Value = client
        .call_ok(
            "session/archive",
            json!({ "path": sess_arch.path, "archived": true }),
        )
        .await;
    assert!(Path::new(&wt_arch.path).is_dir());
    let listed_after_archive: Vec<WorktreeEntry> = client
        .call_ok("worktree/list", json!({ "root": repo }))
        .await;
    assert!(listed_after_archive.iter().any(|w| w.path == wt_arch.path));

    let sess_shared: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": wt_arch.path, "model": "fixture/m1" }),
        )
        .await;
    let shared_trash: TrashResult = client
        .call_ok("session/trash", json!({ "path": sess_shared.path }))
        .await;
    assert!(shared_trash.trashed);
    assert!(!shared_trash.needs_worktree_confirm);
    assert!(!shared_trash.worktree_removed);
    assert!(Path::new(&wt_arch.path).is_dir());

    let sole_clean_trash: TrashResult = client
        .call_ok(
            "session/trash",
            json!({ "path": sess_arch.path, "worktree_policy": "ask_if_dirty" }),
        )
        .await;
    assert!(sole_clean_trash.trashed);
    assert!(!sole_clean_trash.needs_worktree_confirm);
    assert!(sole_clean_trash.worktree_removed);
    assert!(!Path::new(&wt_arch.path).exists());
    assert!(git(&repo, &["branch", "--list", "tiechui/*"]).contains(&wt_arch.branch));

    let wt_dirty_keep: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let sess_dirty_keep: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": wt_dirty_keep.path, "model": "fixture/m1" }),
        )
        .await;
    std::fs::write(
        Path::new(&wt_dirty_keep.path).join("uncommitted.txt"),
        "keep me\n",
    )
    .unwrap();

    let ask_res: TrashResult = client
        .call_ok(
            "session/trash",
            json!({ "path": sess_dirty_keep.path, "worktree_policy": "ask_if_dirty" }),
        )
        .await;
    assert!(!ask_res.trashed);
    assert!(ask_res.needs_worktree_confirm);
    assert!(!ask_res.worktree_removed);
    assert_eq!(ask_res.path, sess_dirty_keep.path);
    assert_eq!(
        ask_res.worktree.as_ref().map(|w| w.path.as_str()),
        Some(wt_dirty_keep.path.as_str())
    );
    assert!(ask_res.worktree.as_ref().is_some_and(|w| w.dirty));
    assert!(Path::new(&sess_dirty_keep.path).is_file());
    assert!(Path::new(&wt_dirty_keep.path).is_dir());
    let _: Snapshot = client
        .call_ok(
            "session/snapshot",
            json!({ "thread_id": sess_dirty_keep.thread_id }),
        )
        .await;

    let keep_res: TrashResult = client
        .call_ok(
            "session/trash",
            json!({ "path": sess_dirty_keep.path, "worktree_policy": "keep" }),
        )
        .await;
    assert!(keep_res.trashed);
    assert!(!keep_res.needs_worktree_confirm);
    assert!(!keep_res.worktree_removed);
    assert!(!Path::new(&sess_dirty_keep.path).exists());
    assert!(Path::new(&keep_res.path).is_file());
    assert!(Path::new(&wt_dirty_keep.path).is_dir());
    let still_registered: Vec<WorktreeEntry> = client
        .call_ok("worktree/list", json!({ "root": repo }))
        .await;
    assert!(
        still_registered
            .iter()
            .any(|w| w.path == wt_dirty_keep.path)
    );

    let wt_dirty_force: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let sess_dirty_force: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": wt_dirty_force.path, "model": "fixture/m1" }),
        )
        .await;
    std::fs::write(Path::new(&wt_dirty_force.path).join("README.md"), "dirty\n").unwrap();

    let force_res: TrashResult = client
        .call_ok(
            "session/trash",
            json!({ "path": sess_dirty_force.path, "worktree_policy": "force_remove" }),
        )
        .await;
    assert!(force_res.trashed);
    assert!(!force_res.needs_worktree_confirm);
    assert!(force_res.worktree_removed);
    assert!(!Path::new(&sess_dirty_force.path).exists());
    assert!(!Path::new(&wt_dirty_force.path).exists());
    let branches = git(&repo, &["branch", "--list", "tiechui/*"]);
    assert!(branches.contains(&wt_dirty_force.branch), "{branches}");

    let wt_auto_clean: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let sess_auto_clean: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": wt_auto_clean.path, "model": "fixture/m1" }),
        )
        .await;
    let auto_clean_res: TrashResult = client
        .call_ok("session/trash", json!({ "path": sess_auto_clean.path }))
        .await;
    assert!(auto_clean_res.trashed);
    assert!(auto_clean_res.worktree_removed);
    assert!(!Path::new(&wt_auto_clean.path).exists());
    assert!(git(&repo, &["branch", "--list", "tiechui/*"]).contains(&wt_auto_clean.branch));

    let wt_auto_dirty: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let sess_auto_dirty: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": wt_auto_dirty.path, "model": "fixture/m1" }),
        )
        .await;
    std::fs::write(Path::new(&wt_auto_dirty.path).join("auto.txt"), "dirty\n").unwrap();
    let auto_dirty_res: TrashResult = client
        .call_ok("session/trash", json!({ "path": sess_auto_dirty.path }))
        .await;
    assert!(auto_dirty_res.trashed);
    assert!(!auto_dirty_res.needs_worktree_confirm);
    assert!(!auto_dirty_res.worktree_removed);
    assert!(Path::new(&wt_auto_dirty.path).is_dir());

    client.close().await;
}

#[tokio::test]
async fn worktree_cleanup_policies_sweep_on_demand_on_config_update_and_at_startup() {
    let Some(home) = isolated_home_child(
        "TC_WT_CLEANUP_HOME",
        "app_server::worktree_tests::worktree_cleanup_policies_sweep_on_demand_on_config_update_and_at_startup",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let repo = workspace_dir.path().join("auto-clean-repo");
    init_repo(&repo);

    let mut client = new_configured_client(&home).await;

    let initial_cfg: ConfigValues = client.call_ok("config/get", json!({})).await;
    assert_eq!(initial_cfg.auto_clean_idle_days, None);
    assert!(!initial_cfg.auto_clean_on_session_delete);

    for (key, bad_val) in [
        ("worktrees.auto_clean_idle_days", json!(-1)),
        ("worktrees.auto_clean_idle_days", json!(3651)),
        ("worktrees.auto_clean_idle_days", json!("0")),
        ("worktrees.auto_clean_on_session_delete", json!("true")),
    ] {
        assert!(
            client
                .call("config/update", json!({ "key": key, "value": bad_val }))
                .await
                .is_err(),
            "expected error for {key} = {bad_val}"
        );
    }

    let wt_clean_idle: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let default_sweep: WorktreeCleanupStatus = client.call_ok("worktree/cleanup", json!({})).await;
    assert!(default_sweep.removed.is_empty());
    assert!(default_sweep.skipped.is_empty());
    assert!(default_sweep.last_checked_at.is_some());
    assert!(Path::new(&wt_clean_idle.path).is_dir());

    let wt_dirty: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    std::fs::write(Path::new(&wt_dirty.path).join("dirty.txt"), "uncommitted\n").unwrap();

    let wt_live: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let live_snap: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": wt_live.path, "model": "fixture/m1" }),
        )
        .await;
    let _: Value = client
        .call_ok(
            "session/rename",
            json!({ "path": live_snap.path, "title": "Running Feature" }),
        )
        .await;

    let _: Value = client
        .call_ok(
            "config/update",
            json!({ "key": "worktrees.auto_clean_idle_days", "value": 0 }),
        )
        .await;
    let cfg_zero: ConfigValues = client.call_ok("config/get", json!({})).await;
    assert_eq!(cfg_zero.auto_clean_idle_days, Some(0));

    wait_until_removed(Path::new(&wt_clean_idle.path), "config/update wakeup").await;

    let status_after_zero: WorktreeCleanupStatus =
        client.call_ok("worktree/cleanup_status", json!({})).await;
    assert!(!Path::new(&wt_clean_idle.path).exists());
    assert!(git(&repo, &["branch", "--list", "tiechui/*"]).contains(&wt_clean_idle.branch));
    assert!(Path::new(&wt_dirty.path).is_dir());
    assert!(Path::new(&wt_live.path).is_dir());

    let manual_sweep: WorktreeCleanupStatus = client.call_ok("worktree/cleanup", json!({})).await;
    assert_dirty_and_live_skipped(&status_after_zero, &wt_dirty.path, &wt_live.path);
    assert_dirty_and_live_skipped(&manual_sweep, &wt_dirty.path, &wt_live.path);

    let _: Value = client
        .call_ok(
            "config/update",
            json!({ "key": "worktrees.auto_clean_idle_days", "value": null }),
        )
        .await;
    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": live_snap.thread_id }))
        .await;

    let wt_orphan = create_cleaned_orphan_worktree(&mut client, &repo).await;
    let _: Value = client
        .call_ok(
            "config/update",
            json!({ "key": "worktrees.auto_clean_on_session_delete", "value": true }),
        )
        .await;
    let cfg_on_del: ConfigValues = client.call_ok("config/get", json!({})).await;
    assert_eq!(cfg_on_del.auto_clean_idle_days, None);
    assert!(cfg_on_del.auto_clean_on_session_delete);

    let on_del_sweep: WorktreeCleanupStatus =
        client.call_ok("worktree/cleanup_status", json!({})).await;
    assert!(!Path::new(&wt_orphan.path).exists());
    assert!(git(&repo, &["branch", "--list", "tiechui/*"]).contains(&wt_orphan.branch));
    assert!(Path::new(&wt_live.path).is_dir());
    assert!(Path::new(&wt_dirty.path).is_dir());
    assert_has_dirty_skip(&on_del_sweep, &wt_dirty.path);

    let wt_startup_clean: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    assert!(Path::new(&wt_startup_clean.path).is_dir());
    client.close().await;

    verify_startup_sweep_and_doctor(
        &home,
        &repo,
        &wt_startup_clean,
        &wt_live,
        &wt_dirty,
        &[&wt_clean_idle.branch, &wt_orphan.branch],
    )
    .await;
}

fn assert_dirty_and_live_skipped(
    status: &WorktreeCleanupStatus,
    dirty_path: &str,
    live_path: &str,
) {
    let dirty_skip = status
        .skipped
        .iter()
        .find(|s| s.path == dirty_path)
        .expect("missing dirty skipped worktree");
    assert_eq!(dirty_skip.reason_kind, WorktreeSkipReason::Dirty);
    assert!(dirty_skip.reason.contains("存在未提交改动"));

    let live_skip = status
        .skipped
        .iter()
        .find(|s| s.path == live_path)
        .expect("missing active_session skipped worktree");
    assert_eq!(live_skip.reason_kind, WorktreeSkipReason::ActiveSession);
    assert!(live_skip.reason.contains("仍有运行中的会话"));
    assert!(live_skip.reason.contains("Running Feature"));
}

fn assert_has_dirty_skip(status: &WorktreeCleanupStatus, dirty_path: &str) {
    assert!(
        status
            .skipped
            .iter()
            .any(|s| s.path == dirty_path && s.reason_kind == WorktreeSkipReason::Dirty)
    );
}

async fn create_cleaned_orphan_worktree(client: &mut TestClient, repo: &Path) -> WorktreeEntry {
    let wt_orphan: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let orphan_sess: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": wt_orphan.path, "model": "fixture/m1" }),
        )
        .await;
    let temp_dirty = Path::new(&wt_orphan.path).join("temp.txt");
    std::fs::write(&temp_dirty, "temp\n").unwrap();
    let kept_orphan: TrashResult = client
        .call_ok(
            "session/trash",
            json!({ "path": orphan_sess.path, "worktree_policy": "keep" }),
        )
        .await;
    assert!(kept_orphan.trashed);
    assert!(Path::new(&wt_orphan.path).is_dir());
    std::fs::remove_file(&temp_dirty).unwrap();
    wt_orphan
}

async fn verify_startup_sweep_and_doctor(
    home: &Path,
    repo: &Path,
    wt_startup_clean: &WorktreeEntry,
    wt_live: &WorktreeEntry,
    wt_dirty: &WorktreeEntry,
    earlier_branches: &[&str],
) {
    tiechui_config::save_preference(
        &home.join("config.toml"),
        "worktrees.auto_clean_idle_days",
        0.into(),
    )
    .unwrap();
    tiechui_config::save_preference(
        &home.join("config.toml"),
        "worktrees.auto_clean_on_session_delete",
        false.into(),
    )
    .unwrap();

    let restarted_server = Server::new(home.to_path_buf());
    wait_until_removed(Path::new(&wt_startup_clean.path), "startup sweep clean").await;
    wait_until_removed(Path::new(&wt_live.path), "startup sweep idle").await;

    let mut restarted = TestClient::new(restarted_server);
    let startup_status: WorktreeCleanupStatus = restarted
        .call_ok("worktree/cleanup_status", json!({}))
        .await;
    assert!(
        startup_status
            .removed
            .iter()
            .any(|r| r.path == wt_startup_clean.path)
    );
    assert!(Path::new(&wt_dirty.path).is_dir());
    assert_has_dirty_skip(&startup_status, &wt_dirty.path);

    let kept_branches = git(repo, &["branch", "--list", "tiechui/*"]);
    for b in earlier_branches
        .iter()
        .copied()
        .chain([wt_startup_clean.branch.as_str(), wt_live.branch.as_str()])
    {
        assert!(kept_branches.contains(b), "missing {b} in {kept_branches}");
    }

    assert_doctor_ok(home).await;

    restarted.close().await;
}

#[tokio::test]
async fn managed_worktree_inherits_project_root_trust_and_blocks_home_ancestor_leak() {
    let Some(home) = isolated_home_child(
        "TC_WT_TRUST_ROOT_HOME",
        "app_server::worktree_tests::managed_worktree_inherits_project_root_trust_and_blocks_home_ancestor_leak",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let trusted_repo = workspace_dir.path().join("trusted-repo");
    init_repo(&trusted_repo);
    std::fs::create_dir_all(trusted_repo.join(".agents/prompts")).unwrap();
    std::fs::write(
        trusted_repo.join(".agents/prompts/wt-check.md"),
        "---\ndescription: wt prompt\n---\nBody\n",
    )
    .unwrap();
    git(&trusted_repo, &["add", "."]);
    git(&trusted_repo, &["commit", "-m", "add project prompt"]);

    let mut client = new_configured_client(&home).await;
    let _: TrustResult = client
        .call_ok("trust", json!({ "cwd": trusted_repo, "allow": true }))
        .await;
    let wt_trusted: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": trusted_repo }))
        .await;
    let wt_trust_status: TrustResult = client
        .call_ok("trust", json!({ "cwd": wt_trusted.path }))
        .await;
    assert_eq!(wt_trust_status.allowed, Some(true));

    let prompts: tiechui_protocol::app::PromptList = client
        .call_ok("prompts/list", json!({ "cwd": wt_trusted.path }))
        .await;
    assert!(
        prompts.prompts.iter().any(|p| p.name == "wt-check"),
        "expected project prompt in trusted worktree: {prompts:?}"
    );

    let mut prep_warnings = Vec::new();
    let schedules = tiechui_module_loop::Schedules::default();
    let prep = crate::exec::prepare(
        &crate::exec::Options {
            home: Some(home.clone()),
            model: Some("fixture/m1".into()),
            ..Default::default()
        },
        CancellationToken::new(),
        true,
        crate::exec::Open::New(Path::new(&wt_trusted.path).to_path_buf()),
        tiechui_protocol::SessionEpoch(0),
        &schedules,
        &mut prep_warnings,
    )
    .await
    .unwrap();
    assert!(
        !prep_warnings
            .iter()
            .any(|w| w.contains("skipping untrusted project content")),
        "unexpected trust warning: {prep_warnings:?}"
    );
    drop(prep);

    let parent_root = tempfile::tempdir().unwrap();
    let nested_home = parent_root.path().join(".tiechui");
    tiechui_config::private::create_dir_all(&nested_home).unwrap();
    let denied_repo = parent_root.path().join("untrusted-repo");
    init_repo(&denied_repo);

    let store = tiechui_config::TrustStore::new(&nested_home);
    let cancel = CancellationToken::new();
    store.set(parent_root.path(), Some(true), &cancel).unwrap();
    store.set(&denied_repo, Some(false), &cancel).unwrap();

    let mut nested_client = new_configured_client(&nested_home).await;
    let wt_denied: WorktreeEntry = nested_client
        .call_ok("worktree/create", json!({ "root": denied_repo }))
        .await;
    let denied_status: TrustResult = nested_client
        .call_ok("trust", json!({ "cwd": wt_denied.path }))
        .await;
    assert_eq!(
        denied_status.allowed,
        Some(false),
        "managed worktree under trusted parent home must inherit owning project decision"
    );

    nested_client.close().await;
    client.close().await;
}

#[tokio::test]
async fn idle_cleanup_retains_session_attribution_reopens_checkout_and_trash_prunes_registry() {
    let Some(home) = isolated_home_child(
        "TC_WT_IDLE_REOPEN_HOME",
        "app_server::worktree_tests::idle_cleanup_retains_session_attribution_reopens_checkout_and_trash_prunes_registry",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let repo = workspace_dir.path().join("idle-reopen-repo");
    init_repo(&repo);

    let mut client = new_configured_client(&home).await;
    let wt: WorktreeEntry = client
        .call_ok(
            "worktree/create",
            json!({ "root": repo, "base_branch": "release/v1" }),
        )
        .await;
    let wt_path = Path::new(&wt.path);
    std::fs::write(wt_path.join("branch-note.txt"), "saved-on-branch\n").unwrap();
    git(wt_path, &["add", "branch-note.txt"]);
    git(wt_path, &["commit", "-m", "save branch work"]);

    let opened: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": wt.path, "model": "fixture/m1" }),
        )
        .await;
    client.close().await;
    append_turn(Path::new(&opened.path), "turn before idle cleanup");

    let mut after_idle = TestClient::new(Server::new(home.clone()));
    let _: Value = after_idle
        .call_ok(
            "config/update",
            json!({ "key": "worktrees.auto_clean_idle_days", "value": 0 }),
        )
        .await;
    let cleaned: WorktreeCleanupStatus = after_idle.call_ok("worktree/cleanup", json!({})).await;
    assert_eq!(cleaned.removed.len(), 1);
    assert!(!wt_path.exists());

    let listed_wt: Vec<WorktreeEntry> = after_idle
        .call_ok("worktree/list", json!({ "root": repo }))
        .await;
    assert!(listed_wt.is_empty());

    let sessions: Vec<SessionSummary> = after_idle
        .call_ok("session/list", json!({ "cwd": repo }))
        .await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].thread_id, opened.thread_id);
    assert_eq!(
        sessions[0].worktree.as_ref().map(|w| w.name.as_str()),
        Some(wt.name.as_str())
    );

    let reopened: Snapshot = after_idle
        .call_ok("session/open", json!({ "path": opened.path }))
        .await;
    assert!(
        wt_path.is_dir(),
        "expected session/open to restore worktree checkout"
    );
    assert_eq!(
        std::fs::read_to_string(wt_path.join("branch-note.txt")).unwrap(),
        "saved-on-branch\n"
    );
    assert_eq!(reopened.cwd, wt.path);
    assert_eq!(
        reopened.worktree.as_ref().map(|w| w.branch.as_str()),
        Some(wt.branch.as_str())
    );
    after_idle.close().await;

    let mut final_client = TestClient::new(Server::new(home.clone()));
    let cleaned_again: WorktreeCleanupStatus =
        final_client.call_ok("worktree/cleanup", json!({})).await;
    assert_eq!(cleaned_again.removed.len(), 1);
    assert!(!wt_path.exists());

    let trashed: TrashResult = final_client
        .call_ok(
            "session/trash",
            json!({ "path": opened.path, "worktree_policy": "keep" }),
        )
        .await;
    assert!(trashed.trashed);
    assert!(tiechui_workspace::project_worktree_paths(&home, &repo).is_empty());
    final_client.close().await;
}

#[tokio::test]
async fn legacy_worktree_registry_session_and_streaming_fs_rpc_round_trip_and_disconnect_cleanup() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use tiechui_protocol::workspace::{
        DownloadReadResult, DownloadStartResult, FileStat, FileTree, SearchResult,
        UploadFinishResult, UploadStartResult, UploadWriteResult,
    };

    let Some(home) = isolated_home_child(
        "TC_WT_LEGACY_TRANSFER_HOME",
        "app_server::worktree_tests::legacy_worktree_registry_session_and_streaming_fs_rpc_round_trip_and_disconnect_cleanup",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let repo = workspace_dir.path().join("rpc-transfer-repo");
    init_repo(&repo);

    let seed_client = new_configured_client(&home).await;
    seed_client.close().await;

    let server = Server::new(home.clone());
    let mut client = TestClient::new(server.clone());
    let _: Value = client
        .call_ok("initialize", json!({ "protocol_version": 1 }))
        .await;

    let created: WorktreeEntry = client
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    assert_eq!(
        Path::new(&created.path),
        repo.join(".tiechui/worktrees").join(&created.name)
    );
    std::fs::write(
        Path::new(&created.path).join("wt_rpc.txt"),
        "RPC_WT_MARKER_77\n",
    )
    .unwrap();

    let repo_tree: FileTree = client.call_ok("fs/tree", json!({ "root": repo })).await;
    assert!(
        repo_tree
            .entries
            .iter()
            .all(|e| !e.path.starts_with(".tiechui"))
    );
    let wt_tree: FileTree = client
        .call_ok("fs/tree", json!({ "root": created.path }))
        .await;
    assert!(wt_tree.entries.iter().any(|e| e.path == "wt_rpc.txt"));

    let repo_search: SearchResult = client
        .call_ok(
            "fs/search",
            json!({ "root": repo, "query": "RPC_WT_MARKER_77" }),
        )
        .await;
    assert_eq!(repo_search.total, 0);
    let wt_search: SearchResult = client
        .call_ok(
            "fs/search",
            json!({ "root": created.path, "query": "RPC_WT_MARKER_77" }),
        )
        .await;
    assert_eq!(wt_search.total, 1);

    let legacy_dir = home.join("worktrees/rpc-transfer-repo-legacy/20261101-abcd1234");
    std::fs::create_dir_all(legacy_dir.parent().unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            legacy_dir.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "tiechui/20261101-abcd1234",
            legacy_dir.to_str().unwrap(),
            "release/v1",
        ],
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&legacy_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let reg_path = home.join("worktrees/registry.json");
    let mut reg: Value = serde_json::from_slice(&std::fs::read(&reg_path).unwrap()).unwrap();
    reg["worktrees"].as_array_mut().unwrap().push(json!({
        "name": "20261101-abcd1234",
        "path": legacy_dir.display().to_string(),
        "project_root": repo.display().to_string(),
        "branch": "tiechui/20261101-abcd1234",
        "base_branch": "release/v1",
        "created_at": 1_000,
        "post_create": { "status": "none", "output": "" }
    }));
    std::fs::write(&reg_path, serde_json::to_vec_pretty(&reg).unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&reg_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let legacy_snap: Snapshot = client
        .call_ok(
            "session/open",
            json!({ "cwd": legacy_dir, "model": "fixture/m1" }),
        )
        .await;
    assert_eq!(
        legacy_snap.worktree.as_ref().map(|w| w.branch.as_str()),
        Some("tiechui/20261101-abcd1234")
    );
    let _: Value = client
        .call_ok(
            "session/close",
            json!({ "thread_id": legacy_snap.thread_id }),
        )
        .await;
    append_turn(Path::new(&legacy_snap.path), "legacy session msg");

    let sessions: Vec<SessionSummary> =
        client.call_ok("session/list", json!({ "cwd": repo })).await;
    assert!(
        sessions
            .iter()
            .any(|s| s.thread_id == legacy_snap.thread_id && s.worktree.is_some())
    );
    assert_doctor_ok(&home).await;

    let payload = b"\x00\xff\xfestreamed-over-rpc\x00";
    let up: UploadStartResult = client
        .call_ok(
            "fs/upload_start",
            json!({ "root": repo, "path": "bin/data.bin", "overwrite": false, "size": payload.len() }),
        )
        .await;
    let wr: UploadWriteResult = client
        .call_ok(
            "fs/upload_write",
            json!({ "transfer_id": up.transfer_id, "data": STANDARD.encode(&payload[..8]) }),
        )
        .await;
    assert_eq!(wr.bytes_written, 8);
    let _: UploadWriteResult = client
        .call_ok(
            "fs/upload_write",
            json!({ "transfer_id": up.transfer_id, "data": STANDARD.encode(&payload[8..]) }),
        )
        .await;
    let done: UploadFinishResult = client
        .call_ok("fs/upload_finish", json!({ "transfer_id": up.transfer_id }))
        .await;
    assert_eq!(done.size, payload.len() as u64);

    let stats: Vec<FileStat> = client
        .call_ok(
            "fs/stat_many",
            json!({ "root": repo, "paths": ["bin/data.bin", "bin/missing.bin"] }),
        )
        .await;
    assert!(stats[0].exists && !stats[0].dir && stats[0].size == payload.len() as u64);
    assert!(!stats[1].exists);

    let dl: DownloadStartResult = client
        .call_ok(
            "fs/download_start",
            json!({ "root": repo, "path": "bin/data.bin" }),
        )
        .await;
    let rd: DownloadReadResult = client
        .call_ok(
            "fs/download_read",
            json!({ "transfer_id": dl.transfer_id, "chunk_size": 1024 }),
        )
        .await;
    assert!(rd.eof);
    assert_eq!(STANDARD.decode(&rd.data).unwrap(), payload);

    let mut transient = TestClient::new(server.clone());
    let _: Value = transient
        .call_ok("initialize", json!({ "protocol_version": 1 }))
        .await;
    let _: UploadStartResult = transient
        .call_ok(
            "fs/upload_start",
            json!({ "root": repo, "path": "dropped-client/half.bin", "overwrite": false }),
        )
        .await;
    transient.disconnect();
    assert!(
        !repo.join("dropped-client").exists(),
        "client disconnect did not clean up unfinished upload directory"
    );

    client.close().await;
}
