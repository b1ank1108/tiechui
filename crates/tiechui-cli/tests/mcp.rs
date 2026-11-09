#![allow(clippy::unwrap_used)]

#[path = "../../../tests/support/env.rs"]
mod env_support;

use std::path::Path;
use std::process::{Command, Output};

#[tokio::test]
async fn oauth_login_and_logout_use_the_selected_profile() {
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/mcp-cli-tests");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(root).unwrap();
    let server = MockServer::start().await;
    let base = server.uri();
    Mock::given(method("GET"))
        .and(path("/.well-known/oauth-protected-resource/mcp"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"resource":format!("{base}/mcp"), "authorization_servers":[base]}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET")).and(path("/.well-known/oauth-authorization-server"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"issuer":base,"authorization_endpoint":format!("{base}/authorize"),"token_endpoint":format!("{base}/token"),"registration_endpoint":format!("{base}/register"),"code_challenge_methods_supported":["S256"]}))).mount(&server).await;
    Mock::given(method("POST"))
        .and(path("/register"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"client_id":"cli-fixture"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token":"cli-fake-token","token_type":"Bearer","expires_in":3600}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    std::fs::write(
        temp.path().join("config.toml"),
        format!("[mcp_servers.fixture]\nurl = '{base}/mcp'\nenabled = false\n"),
    )
    .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_chui"))
        .env_clear()
        .envs(env_support::windows_env())
        .env("HOME", temp.path())
        .env("TIECHUI_HOME", temp.path())
        .current_dir(temp.path())
        .args(["mcp", "login", "fixture"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap()).lines();
    let url = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(line) = output.next_line().await.unwrap() {
            if line.starts_with("http://") {
                return line;
            }
        }
        panic!("login did not print authorization URL");
    })
    .await
    .unwrap();
    let url = reqwest::Url::parse(&url).unwrap();
    let fields: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    let mut callback = reqwest::Url::parse(&fields["redirect_uri"]).unwrap();
    callback.query_pairs_mut().extend_pairs([
        ("code", "fixture-code"),
        ("state", fields["state"].as_str()),
    ]);
    assert!(reqwest::get(callback).await.unwrap().status().is_success());
    assert!(child.wait().await.unwrap().success());
    let status = run(temp.path(), temp.path(), &["mcp", "list", "--json"]);
    let status: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["servers"][0]["oauth"], "authenticated");
    assert!(
        run(temp.path(), temp.path(), &["mcp", "logout", "fixture"])
            .status
            .success()
    );
    let status = run(temp.path(), temp.path(), &["mcp", "list", "--json"]);
    assert!(String::from_utf8_lossy(&status.stdout).contains("logged_out"));
    assert!(
        !run(temp.path(), temp.path(), &["mcp", "login", "unknown"])
            .status
            .success()
    );
}

fn fixture_provider(home: &Path, base_url: &str) {
    std::fs::write(home.join("providers.toml"), format!("[fixture]\nwire = 'anthropic-messages'\nbase_url = '{base_url}'\napi_key_env = 'TC_MCP_FAKE_KEY'\nmodels = [{{id = 'model', context_window = 200000, max_output = 4096}}]\n")).unwrap();
}

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chui"))
        .env_clear()
        .envs(env_support::windows_env())
        .env("HOME", home)
        .env("TIECHUI_HOME", home)
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .output()
        .unwrap()
}

#[test]
fn list_reports_failures_disabled_servers_unknown_keys_and_project_trust() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/mcp-cli-tests");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(root).unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(cwd.join(".agents")).unwrap();
    std::fs::write(home.join("config.toml"), "[mcp_servers.disabled]\ncommand = '/nonexistent/disabled'\nenabled = false\nfuture_option = true\n[mcp_servers.failed]\ncommand = '/nonexistent/failed'\n").unwrap();
    std::fs::write(
        cwd.join(".agents/config.toml"),
        "[mcp_servers.project]\ncommand = '/nonexistent/project'\n",
    )
    .unwrap();
    let list = || {
        let output = run(&home, &cwd, &["mcp", "list", "--json"]);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let untrusted = list();
    assert_eq!(untrusted["servers"].as_array().unwrap().len(), 2);
    assert_eq!(untrusted["servers"][0]["state"], "disabled");
    assert_eq!(untrusted["servers"][1]["state"], "failed");
    assert!(
        !untrusted["servers"][1]["error"]
            .as_str()
            .unwrap()
            .is_empty()
    );
    assert!(
        untrusted["config_diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["key"] == "mcp_servers.disabled.future_option")
    );
    assert!(run(&home, &cwd, &["trust", "allow"]).status.success());
    assert_eq!(list()["servers"].as_array().unwrap().len(), 3);
    let text = run(&home, &cwd, &["mcp", "list"]);
    assert!(text.status.success());
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains("disabled\tDisabled"));
    assert!(text.contains("failed\tFailed"));
    assert!(text.contains("error:"));
}

