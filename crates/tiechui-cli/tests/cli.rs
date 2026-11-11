#[path = "../../../tests/support/env.rs"]
mod env_support;

use std::process::Command;

const BINARIES: [(&str, &str); 2] = [
    ("chui", env!("CARGO_BIN_EXE_chui")),
    ("tiechui", env!("CARGO_BIN_EXE_tiechui")),
];

#[test]
fn help_uses_the_invoked_name() {
    for (name, binary) in BINARIES {
        let output = Command::new(binary).arg("--help").output().unwrap();
        assert!(output.status.success(), "{name}: {output:?}");
        assert!(output.stderr.is_empty());
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(
            help.contains(&format!("Usage: {name} [OPTIONS] [COMMAND]")),
            "{name}: {help}"
        );
        assert!(help.contains("--help"));
        assert!(help.contains("--version"));
        assert!(help.contains("--no-alt-screen"));
        assert!(help.contains("--yolo"), "{name}: {help}");
        assert!(
            !help.contains("--dangerously-bypass-approvals-and-sandbox"),
            "{name}: {help}"
        );

        let exec_output = Command::new(binary)
            .args(["exec", "--help"])
            .output()
            .unwrap();
        assert!(exec_output.status.success(), "{name} exec: {exec_output:?}");
        let exec_help = String::from_utf8(exec_output.stdout).unwrap();
        assert!(exec_help.contains("--yolo"), "{name} exec: {exec_help}");
        assert!(
            !exec_help.contains("--dangerously-bypass-approvals-and-sandbox"),
            "{name} exec: {exec_help}"
        );
    }
}

#[test]
fn yolo_conflicts_with_mode_and_explicit_approval() {
    let cases: &[(&[&str], &str)] = &[
        (&["--yolo", "--mode", "rules"], "--mode"),
        (&["--yolo", "--mode", "yolo"], "--mode"),
        (&["exec", "x", "--yolo", "--mode", "auto"], "--mode"),
        (&["exec", "x", "--yolo", "--approval", "deny"], "--approval"),
        (
            &["exec", "x", "--yolo", "--approval", "allow-listed"],
            "--approval",
        ),
        (&["exec", "x", "--yolo", "--approval", "yolo"], "--approval"),
    ];
    for (args, conflicting) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_chui"))
            .args(*args)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("--yolo"), "{args:?}: {stderr}");
        assert!(stderr.contains(conflicting), "{args:?}: {stderr}");
    }

    let bypass = Command::new(env!("CARGO_BIN_EXE_chui"))
        .arg("--dangerously-bypass-approvals-and-sandbox")
        .output()
        .unwrap();
    assert_eq!(bypass.status.code(), Some(2), "{bypass:?}");
}

