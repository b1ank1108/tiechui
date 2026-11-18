#![allow(clippy::unwrap_used)]

use std::path::Path;

use serde_json::{Value, json};
use tiechui_protocol::app::{LiveState, SessionSummary, Snapshot, TrashResult};
use tiechui_protocol::workspace::WorktreeEntry;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::tests::{TestClient, isolated_home_child, sse_chat};
use super::worktree_tests::{
    append_turn, assert_doctor_ok, init_repo, new_configured_client, run_user_turn,
    setup_mock_server_and_client, sse_tool_calls,
};

#[cfg(unix)]
fn assert_mode_0700(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    assert_eq!(
        mode,
        0o700,
        "unexpected mode {mode:04o} on {}",
        path.display()
    );
}

async fn verify_invalid_and_failed_scratch_opens(client: &mut TestClient, home: &Path) {
    for bad in [
        json!({ "cwd": home, "scratch": { "summary": "x" } }),
        json!({ "path": home.join("missing.jsonl"), "scratch": {} }),
    ] {
        assert!(client.call("session/open", bad).await.is_err());
    }
    assert!(
        client
            .call(
                "session/open",
                json!({ "scratch": {}, "model": "missing/model" })
            )
            .await
            .is_err()
    );
    assert!(
        home.join("scratch/default").is_dir(),
        "failed open removed the shared scratch dir"
    );
}

