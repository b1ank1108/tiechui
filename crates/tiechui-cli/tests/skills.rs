#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn command(user: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tc"))
        .env_clear()
        .env("TIECHUI_HOME", user.join(".tiechui"))
        .env("HOME", user)
        .args(args)
        .arg("--cwd")
        .arg(cwd)
        .output()
        .unwrap()
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn skills_list_discovers_agents_and_claude_by_default() {
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/skills-cli-tests");
    fs::create_dir_all(&target).unwrap();
    let temp = tempfile::tempdir_in(target).unwrap();
    let user = temp.path().join("user");
    let cwd = temp.path().join("work");
    let text = "---\nname: shared\ndescription: Fixture skill\n---\nBODY\n";
    write(&user.join(".agents/skills/shared/SKILL.md"), text);
    write(&cwd.join(".agents/skills/shared/SKILL.md"), text);
    for (root, name) in [
        (user.join(".claude/skills"), "claude-user"),
        (cwd.join(".claude/skills"), "claude-project"),
    ] {
        write(
            &root.join(name).join("SKILL.md"),
            &format!(
                "---\nname: {name}\ndescription: Default discovery\ndisable-model-invocation: true\n---\n"
            ),
        );
    }
    let list = || {
        let output = command(&user, &cwd, &["skills", "list", "--json"]);
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let untrusted = list();
    assert_eq!(untrusted["skills"].as_array().unwrap().len(), 2);
    assert_eq!(untrusted["skills"][0]["source"], "user");
    assert_eq!(
        untrusted["diagnostics"][0]["message"],
        "skipping untrusted project skills"
    );
    assert!(command(&user, &cwd, &["trust", "allow"]).status.success());
    let trusted = list();
    assert_eq!(trusted["skills"].as_array().unwrap().len(), 3);
    assert_eq!(trusted["skills"][0]["source"], "user");
    assert_eq!(
        trusted["skills"][0]["root"],
        serde_json::json!(user.join(".agents/skills"))
    );
    assert_eq!(trusted["skills"][1]["name"], "claude-user");
    assert_eq!(trusted["skills"][2]["name"], "claude-project");
    assert_eq!(trusted["skills"][1]["disable_model_invocation"], true);
    assert_eq!(trusted["diagnostics"][0]["type"], "collision");
    assert_eq!(
        trusted["diagnostics"][0]["path"],
        serde_json::json!(
            cwd.canonicalize()
                .unwrap()
                .join(".agents/skills/shared/SKILL.md")
        )
    );
    assert_eq!(
        trusted["diagnostics"][0]["winner_path"],
        trusted["skills"][0]["path"]
    );
    let output = command(&user, &cwd, &["skills", "list"]);
    assert!(output.status.success(), "{output:?}");
    let output = String::from_utf8(output.stdout).unwrap();
    for expected in [
        "shared",
        "root:",
        "collision:",
        "claude-user",
        "disable-model-invocation: true",
    ] {
        assert!(output.contains(expected), "{output}");
    }
    assert!(!output.contains("BODY"));
}

#[tokio::test]
async fn exec_injects_catalog_rediscovers_on_resume_and_respects_trust_and_activation() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method},
    };
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/skills-cli-tests");
    fs::create_dir_all(&target).unwrap();
    let temp = tempfile::tempdir_in(target).unwrap();
    let user = temp.path().join("user");
    let cwd = temp.path().join("work");
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("x-api-key", "fixture-only"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            include_str!("../../tiechui-providers/tests/fixtures/e2e/anthropic-resume.sse"),
            "text/event-stream",
        ))
        .mount(&server)
        .await;
    write(
        &user.join(".tiechui/config.toml"),
        "model = 'fixture/model'\n",
    );
    write(
        &user.join(".tiechui/providers.toml"),
        &format!(
            "[fixture]\nwire = 'anthropic-messages'\nbase_url = '{}'\napi_key = 'fixture-only'\nmodels = [{{id = 'model', context_window = 200000, max_output = 4096}}]\n",
            server.uri()
        ),
    );
    let user_skill = user.join(".agents/skills/user/SKILL.md");
    let project_skill = cwd.join(".claude/skills/project/SKILL.md");
    write(
        &user_skill,
        "---\nname: user-skill\ndescription: USER_SKILL_DESCRIPTION\n---\nBODY_NOT_IN_PROMPT\n",
    );
    write(
        &project_skill,
        "---\nname: project-skill\ndescription: PROJECT_SKILL_DESCRIPTION\n---\nBODY_NOT_IN_PROMPT\n",
    );
    write(
        &user.join(".agents/skills/disabled/SKILL.md"),
        "---\nname: disabled-skill\ndescription: DISABLED_SKILL_DESCRIPTION\ndisable-model-invocation: true\n---\n",
    );
    let run = |resume: Option<String>| {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tc"));
        command
            .env_clear()
            .env("HOME", &user)
            .env("TIECHUI_HOME", user.join(".tiechui"))
            .args(["exec", "Hello", "--cwd"])
            .arg(&cwd);
        if let Some(id) = resume {
            command.args(["--resume", &id]);
        }
        async move {
            let output = tokio::time::timeout(std::time::Duration::from_secs(15), command.output())
                .await
                .unwrap()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
    };
    run(None).await;
    assert!(command(&user, &cwd, &["trust", "allow"]).status.success());
    let sessions = tiechui_session::list(
        &user.join(".tiechui"),
        &cwd,
        &tiechui_plugin_api::CancellationToken::new(),
    )
    .unwrap();
    let id = sessions.sessions[0].header.id.clone();
    write(
        &user_skill,
        "---\nname: user-skill\ndescription: UPDATED_SKILL_DESCRIPTION\n---\nBODY_NOT_IN_PROMPT\n",
    );
    run(Some(id)).await;
    write(
        &user.join(".tiechui/config.toml"),
        &format!(
            "{}\n[modules]\nskills = false\n",
            fs::read_to_string(user.join(".tiechui/config.toml")).unwrap()
        ),
    );
    run(None).await;
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    let systems: Vec<_> = requests
        .iter()
        .map(|request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            body["system"].to_string()
        })
        .collect();
    assert!(systems[0].contains("USER_SKILL_DESCRIPTION"));
    assert!(!systems[0].contains("PROJECT_SKILL_DESCRIPTION"));
    assert!(systems[1].contains("UPDATED_SKILL_DESCRIPTION"));
    assert!(systems[1].contains("PROJECT_SKILL_DESCRIPTION"));
    assert!(
        systems[1].find("<name>user-skill</name>").unwrap()
            < systems[1].find("<name>project-skill</name>").unwrap()
    );
    assert!(!systems[1].contains("USER_SKILL_DESCRIPTION"));
    assert!(!systems[2].contains("<available_skills>"));
    for system in systems {
        assert!(!system.contains("BODY_NOT_IN_PROMPT"));
        assert!(!system.contains("DISABLED_SKILL_DESCRIPTION"));
    }
}
