#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use std::{
    fs,
    os::unix::{
        fs::{PermissionsExt, symlink},
        process::CommandExt,
    },
    path::Path,
    process::Command,
    sync::Arc,
};
use tiechui_config::TrustStore;
use tiechui_module_mcp::oauth::OAuthStore;
use tiechui_module_plugins::{Extension, PluginHost};
use tiechui_plugin_api::{
    CancellationToken, ModuleConfig, ToolError, async_trait,
    protocol::{ApprovalDecision, PluginPrompt},
};

fn mode(path: &Path, expected: u32) {
    assert_eq!(
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o7777,
        expected,
        "{}",
        path.display()
    );
}

#[test]
fn fresh_profile_paths_are_private_and_pass_doctor() {
    // umask is process-wide; isolate it from every other test and runtime thread.
    let Ok(root) = std::env::var("TIECHUI_PERMISSION_TEST_ROOT") else {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .env_clear()
            .env("TIECHUI_PERMISSION_TEST_ROOT", root.path())
            .env("HOME", root.path())
            .env("PATH", "/usr/bin:/bin")
            .current_dir(root.path())
            .args([
                "--exact",
                "fresh_profile_paths_are_private_and_pass_doctor",
                "--nocapture",
            ]);
        unsafe {
            child.pre_exec(|| {
                libc::umask(0);
                Ok(())
            });
        }
        let result = child.output().unwrap();
        assert!(result.status.success(), "{result:?}");
        return;
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(exercise(Path::new(&root)));
}

struct Approve;
#[async_trait]
impl PluginHost for Approve {
    async fn ask(
        &self,
        _: &str,
        _: PluginPrompt,
        _: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError> {
        Ok(ApprovalDecision::Accept { content: None })
    }
}

async fn exercise(root: &Path) {
    let home = root.join("profile");
    let cancel = CancellationToken::new();
    let preferences = root.join("preferences-profile/config.toml");
    tiechui_config::save_preference(&preferences, "model", "local/fixture".into()).unwrap();
    mode(preferences.parent().unwrap(), 0o700);
    mode(&preferences, 0o600);

    let trust_home = root.join("trust-profile");
    TrustStore::new(&trust_home).get(root, &cancel).unwrap();
    mode(&trust_home, 0o700);
    let trust_lock = trust_home.join("trust.json.lock");
    mode(&trust_lock, 0o600);
    fs::set_permissions(&trust_home, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&trust_lock, fs::Permissions::from_mode(0o644)).unwrap();
    TrustStore::new(&trust_home).get(root, &cancel).unwrap();
    mode(&trust_home, 0o700);
    mode(&trust_lock, 0o600);

    let oauth = OAuthStore::new(
        &home,
        "fixture",
        "https://example.invalid/mcp",
        Default::default(),
    );
    assert!(oauth.access_token(&cancel).await.unwrap().is_none());
    let oauth_dir = home.join("mcp-oauth");
    let lock = fs::read_dir(&oauth_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    mode(&home, 0o700);
    mode(&oauth_dir, 0o700);
    mode(&lock, 0o600);

    // Old builds left these inodes readable by other users. Preserve lock identity.
    use std::os::unix::fs::MetadataExt;
    let inode = fs::metadata(&lock).unwrap().ino();
    fs::set_permissions(&oauth_dir, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(oauth.access_token(&cancel).await.unwrap().is_none());
    mode(&oauth_dir, 0o700);
    mode(&lock, 0o600);
    assert_eq!(fs::metadata(&lock).unwrap().ino(), inode);

    let outside = root.join("outside.lock");
    fs::write(&outside, b"untouched").unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o644)).unwrap();
    fs::remove_file(&lock).unwrap();
    symlink(&outside, &lock).unwrap();
    assert!(oauth.access_token(&cancel).await.is_err());
    mode(&outside, 0o644);
    assert_eq!(fs::read(&outside).unwrap(), b"untouched");
    fs::remove_file(&lock).unwrap();
    fs::remove_dir(&oauth_dir).unwrap();
    let outside_dir = root.join("outside-dir");
    fs::create_dir(&outside_dir).unwrap();
    fs::set_permissions(&outside_dir, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&outside_dir, &oauth_dir).unwrap();
    assert!(oauth.access_token(&cancel).await.is_err());
    mode(&outside_dir, 0o755);
    assert_eq!(fs::read_dir(&outside_dir).unwrap().count(), 0);
    fs::remove_file(&oauth_dir).unwrap();
    oauth.logout(&cancel).await.unwrap();

    TrustStore::new(&home)
        .set(root, Some(true), &cancel)
        .unwrap();
    mode(&home.join("trust.json"), 0o600);
    mode(&home.join("trust.json.lock"), 0o600);
    tiechui_config::save_preference(&home.join("config.toml"), "model", "local/fixture".into())
        .unwrap();
    mode(&home.join("config.toml"), 0o600);
    let session = tiechui_session::SessionStore::create(&home, root, &cancel).unwrap();
    mode(session.path(), 0o600);

    let source = root.join("source");
    fs::create_dir_all(source.join(".tiechui-plugin")).unwrap();
    fs::write(source.join(".tiechui-plugin/plugin.json"), serde_json::json!({
        "name":"fixture", "tiechui":{"apiVersion":1},
        "extension":{"command":"server", "capabilities":[], "contributions":{}},
        "hooks":{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command", "command":"printf 'private hook'"}]}]}}
    }).to_string()).unwrap();
    fs::write(source.join("server"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(source.join("server"), fs::Permissions::from_mode(0o755)).unwrap();
    tiechui_module_plugins::add(&source, &home, &cancel).unwrap();
    mode(&home.join("plugins/.lock"), 0o600);
    mode(&home.join("plugins/.records/fixture.json"), 0o600);
    mode(&home.join("plugins/fixture/server"), 0o755);
    let plugins =
        tiechui_module_plugins::discover(root, &home, true, &ModuleConfig::default(), &cancel)
            .unwrap();
    let plugin = &plugins.plugins[0];
    assert!(plugin.diagnostics.is_empty(), "{:?}", plugin.diagnostics);
    let extension =
        Extension::start(plugin, &home, true, Some(Arc::new(Approve)), cancel.clone()).unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        extension.wait_for_startup(cancel.clone()),
    )
    .await
    .unwrap();
    mode(&home.join("plugin-grants/fixture.json"), 0o600);
    let registry = plugins.registry(&ModuleConfig::default()).unwrap();
    assert_eq!(
        tiechui_module_hooks::trust_with_sources(
            root,
            &home,
            true,
            registry.hook_sources(),
            &cancel
        )
        .unwrap(),
        1
    );
    mode(&home.join("hooks-trust.lock"), 0o600);
    mode(&home.join("hooks-trust.json"), 0o600);
    let hooks = tiechui_module_hooks::HooksModule::load_with_sources(
        root,
        &home,
        true,
        Default::default(),
        registry.hook_sources(),
        &cancel,
    )
    .unwrap();
    let result = hooks
        .dispatch(
            tiechui_module_hooks::Event::UserPromptSubmit,
            serde_json::json!({}),
            None,
            root,
            cancel.clone(),
        )
        .await
        .unwrap();
    assert_eq!(result.context, ["private hook"]);
    mode(&plugin.data_root, 0o700);
    assert_private_directories(&home);

    let result = doctor(&home);
    assert!(result.status.success(), "{result:?}");
    assert!(
        String::from_utf8(result.stdout)
            .unwrap()
            .contains("Doctor: OK")
    );
    cancel.cancel();

    let grant_path = home.join("plugin-grants/fixture.json");
    let original = fs::read(&grant_path).unwrap();
    let mut grant: serde_json::Value = serde_json::from_slice(&original).unwrap();
    assert_eq!(
        grant["hash"],
        tiechui_module_plugins::content_hash(&plugin.root, &CancellationToken::new()).unwrap()
    );
    for hash in [
        serde_json::Value::Null,
        serde_json::json!(42),
        serde_json::json!(""),
        serde_json::json!("a".repeat(64)),
        serde_json::json!(format!("sha256:{}", "a".repeat(63))),
        serde_json::json!(format!("sha256:{}", "a".repeat(65))),
        serde_json::json!(format!("sha256:{}g", "a".repeat(63))),
        serde_json::json!(format!("sha512:{}", "a".repeat(64))),
        serde_json::json!(format!("sha256:{}\n", "a".repeat(64))),
    ] {
        grant["hash"] = hash;
        fs::write(&grant_path, serde_json::to_vec(&grant).unwrap()).unwrap();
        let result = doctor(&home);
        assert_eq!(result.status.code(), Some(2), "{grant}: {result:?}");
        assert!(
            String::from_utf8(result.stdout)
                .unwrap()
                .contains("unsupported plugin grant schema")
        );
    }
    fs::write(&grant_path, original).unwrap();
    assert!(doctor(&home).status.success());

    exercise_prompts_agents_and_trash(root).await;
}

async fn exercise_prompts_agents_and_trash(root: &Path) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::process::Command as TokioCommand;

    let app_home = root.join("app-profile");
    assert!(!app_home.exists());
    let mut child = TokioCommand::new(env!("CARGO_BIN_EXE_chui"))
        .env_clear()
        .env("TIECHUI_HOME", &app_home)
        .env("HOME", root)
        .env("PATH", "/usr/bin:/bin")
        .current_dir(root)
        .arg("app-server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();

    let mut rpc = async |id: u64, method: &str, params: serde_json::Value| {
        let req =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        stdin
            .write_all(format!("{req}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
        loop {
            let line = lines.next_line().await.unwrap().unwrap();
            let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
            if msg["id"] == id {
                assert!(msg.get("error").is_none(), "{method} failed: {msg}");
                return;
            }
        }
    };

    rpc(
        1,
        "prompts/save",
        serde_json::json!({
            "cwd": null,
            "scope": "user",
            "name": "review",
            "description": "Review prompt",
            "argument_hint": null,
            "body": "Review carefully",
            "previous": null,
        }),
    )
    .await;
    mode(&app_home, 0o700);
    mode(&app_home.join("prompts"), 0o700);

    let agents_only_home = root.join("agents-profile");
    assert!(!agents_only_home.exists());
    let mut agents_child = TokioCommand::new(env!("CARGO_BIN_EXE_chui"))
        .env_clear()
        .env("TIECHUI_HOME", &agents_only_home)
        .env("HOME", root)
        .env("PATH", "/usr/bin:/bin")
        .current_dir(root)
        .arg("app-server")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut agents_stdin = agents_child.stdin.take().unwrap();
    let mut agents_lines = BufReader::new(agents_child.stdout.take().unwrap()).lines();
    let req = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "agents/save",
        "params": {
            "cwd": null,
            "scope": "user",
            "name": "reviewer",
            "description": "Reviewer agent",
            "tools": null,
            "model": null,
            "thinking": null,
            "prompt": "Review diff",
            "previous": null,
        }
    });
    agents_stdin
        .write_all(format!("{req}\n").as_bytes())
        .await
        .unwrap();
    agents_stdin.flush().await.unwrap();
    loop {
        let line = agents_lines.next_line().await.unwrap().unwrap();
        let msg: serde_json::Value = serde_json::from_str(&line).unwrap();
        if msg["id"] == 1 {
            assert!(msg.get("error").is_none(), "agents/save failed: {msg}");
            break;
        }
    }
    drop(agents_stdin);
    let _ = agents_child.wait().await;
    mode(&agents_only_home, 0o700);
    mode(&agents_only_home.join("agents"), 0o700);

    rpc(
        2,
        "agents/save",
        serde_json::json!({
            "cwd": null,
            "scope": "user",
            "name": "reviewer",
            "description": "Reviewer agent",
            "tools": null,
            "model": null,
            "thinking": null,
            "prompt": "Review diff",
            "previous": null,
        }),
    )
    .await;
    mode(&app_home.join("agents"), 0o700);

    let session =
        tiechui_session::SessionStore::create(&app_home, root, &CancellationToken::new()).unwrap();
    let session_path = session.path().to_path_buf();
    drop(session);
    rpc(
        3,
        "session/trash",
        serde_json::json!({ "path": session_path }),
    )
    .await;
    drop(stdin);
    let _ = child.wait().await;

    mode(&app_home.join("trash"), 0o700);
    assert_private_directories(&app_home);
    assert_private_directories(&agents_only_home);

    let trash_only_home = root.join("trash-profile");
    assert!(!trash_only_home.exists());
    let staging = root.join("staging-profile");
    let staged =
        tiechui_session::SessionStore::create(&staging, root, &CancellationToken::new()).unwrap();
    let staged_path = staged.path().to_path_buf();
    drop(staged);
    tiechui_session::trash(&trash_only_home, &staged_path).unwrap();
    mode(&trash_only_home, 0o700);
    mode(&trash_only_home.join("trash"), 0o700);
    assert_private_directories(&trash_only_home);

    for profile in [&app_home, &agents_only_home, &trash_only_home] {
        let out = doctor(profile);
        assert!(out.status.success(), "{profile:?}: {out:?}");
        assert!(
            String::from_utf8(out.stdout)
                .unwrap()
                .contains("Doctor: OK")
        );
    }
}

fn doctor(home: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_chui"))
        .env_clear()
        .args(["doctor", "--profile"])
        .arg(home)
        .output()
        .unwrap()
}

fn assert_private_directories(path: &Path) {
    mode(path, 0o700);
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            assert_private_directories(&entry.path());
        }
    }
}