#[tokio::test]
async fn scratch_open_shares_private_default_dir_executes_bash_there_and_passes_doctor() {
    let Some(home) = isolated_home_child(
        "TC_SCRATCH_OPEN_BASH_HOME",
        "app_server::scratch_tests::scratch_open_shares_private_default_dir_executes_bash_there_and_passes_doctor",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();

    let (_mock, mut client) = setup_mock_server_and_client(
        &home,
        vec![
            sse_tool_calls(&[(
                "call-scratch-bash",
                "bash",
                json!({
                    "command": "pwd > pwd.txt && mkdir -m 0755 sub && printf 'scratch-ok\\n' > sub/out.txt"
                }),
            )]),
            sse_chat("scratch command finished"),
        ],
    )
    .await;

    let initial: Vec<SessionSummary> = client.call_ok("scratch/list", json!({})).await;
    assert!(initial.is_empty());
    assert!(!home.join("scratch").exists());

    client.drain_notices();
    let first: Snapshot = client
        .call_ok("session/open", json!({ "scratch": {}, "mode": "yolo" }))
        .await;
    client.wait_sessions_changed().await;
    let second: Snapshot = client
        .call_ok("session/open", json!({ "scratch": {} }))
        .await;

    let default = home.join("scratch/default");
    assert_eq!(Path::new(&first.cwd), default);
    assert_eq!(second.cwd, first.cwd);
    assert_ne!(second.thread_id, first.thread_id);
    assert_eq!(first.worktree, None);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            home.join("providers.toml"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert_mode_0700(&home.join("scratch"));
        assert_mode_0700(&default);
    }

    verify_invalid_and_failed_scratch_opens(&mut client, &home).await;

    let _ = run_user_turn(&mut client, &first.thread_id, "执行目录检查").await;
    let first_dir = Path::new(&first.cwd);
    let recorded_pwd = std::fs::read_to_string(first_dir.join("pwd.txt")).unwrap();
    assert_eq!(
        Path::new(recorded_pwd.trim()).canonicalize().unwrap(),
        first_dir.canonicalize().unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(first_dir.join("sub/out.txt")).unwrap(),
        "scratch-ok\n"
    );

    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": first.thread_id }))
        .await;
    let reopened: Snapshot = client
        .call_ok("session/open", json!({ "path": first.path }))
        .await;
    assert_eq!(reopened.cwd, first.cwd);
    assert_eq!(reopened.worktree, None);

    assert_doctor_ok(&home).await;
    client.close().await;
}

async fn attach_lightweight_title_mock(client: &mut TestClient) -> MockServer {
    let light = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(sse_chat("自动生成的临时标题"), "text/event-stream"),
        )
        .mount(&light)
        .await;
    let _: Value = client
        .call_ok(
            "providers/save",
            json!({
                "name": "light",
                "wire": "openai-completions",
                "base_url": light.uri(),
                "api_key": "sk-light"
            }),
        )
        .await;
    let _: Value = client
        .call_ok(
            "providers/save_model",
            json!({
                "provider": "light",
                "previous": null,
                "model": {
                    "id": "fast",
                    "context_window": 64000,
                    "max_output": 2048,
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
            json!({ "key": "lightweight_model", "value": "light/fast" }),
        )
        .await;
    light
}

async fn seed_project_worktree_and_legacy_scratch_sessions(home: &Path, repo: &Path) -> Snapshot {
    let mut setup = new_configured_client(home).await;
    let wt: WorktreeEntry = setup
        .call_ok("worktree/create", json!({ "root": repo }))
        .await;
    let proj_snap: Snapshot = setup
        .call_ok(
            "session/open",
            json!({ "cwd": repo, "model": "fixture/m1" }),
        )
        .await;
    let wt_snap: Snapshot = setup
        .call_ok(
            "session/open",
            json!({ "cwd": wt.path, "model": "fixture/m1" }),
        )
        .await;
    let legacy_dir = home.join("scratch/2026-11-14-legacy-aaaa1111");
    tiechui_config::private::create_dir_all(&legacy_dir).unwrap();
    let legacy: Snapshot = setup
        .call_ok(
            "session/open",
            json!({ "cwd": legacy_dir, "model": "fixture/m1" }),
        )
        .await;
    setup.close().await;
    append_turn(Path::new(&proj_snap.path), "project turn");
    append_turn(Path::new(&wt_snap.path), "worktree turn");
    append_turn(Path::new(&legacy.path), "legacy scratch turn");
    legacy
}

#[tokio::test]
async fn scratch_list_isolates_from_projects_tracks_live_and_archived_and_emits_sessions_changed() {
    let Some(home) = isolated_home_child(
        "TC_SCRATCH_LIST_HOME",
        "app_server::scratch_tests::scratch_list_isolates_from_projects_tracks_live_and_archived_and_emits_sessions_changed",
    ) else {
        return;
    };
    tiechui_config::private::create_dir_all(&home).unwrap();
    let workspace_dir = tempfile::tempdir().unwrap();
    let repo = workspace_dir.path().join("normal-repo");
    init_repo(&repo);
    let legacy = seed_project_worktree_and_legacy_scratch_sessions(&home, &repo).await;

    let (_main_mock, mut client) =
        setup_mock_server_and_client(&home, vec![sse_chat("reply 1"), sse_chat("reply 2")]).await;
    let _light_mock = attach_lightweight_title_mock(&mut client).await;

    let s1: Snapshot = client
        .call_ok("session/open", json!({ "scratch": {} }))
        .await;
    let live_before_turn: Vec<SessionSummary> = client.call_ok("scratch/list", json!({})).await;
    assert_eq!(live_before_turn.len(), 2);
    assert_eq!(live_before_turn[0].thread_id, s1.thread_id);
    assert_eq!(live_before_turn[0].cwd, s1.cwd);
    assert_eq!(live_before_turn[0].live, Some(LiveState::Idle));
    assert_eq!(live_before_turn[1].thread_id, legacy.thread_id);

    client.drain_notices();
    let _ = run_user_turn(&mut client, &s1.thread_id, "帮我整理一段 JSON").await;
    client.wait_sessions_changed().await;
    client.drain_title_tasks().await;

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let s2: Snapshot = client
        .call_ok("session/open", json!({ "scratch": {} }))
        .await;
    assert_eq!(s2.cwd, s1.cwd);
    let _ = run_user_turn(&mut client, &s2.thread_id, "算一下磁盘占用").await;
    client.drain_title_tasks().await;
    let _: Value = client
        .call_ok("session/close", json!({ "thread_id": s1.thread_id }))
        .await;

    let listed: Vec<SessionSummary> = client.call_ok("scratch/list", json!({})).await;
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].thread_id, s2.thread_id);
    assert_eq!(listed[0].live, Some(LiveState::Idle));
    assert_eq!(listed[1].thread_id, s1.thread_id);
    assert_eq!(listed[1].live, None);
    assert_eq!(listed[1].title.as_deref(), Some("自动生成的临时标题"));
    assert_eq!(listed[2].thread_id, legacy.thread_id);
    assert_eq!(listed[2].cwd, legacy.cwd);

    let proj_sessions: Vec<SessionSummary> =
        client.call_ok("session/list", json!({ "cwd": repo })).await;
    assert_eq!(proj_sessions.len(), 2);
    assert!(
        proj_sessions
            .iter()
            .all(|s| s.thread_id != s1.thread_id && s.thread_id != s2.thread_id)
    );

    client.drain_notices();
    let _: Value = client
        .call_ok(
            "session/rename",
            json!({ "path": s2.path, "title": "手动重命名临时会话" }),
        )
        .await;
    client.wait_sessions_changed().await;

    client.drain_notices();
    let _: Value = client
        .call_ok(
            "session/archive",
            json!({ "path": s1.path, "archived": true }),
        )
        .await;
    client.wait_sessions_changed().await;

    let active_only: Vec<SessionSummary> = client.call_ok("scratch/list", json!({})).await;
    assert_eq!(active_only.len(), 2);
    assert_eq!(active_only[0].thread_id, s2.thread_id);
    assert_eq!(active_only[0].title.as_deref(), Some("手动重命名临时会话"));

    let with_archived: Vec<SessionSummary> = client
        .call_ok("scratch/list", json!({ "archived": true }))
        .await;
    assert_eq!(with_archived.len(), 3);
    assert!(
        with_archived
            .iter()
            .any(|s| s.thread_id == s1.thread_id && s.archived_at.is_some())
    );

    client.drain_notices();
    let trashed: TrashResult = client
        .call_ok("session/trash", json!({ "path": s2.path }))
        .await;
    assert!(trashed.trashed);
    client.wait_sessions_changed().await;
    assert!(
        Path::new(&s2.cwd).is_dir(),
        "scratch dir must not be auto-deleted"
    );

    let after_trash: Vec<SessionSummary> = client.call_ok("scratch/list", json!({})).await;
    assert_eq!(after_trash.len(), 1);
    assert_eq!(after_trash[0].thread_id, legacy.thread_id);

    client.close().await;
}