#[tokio::test]
async fn exec_yolo_allows_protected_paths_and_ask_rules_without_mutating_config() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("profile");
    let cwd = temp.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();

    let server = MockServer::start().await;
    let turn = AtomicUsize::new(0);
    let first = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "choices": [{
                "index": 0,
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call-env",
                            "type": "function",
                            "function": {
                                "name": "write",
                                "arguments": r#"{"path":".env","content":"SECRET=1\n"}"#
                            }
                        },
                        {
                            "index": 1,
                            "id": "call-asked",
                            "type": "function",
                            "function": {
                                "name": "write",
                                "arguments": r#"{"path":"asked.txt","content":"ok\n"}"#
                            }
                        },
                        {
                            "index": 2,
                            "id": "call-denied",
                            "type": "function",
                            "function": {
                                "name": "write",
                                "arguments": r#"{"path":"blocked.secret","content":"no\n"}"#
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        })
    );
    let second = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let body = if turn.fetch_add(1, Ordering::SeqCst) == 0 {
                first.clone()
            } else {
                second.clone()
            };
            ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
        })
        .mount(&server)
        .await;

    std::fs::write(
        home.join("providers.toml"),
        format!(
            "[fixture]\nwire = \"openai-completions\"\nbase_url = \"{}\"\napi_key_env = \"TC_CLI_YOLO_KEY\"\nmodels = [{{ id = \"model\", context_window = 200000, max_output = 4096 }}]\n",
            server.uri()
        ),
    )
    .unwrap();
    let config_path = home.join("config.toml");
    let config_before = concat!(
        "model = \"fixture/model\"\n",
        "[[permissions.rules]]\nsurface = \"path\"\npattern = \"asked.txt\"\naction = \"ask\"\n",
        "[[permissions.rules]]\nsurface = \"path\"\npattern = \"blocked.secret\"\naction = \"deny\"\n",
    );
    std::fs::write(&config_path, config_before).unwrap();

    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_chui"))
            .env_clear()
            .envs(env_support::windows_env())
            .env("HOME", temp.path())
            .env("TIECHUI_HOME", &home)
            .env("TC_CLI_YOLO_KEY", "fake-key")
            .args(["exec", "apply", "--yolo", "--json", "--cwd"])
            .arg(&cwd)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(cwd.join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert_eq!(
        std::fs::read_to_string(cwd.join("asked.txt")).unwrap(),
        "ok\n"
    );
    assert!(!cwd.join("blocked.secret").exists());
    assert_eq!(
        std::fs::read_to_string(&config_path).unwrap(),
        config_before
    );

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let second_body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let messages = second_body["messages"].as_array().unwrap();
    let tool_content = |call_id: &str| {
        messages
            .iter()
            .find(|m| m["role"] == "tool" && m["tool_call_id"] == call_id)
            .and_then(|m| m["content"].as_str())
            .unwrap_or_default()
            .to_string()
    };
    assert!(!tool_content("call-env").contains("Denied"));
    assert!(!tool_content("call-asked").contains("Denied"));
    assert!(
        tool_content("call-denied").contains("Denied by policy"),
        "{second_body}"
    );

    let cancel = tokio_util::sync::CancellationToken::new();
    let sessions = tiechui_session::list(&home, &cwd, &cancel).unwrap();
    assert_eq!(sessions.sessions.len(), 1);
    let store =
        tiechui_session::SessionStore::open_read_only(&sessions.sessions[0].path, &cancel).unwrap();
    let decisions: Vec<_> = store
        .entries()
        .iter()
        .filter_map(|entry| match &entry.kind {
            tiechui_session::EntryKind::Custom { kind, data } if kind == "permissions.decision" => {
                Some(data)
            }
            _ => None,
        })
        .collect();
    assert!(
        decisions.iter().any(|d| d["call_id"] == "call-env"
            && d["surface"] == "protected_path"
            && d["outcome"] == "approved"
            && d["source"] == "yolo"),
        "{decisions:?}"
    );
    assert!(
        decisions.iter().any(|d| d["call_id"] == "call-asked"
            && d["surface"] == "path_write"
            && d["outcome"] == "approved"
            && d["source"] == "yolo"),
        "{decisions:?}"
    );
}

#[test]
fn version_uses_the_invoked_name() {
    for (name, binary) in BINARIES {
        let output = Command::new(binary).arg("--version").output().unwrap();
        assert!(output.status.success(), "{name}: {output:?}");
        assert!(output.stderr.is_empty());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{name} {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}

#[cfg(all(windows, target_env = "msvc"))]
#[test]
fn windows_msvc_binaries_reserve_eight_mib_for_the_main_stack() {
    use std::io::{Read, Seek, SeekFrom};

    for (name, binary) in BINARIES {
        let mut file = std::fs::File::open(binary).unwrap();
        file.seek(SeekFrom::Start(0x3c)).unwrap();
        let mut offset = [0; 4];
        file.read_exact(&mut offset).unwrap();
        let pe = u64::from(u32::from_le_bytes(offset));
        file.seek(SeekFrom::Start(pe)).unwrap();
        let mut signature = [0; 4];
        file.read_exact(&mut signature).unwrap();
        assert_eq!(&signature, b"PE\0\0", "{name}");
        file.seek(SeekFrom::Start(pe + 24)).unwrap();
        let mut header = [0; 88];
        file.read_exact(&mut header).unwrap();
        let reserve = match u16::from_le_bytes(header[..2].try_into().unwrap()) {
            0x10b => u64::from(u32::from_le_bytes(header[72..76].try_into().unwrap())),
            0x20b => u64::from_le_bytes(header[72..80].try_into().unwrap()),
            magic => panic!("{name}: unexpected PE optional header {magic:#x}"),
        };
        assert_eq!(reserve, 8 * 1024 * 1024, "{name}");
    }
}
