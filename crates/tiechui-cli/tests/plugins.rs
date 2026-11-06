#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_chui"))
        .env_clear()
        .env("HOME", home)
        .env("TIECHUI_HOME", home)
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .output()
        .unwrap()
}

fn setup() -> tempfile::TempDir {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-cli-tests");
    fs::create_dir_all(&directory).unwrap();
    let temp = tempfile::tempdir_in(directory).unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join(".tiechui-plugin")).unwrap();
    fs::create_dir_all(source.join("skills/demo")).unwrap();
    fs::create_dir_all(source.join("prompts")).unwrap();
    fs::create_dir_all(source.join("agents")).unwrap();
    fs::create_dir_all(temp.path().join("work")).unwrap();
    fs::create_dir_all(temp.path().join("home")).unwrap();
    fs::write(source.join(".tiechui-plugin/plugin.json"), json!({"name":"demo-plugin","extension":{"command":"never-run"},
        "hooks":{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"UserPromptSubmit\",\"additionalContext\":\"Plugin hook context\"}}'"}]}]}},
        "mcpServers":{"plugin-server":{"command":"/nonexistent/plugin-fixture","enabled":false}}
    }).to_string()).unwrap();
    fs::write(source.join("skills/demo/SKILL.md"), "---\nname: demo-plugin-skill\ndescription: Plugin skill catalog entry.\n---\nPlugin instructions.\n").unwrap();
    temp
}

#[test]
fn add_list_toggle_inspect_contributions_and_remove() {
    let temp = setup();
    let home = temp.path().join("home");
    let work = temp.path().join("work");
    let source = temp.path().join("source");
    let output = run(&home, &work, &["plugin", "add", source.to_str().unwrap()]);
    assert!(output.status.success(), "{output:?}");
    assert!(source.exists());
    assert!(
        home.join("plugins/demo-plugin/.tiechui-plugin/plugin.json")
            .exists()
    );
    let list = || {
        let output = run(&home, &work, &["plugin", "list", "--json"]);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    let plugin = list()["plugins"][0].clone();
    assert_eq!(plugin["name"], "demo-plugin");
    assert_eq!(plugin["enabled"], true);
    assert!(
        plugin["installation"]["content_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
    assert_eq!(plugin["prompts"].as_array().unwrap().len(), 1);
    assert_eq!(plugin["agents"].as_array().unwrap().len(), 1);
    let skills = run(&home, &work, &["skills", "list", "--json"]);
    assert!(String::from_utf8_lossy(&skills.stdout).contains("demo-plugin-skill"));
    let mcp = run(&home, &work, &["mcp", "list", "--json"]);
    assert!(mcp.status.success(), "{mcp:?}");
    assert!(String::from_utf8_lossy(&mcp.stdout).contains("plugin-server"));
    let hooks = run(&home, &work, &["hooks", "list", "--json"]);
    assert!(String::from_utf8_lossy(&hooks.stdout).contains("unapproved"));
    assert!(run(&home, &work, &["hooks", "trust"]).status.success());
    let hooks = run(&home, &work, &["hooks", "list", "--json"]);
    assert!(String::from_utf8_lossy(&hooks.stdout).contains("trusted"));
    fs::write(home.join("config.toml"), "[modules]\ndemo-plugin = false\n").unwrap();
    assert_eq!(list()["plugins"][0]["enabled"], false);
    for command in ["skills", "mcp", "hooks"] {
        let output = run(&home, &work, &[command, "list", "--json"]);
        assert!(output.status.success(), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("demo-plugin-skill"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("plugin-server"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("UserPromptSubmit"));
    }
    assert!(
        !run(&home, &work, &["plugin", "remove", "../source"])
            .status
            .success()
    );
    assert!(
        run(&home, &work, &["plugin", "remove", "demo-plugin"])
            .status
            .success()
    );
    assert!(list()["plugins"].as_array().unwrap().is_empty());
    assert!(source.exists());
}

#[tokio::test]
async fn exec_uses_plugin_skill_catalog_and_approved_hook_context() {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    let temp = setup();
    let home = temp.path().join("home");
    let work = temp.path().join("work");
    let source = temp.path().join("source");
    assert!(
        run(&home, &work, &["plugin", "add", source.to_str().unwrap()])
            .status
            .success()
    );
    assert!(run(&home, &work, &["hooks", "trust"]).status.success());
    let provider = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("../../tiechui-providers/tests/fixtures/e2e/anthropic-resume.sse"),
            "text/event-stream",
        ))
        .mount(&provider)
        .await;
    fs::write(home.join("config.toml"), "model = 'fixture/model'\n").unwrap();
    fs::write(home.join("providers.toml"), format!("[fixture]\nwire = 'anthropic-messages'\nbase_url = '{}'\napi_key_env = 'TC_PLUGIN_FIXTURE_KEY'\nmodels = [{{id = 'model', context_window = 200000, max_output = 4096}}]\n", provider.uri())).unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_chui"))
        .env_clear()
        .env("HOME", &home)
        .env("TIECHUI_HOME", &home)
        .env("TC_PLUGIN_FIXTURE_KEY", "fixture-only")
        .args(["exec", "Hello", "--cwd"])
        .arg(&work)
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let requests = provider.received_requests().await.unwrap();
    let body = String::from_utf8(requests[0].body.clone()).unwrap();
    assert!(body.contains("demo-plugin-skill"), "{body}");
    assert!(body.contains("Plugin hook context"), "{body}");
}

#[test]
fn nested_project_plugin_is_listed_but_gated_by_project_trust() {
    let temp = setup();
    let home = temp.path().join("home");
    let work = temp.path().join("work");
    fs::create_dir(work.join(".git")).unwrap();
    fs::create_dir_all(work.join(".agents/plugins")).unwrap();
    fs::rename(
        temp.path().join("source"),
        work.join(".agents/plugins/demo-plugin"),
    )
    .unwrap();
    let nested = work.join("nested");
    fs::create_dir(&nested).unwrap();
    let skills = || {
        let output = run(&home, &nested, &["skills", "list", "--json"]);
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    assert!(!skills().contains("demo-plugin-skill"));
    assert!(run(&home, &work, &["trust", "allow"]).status.success());
    assert!(skills().contains("demo-plugin-skill"));
    assert!(run(&home, &nested, &["trust", "deny"]).status.success());
    assert!(!skills().contains("demo-plugin-skill"));
}
