#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::{Command, Output};

fn command(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tc"))
        .env_clear()
        .env("TIECHUI_HOME", home)
        .env("HOME", home.parent().unwrap())
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .output()
        .unwrap()
}

#[test]
fn trust_cli_is_headless_persistent_and_inherits() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    let child = project.join("child");
    std::fs::create_dir_all(&child).unwrap();
    let output = command(&home, &project, &["trust"]);
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .starts_with("unknown:")
    );
    for (cwd, action, expected) in [
        (&project, "allow", "allowed:"),
        (&child, "status", "allowed:"),
        (&child, "deny", "denied:"),
        (&project, "status", "allowed:"),
    ] {
        let output = command(&home, cwd, &["trust", action]);
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty());
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .starts_with(expected)
        );
    }
    let decisions: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join("trust.json")).unwrap()).unwrap();
    assert_eq!(
        decisions[project.canonicalize().unwrap().to_str().unwrap()],
        true
    );
    assert_eq!(
        decisions[child.canonicalize().unwrap().to_str().unwrap()],
        false
    );
}

#[test]
fn exec_skips_untrusted_config_with_one_diagnostic_and_loads_it_after_allow() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    std::fs::create_dir_all(project.join(".agents")).unwrap();
    std::fs::write(
        project.join(".agents/config.toml"),
        "model = 'project/model'\n[modules]\nexec = false\nediting = false\n",
    )
    .unwrap();
    let output = command(&home, &project, &["exec", "hello"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.starts_with("trust:"))
            .count(),
        1
    );
    assert!(stderr.contains("set model in config.toml"));
    assert!(!stderr.contains("UntrustedProject"));
    assert!(
        command(&home, &project, &["trust", "allow"])
            .status
            .success()
    );
    let output = command(&home, &project, &["exec", "hello"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("trust:"));
    assert!(stderr.contains("unknown provider project"));
}

#[test]
fn project_resources_without_config_are_skipped_without_prompting() {
    for resource in [
        ".claude/skills",
        ".claude/agents",
        ".agents/hooks",
        ".agents/plugins",
        ".agents/mcp.json",
        ".agents/skills",
    ] {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join(resource)).unwrap();
        let output = command(&home, &project, &["exec", "hello"]);
        assert_eq!(output.status.code(), Some(2));
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            stderr
                .lines()
                .filter(|line| line.starts_with("trust:"))
                .count(),
            1,
            "{stderr}"
        );
    }
}

#[test]
fn interactive_start_without_a_terminal_asks_nothing_and_saves_nothing() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    std::fs::create_dir_all(project.join(".agents")).unwrap();
    std::fs::write(project.join(".agents/config.toml"), "model = 'a/b'\n").unwrap();
    let output = command(&home, &project, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("trust: skipping"), "{stderr}");
    assert!(!home.join("trust.json").exists());
}

#[test]
fn legacy_project_directory_does_not_trigger_trust_or_load_configuration() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("project");
    std::fs::create_dir_all(project.join(".tiechui")).unwrap();
    std::fs::write(
        project.join(".tiechui/config.toml"),
        "invalid ignored configuration",
    )
    .unwrap();
    let output = command(&home, &project, &["exec", "hello"]);
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(!stderr.contains("trust:"), "{stderr}");
    assert!(stderr.contains("set model in config.toml"), "{stderr}");
}

#[test]
fn exec_and_editing_modules_keep_tool_specs_and_can_be_disabled() {
    use tiechui_plugin_api::{Module, ModuleConfig, Registry};
    let modules: [&dyn Module; 2] = [
        &tiechui_module_exec::ExecModule { background: true },
        &tiechui_module_editing::EditingModule,
    ];
    let mut config = ModuleConfig::default();
    let registry = Registry::from_modules(&modules, &config).unwrap();
    let names: Vec<_> = registry
        .tools()
        .specs()
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    assert_eq!(
        names,
        [
            "bash",
            "edit",
            "find",
            "grep",
            "read",
            "shell_list",
            "shell_output",
            "shell_stop",
            "write"
        ]
    );
    config.modules.insert("exec".into(), false);
    let registry = Registry::from_modules(&modules, &config).unwrap();
    assert!(registry.tools().get("bash").is_none());
    assert!(registry.tools().get("edit").is_some());
    config.modules.insert("editing".into(), false);
    assert!(
        Registry::from_modules(&modules, &config)
            .unwrap()
            .tools()
            .specs()
            .is_empty()
    );
}