#[tokio::test]
async fn exec_first_turn_finishes_while_server_is_still_starting() {
    use std::time::Duration;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/mcp-cli-tests");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(root).unwrap();
    let home = temp.path().join("home");
    let cwd = temp.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("../../tiechui-providers/tests/fixtures/e2e/anthropic-resume.sse"),
            "text/event-stream",
        ))
        .mount(&provider)
        .await;
    fixture_provider(&home, &provider.uri());
    std::fs::write(home.join("config.toml"), "model = 'fixture/model'\n[mcp_servers.slow]\ncommand = '/bin/sleep'\nargs = ['30']\nstartup_timeout_sec = 30\n").unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_chui"))
            .env_clear()
            .envs(env_support::windows_env())
            .env("HOME", &home)
            .env("TIECHUI_HOME", &home)
            .env("TC_MCP_FAKE_KEY", "fixture-only")
            .args(["exec", "Hello", "--cwd"])
            .arg(&cwd)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let requests = provider.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(!body["tools"].to_string().contains("mcp__slow"));
}

#[tokio::test]
async fn exec_first_request_includes_a_server_that_starts_promptly() {
    use std::time::Duration;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/mcp-cli-tests");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(root).unwrap();
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("../../tiechui-providers/tests/fixtures/e2e/anthropic-resume.sse"),
            "text/event-stream",
        ))
        .mount(&provider)
        .await;
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let request: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let (result, delay) = match request["method"].as_str().unwrap_or_default() {
                "initialize" => (serde_json::json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}), 500),
                "tools/list" => (serde_json::json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}), 0),
                _ => return ResponseTemplate::new(202),
            };
            ResponseTemplate::new(200)
                .insert_header("mcp-session-id", "fixture")
                .set_body_json(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
                .set_delay(Duration::from_millis(delay))
        })
        .mount(&server)
        .await;
    fixture_provider(temp.path(), &provider.uri());
    std::fs::write(
        temp.path().join("config.toml"),
        format!(
            "model = 'fixture/model'\n[mcp_servers.http]\nurl = '{}'\n",
            server.uri()
        ),
    )
    .unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_chui"))
        .env_clear()
        .envs(env_support::windows_env())
        .env("HOME", temp.path())
        .env("TIECHUI_HOME", temp.path())
        .env("TC_MCP_FAKE_KEY", "fixture-only")
        .args(["exec", "Hello", "--cwd"])
        .arg(temp.path())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let requests = provider.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(
        body["tools"].to_string().contains("mcp__http__echo"),
        "{body}"
    );
}

#[tokio::test]
async fn list_http_tools_sends_static_headers_and_environment_bearer_token() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method},
    };
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/mcp-cli-tests");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(root).unwrap();
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
    Mock::given(method("DELETE"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("POST")).and(header("authorization", "Bearer fixture-only")).and(header("x-fixture", "static-value"))
        .respond_with(|request: &wiremock::Request| {
            let request: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            let result = match request["method"].as_str().unwrap_or_default() {
                "initialize" => serde_json::json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}),
                "tools/list" => serde_json::json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}),
                _ => return ResponseTemplate::new(202),
            };
            ResponseTemplate::new(200).insert_header("mcp-session-id", "fixture").set_body_json(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
        }).mount(&server).await;
    std::fs::write(temp.path().join("config.toml"), format!("[mcp_servers.http]\nurl = '{}'\nbearer_token_env_var = 'TC_MCP_FIXTURE_TOKEN'\nhttp_headers = {{ x-fixture = 'static-value' }}\n", server.uri())).unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_chui"))
        .env_clear()
        .envs(env_support::windows_env())
        .env("HOME", temp.path())
        .env("TIECHUI_HOME", temp.path())
        .env("TC_MCP_FIXTURE_TOKEN", "fixture-only")
        .args(["mcp", "list", "--json", "--cwd"])
        .arg(temp.path())
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let list: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(list["servers"][0]["state"], "ready", "{list}");
    assert_eq!(list["servers"][0]["tools"][0], "mcp__http__echo");
}
