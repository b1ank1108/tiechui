#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use std::path::Path;
use std::process::{Command, Output};

fn command(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tc"))
        .env_clear()
        .env("TIECHUI_HOME", home)
        .env("HOME", home)
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .output()
        .unwrap()
}

#[test]
fn list_and_trust_require_both_project_and_hash_approval() {
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let home = root.path().join("TIECHUI_HOME");
    let cwd = root.path().join("work");
    std::fs::create_dir_all(cwd.join(".codex")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let file = cwd.join(".codex/hooks.json");
    let config = json!({"hooks":{"PreToolUse":[{"matcher":"Bash|bash", "hooks":[{"type":"command","command":"true"}]}]}});
    std::fs::write(&file, config.to_string()).unwrap();
    std::fs::write(
        home.join("hooks.json"),
        json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"true"}]}]}}).to_string(),
    )
    .unwrap();
    let list = || {
        let output = command(&home, &cwd, &["hooks", "list", "--json"]);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<Value>(&output.stdout).unwrap()
    };
    assert_eq!(list()["hooks"][1]["trust"], "untrusted_project");
    assert!(!command(&home, &cwd, &["hooks", "trust"]).status.success());
    assert!(command(&home, &cwd, &["trust", "allow"]).status.success());
    assert_eq!(list()["hooks"][1]["trust"], "unapproved");
    assert!(command(&home, &cwd, &["hooks", "trust"]).status.success());
    assert_eq!(list()["hooks"][1]["enabled"], true);
    let mut changed = config;
    changed["hooks"]["PreToolUse"][0]["hooks"][0]["command"] = json!("false");
    std::fs::write(file, changed.to_string()).unwrap();
    assert_eq!(list()["hooks"][1]["trust"], "changed");
    assert_eq!(list()["hooks"][1]["enabled"], false);
    assert!(command(&home, &cwd, &["hooks", "trust"]).status.success());
    assert_eq!(list()["hooks"][1]["enabled"], true);
    let text = command(&home, &cwd, &["hooks", "list"]);
    assert!(text.status.success());
    assert!(
        String::from_utf8(text.stdout)
            .unwrap()
            .contains("PreToolUse\tenabled")
    );
    assert!(command(&home, &cwd, &["trust", "deny"]).status.success());
    assert_eq!(list()["hooks"][1]["trust"], "untrusted_project");
}
