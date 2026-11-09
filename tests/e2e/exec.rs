#![allow(clippy::unwrap_used)]

mod support;

use std::path::{Path, PathBuf};
use std::process::Output;
#[cfg(unix)]
use std::process::Stdio;
use std::time::Duration;

use tempfile::TempDir;
use tiechui_protocol::{Event, EventKind, Item, KnownItem, TurnStatus};
use tiechui_providers::testing::ScriptedServer;
use tiechui_session::SessionStore;
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

struct Fixture {
    root: TempDir,
    home: PathBuf,
    cwd: PathBuf,
    server: ScriptedServer,
}

#[tokio::test]
async fn search_tools_run_in_headless_exec() {
    let script = tempfile::tempdir().unwrap();
    std::fs::write(
        script.path().join("search.sse"),
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[
                {"index":0,"id":"grep-call","type":"function","function":{"name":"grep","arguments":r#"{"pattern":"needle","path":"."}"#}},
                {"index":1,"id":"find-call","type":"function","function":{"name":"find","arguments":r#"{"pattern":"*.txt","path":"."}"#}}
            ]},"finish_reason":"tool_calls"}]}),
        ),
    )
    .unwrap();
    std::fs::write(
        script.path().join("answer.sse"),
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"found\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
    )
    .unwrap();
    let path = script.path().join("script.json");
    std::fs::write(
        &path,
        r#"[{"path":"/v1/chat/completions","status":200,"body_file":"search.sse"},
            {"path":"/v1/chat/completions","status":200,"body_file":"answer.sse"}]"#,
    )
    .unwrap();
    let fixture = Fixture::new("openai-completions", path.to_str().unwrap()).await;
    std::fs::write(fixture.cwd.join("match.txt"), "needle\n").unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        fixture
            .command("Search")
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .args(["--json", "--mode", "yolo"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.status.success(), "{output:?}");
    let messages = fixture.store().build_context();
    for id in ["grep-call", "find-call"] {
        let result = messages
            .iter()
            .flat_map(|message| &message.content)
            .find_map(|content| match content {
                tiechui_model::Content::ToolResult(result) if result.call_id == id => Some(result),
                _ => None,
            })
            .unwrap();
        assert!(!result.is_error, "{result:?}");
        assert!(serde_json::to_string(result).unwrap().contains("match.txt"));
    }
    fixture.server.verify().unwrap();
}

#[tokio::test]
async fn text_answer_ends_before_usage_without_adding_a_blank_line() {
    for trailing_newline in [false, true] {
        let script = tempfile::tempdir().unwrap();
        let answer = if trailing_newline {
            "tiechui ok\n"
        } else {
            "tiechui ok"
        };
        std::fs::write(
            script.path().join("answer.sse"),
            format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                serde_json::json!({"choices":[{"index":0,"delta":{"content":answer},"finish_reason":null}]}),
                serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":2}}),
            ),
        )
        .unwrap();
        let path = script.path().join("script.json");
        std::fs::write(
            &path,
            r#"[{"path":"/v1/chat/completions","status":200,"body_file":"answer.sse"}]"#,
        )
        .unwrap();
        let fixture = Fixture::new("openai-completions", path.to_str().unwrap()).await;
        // Both streams share one descriptor, preserving the order seen in a terminal.
        let combined = tempfile::tempfile().unwrap();
        let status = fixture
            .command("hello")
            .stdout(combined.try_clone().unwrap())
            .stderr(combined.try_clone().unwrap())
            .status()
            .await
            .unwrap();
        use std::io::{Read, Seek};
        let mut combined = combined;
        combined.rewind().unwrap();
        let mut text = String::new();
        combined.read_to_string(&mut text).unwrap();
        assert!(status.success(), "{text}");
        assert!(text.contains("tiechui ok\nusage:"), "{text}");
        assert!(!text.contains("\n\n"), "{text}");
        fixture.server.verify().unwrap();
    }
}

#[tokio::test]
async fn retry_config_and_events_reach_exec_text_and_json() {
    for json in [false, true] {
        let fixture = Fixture::new("anthropic-messages", "retry.json").await;
        let path = fixture.home.join("config.toml");
        let config = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            path,
            format!("{config}\n[retry]\nmax_retries=1\nbase_delay_ms=1\nmax_delay_ms=10\n"),
        )
        .unwrap();
        let output = fixture
            .run("retry", if json { &["--json"] } else { &[] })
            .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(stderr.contains("retry: 1/1 in 1ms: temporarily unavailable"));
        assert!(stderr.contains("retry: 1/1 starting: temporarily unavailable"));
        if json {
            let events: Vec<Event> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event.event, EventKind::RequestRetry { .. }))
                    .count(),
                2
            );
        }
        fixture.server.verify().unwrap();
    }
}

impl Fixture {
    async fn new(wire: &str, script: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cwd = root.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let script = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tiechui-providers/tests/fixtures/e2e")
            .join(script);
        let server = ScriptedServer::from_file(&script, CancellationToken::new())
            .await
            .unwrap();
        std::fs::write(home.join("config.toml"), "model = \"fixture/model\"\n").unwrap();
        std::fs::write(
            home.join("providers.toml"),
            format!(
                r#"
[fixture]
wire = "{wire}"
base_url = "{}"
api_key_env = "TC_E2E_FAKE_KEY"
models = [{{ id = "model", context_window = 200000, max_output = 4096 }}]
"#,
                server.uri()
            ),
        )
        .unwrap();
        Self {
            root,
            home,
            cwd,
            server,
        }
    }

    fn command(&self, prompt: &str) -> Command {
        let mut command = support::command();
        command
            .env("TIECHUI_HOME", &self.home)
            .env("HOME", self.root.path())
            .env("TC_E2E_FAKE_KEY", "not-a-real-key")
            .arg("exec")
            .arg(prompt)
            .arg("--cwd")
            .arg(&self.cwd)
            .kill_on_drop(true);
        command
    }

    async fn run(&self, prompt: &str, args: &[&str]) -> Output {
        tokio::time::timeout(
            Duration::from_secs(15),
            self.command(prompt).args(args).output(),
        )
        .await
        .unwrap()
        .unwrap()
    }

    fn store(&self) -> SessionStore {
        let sessions =
            tiechui_session::list(&self.home, &self.cwd, &CancellationToken::new()).unwrap();
        assert_eq!(sessions.sessions.len(), 1);
        let store =
            SessionStore::open_read_only(&sessions.sessions[0].path, &CancellationToken::new())
                .unwrap();
        assert!(store.diagnostics().is_empty());
        store
    }

    fn context_files<'a>(&self, audit: &'a serde_json::Value) -> Vec<&'a serde_json::Value> {
        // Repository-local temp directories can inherit the workspace's AGENTS.md.
        audit["files"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|file| {
                file["path"]
                    .as_str()
                    .is_some_and(|path| Path::new(path).starts_with(self.root.path()))
            })
            .collect()
    }
}

fn events(output: &Output) -> Vec<Event> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn denied_bash_is_a_tool_error_and_the_turn_continues_with_audit() {
    for hard_deny in [false, true] {
        let fixture = Fixture::new("anthropic-messages", "permissions.json").await;
        std::fs::write(fixture.cwd.join("protected.txt"), "keep me").unwrap();
        let file = fixture.home.join("config.toml");
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str("\n[[permissions.rules]]\nsurface='*'\npattern='*'\naction='allow'\n[[permissions.rules]]\nsurface='bash'\npattern='rm *'\n");
        text.push_str(if hard_deny {
            "action='deny'\nreason='Keep the fixture'\n"
        } else {
            "action='ask'\n"
        });
        std::fs::write(file, text).unwrap();
        let output = fixture
            .run(
                "Permission fixture",
                if hard_deny {
                    &["--json", "--approval", "yolo"]
                } else {
                    &["--json"]
                },
            )
            .await;
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert_eq!(
            std::fs::read_to_string(fixture.cwd.join("protected.txt")).unwrap(),
            "keep me"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.cwd.join("continued")).unwrap(),
            "safe"
        );
        let seen = events(&output);
        let denial = seen
            .iter()
            .find_map(|event| match &event.event {
                EventKind::ItemCompleted {
                    item:
                        Item::Known(KnownItem::ToolResult {
                            output,
                            is_error: true,
                            ..
                        }),
                    ..
                } => Some(output.to_string()),
                _ => None,
            })
            .unwrap();
        assert!(
            denial.contains(if hard_deny {
                "Denied by policy"
            } else {
                "no interactive UI is available"
            }),
            "{denial}"
        );
        assert!(seen.iter().any(|event| matches!(&event.event, EventKind::TurnCompleted { turn } if turn.status == TurnStatus::Completed)));
        assert!(seen.iter().any(|event| matches!(&event.event, EventKind::ItemCompleted { item: Item::Known(KnownItem::AssistantMessage { text, .. }), .. } if text == "Continued after denied command.")));
        let store = fixture.store();
        assert!(store.entries().iter().any(|entry| matches!(&entry.kind, tiechui_session::EntryKind::Custom {kind,data} if kind == "permissions.decision" && data["outcome"] == "denied")));
        assert!(store.entries().iter().any(|entry| matches!(&entry.kind, tiechui_session::EntryKind::Custom {kind,data} if kind == "permissions.decision" && data["outcome"] == "allowed")));
        fixture.server.verify().unwrap();
    }
}

async fn edit_and_resume(wire: &str, script: &str) {
    let fixture = Fixture::new(wire, script).await;
    std::fs::write(fixture.cwd.join("hello.txt"), "before\n").unwrap();
    let first = fixture
        .run(
            "Change hello.txt from before to after using read and edit.",
            &["--json", "--mode", "auto"],
        )
        .await;
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    assert_eq!(
        std::fs::read_to_string(fixture.cwd.join("hello.txt")).unwrap(),
        "after\n"
    );
    let first_events = events(&first);
    assert!(!first_events.iter().any(|event| matches!(
        &event.event,
        EventKind::ItemCompleted {
            item: Item::Known(KnownItem::ToolResult { is_error: true, .. }),
            ..
        }
    )));
    assert!(first_events.iter().any(|event| matches!(&event.event, EventKind::ItemCompleted { item: Item::Known(KnownItem::ToolCall { name, .. }), .. } if name == "edit")));
    let id = first_events[0].thread_id.0.clone();
    let previous = fixture.store().entries().len();
    let second = fixture.run("What changed?", &["--resume", &id]).await;
    assert_eq!(second.status.code(), Some(0), "{second:?}");
    assert_eq!(
        String::from_utf8(second.stdout).unwrap(),
        "Changed before to after.\n"
    );
    assert!(
        String::from_utf8(second.stderr)
            .unwrap()
            .contains("cache_read=11")
    );
    let store = fixture.store();
    assert_eq!(store.header().id, id);
    assert!(store.entries().len() > previous);
    assert_eq!(store.build_context().len(), 8);
    fixture.server.verify().unwrap();
}

#[tokio::test]
async fn anthropic_edits_and_resumes_through_real_process() {
    edit_and_resume("anthropic-messages", "anthropic.json").await;
}

#[tokio::test]
async fn completions_edits_and_resumes_through_real_process() {
    edit_and_resume("openai-completions", "completions.json").await;
}

#[tokio::test]
async fn resume_rediscovers_context_and_audits_only_changes_to_paths_and_byte_counts() {
    let fixture = Fixture::new("anthropic-messages", "context.json").await;
    #[cfg(unix)]
    let fixture = {
        let alias = fixture.root.path().join("alias");
        std::os::unix::fs::symlink(fixture.root.path(), &alias).unwrap();
        Fixture {
            home: alias.join("home"),
            cwd: alias.join("work"),
            ..fixture
        }
    };
    std::fs::write(fixture.home.join("AGENTS.md"), "GLOBAL_CONTEXT_MARKER").unwrap();
    std::fs::write(
        fixture.cwd.join("AGENTS.md"),
        "TRUSTED_PROJECT_CONTEXT_MARKER",
    )
    .unwrap();
    let trust = support::command()
        .env("TIECHUI_HOME", &fixture.home)
        .env("HOME", fixture.root.path())
        .args(["trust", "allow", "--cwd"])
        .arg(&fixture.cwd)
        .output()
        .await
        .unwrap();
    assert!(trust.status.success(), "{trust:?}");
    let first = fixture.run("Hello", &[]).await;
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    let id = fixture.store().header().id.clone();
    std::fs::remove_file(fixture.cwd.join("AGENTS.md")).unwrap();
    std::fs::write(
        fixture.cwd.join("AGENTS.override.md"),
        "replacement instructions",
    )
    .unwrap();
    let resumed = fixture.run("Continue", &["--resume", &id]).await;
    assert_eq!(resumed.status.code(), Some(0), "{resumed:?}");
    let store = fixture.store();
    let audits: Vec<_> = store
        .entries()
        .iter()
        .filter_map(|entry| match &entry.kind {
            tiechui_session::EntryKind::Custom { kind, data } if kind == "context_files" => {
                Some(data)
            }
            _ => None,
        })
        .collect();
    assert_eq!(audits.len(), 2);
    assert_eq!(fixture.context_files(audits[0]).len(), 2);
    assert_eq!(
        fixture.context_files(audits[0])[1]["bytes"],
        "TRUSTED_PROJECT_CONTEXT_MARKER".len()
    );
    assert_eq!(
        fixture.context_files(audits[1])[1]["path"],
        fixture.cwd.join("AGENTS.override.md").to_str().unwrap()
    );
    assert!(!serde_json::to_string(&audits).unwrap().contains("content"));
    drop(store);
    let replacement = "updated instructions with different byte count";
    std::fs::write(fixture.cwd.join("AGENTS.override.md"), replacement).unwrap();
    let resized = fixture.run("Continue", &["--resume", &id]).await;
    assert_eq!(resized.status.code(), Some(0), "{resized:?}");
    std::fs::write(
        fixture.cwd.join("AGENTS.override.md"),
        replacement.replace("updated", "changed"),
    )
    .unwrap();
    for _ in 0..2 {
        let resumed = fixture.run("Continue", &["--resume", &id]).await;
        assert_eq!(resumed.status.code(), Some(0), "{resumed:?}");
        let store = fixture.store();
        let audits: Vec<_> = store
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                tiechui_session::EntryKind::Custom { kind, data } if kind == "context_files" => {
                    Some(data)
                }
                _ => None,
            })
            .collect();
        assert_eq!(audits.len(), 3);
        assert_eq!(
            fixture.context_files(audits[2])[1]["bytes"],
            replacement.len()
        );
        assert!(audits.iter().all(
            |audit| audit["files"].as_array().unwrap().iter().all(|file| {
                file.as_object().unwrap().len() == 2
                    && file.get("path").is_some()
                    && file.get("bytes").is_some()
            })
        ));
    }
    fixture.server.verify().unwrap();
}

#[tokio::test]
async fn untrusted_project_and_ancestor_context_reach_request() {
    let fixture = Fixture::new("anthropic-messages", "context-untrusted.json").await;
    std::fs::write(fixture.home.join("CLAUDE.md"), "GLOBAL_CONTEXT_MARKER").unwrap();
    std::fs::write(
        fixture.root.path().join("AGENTS.md"),
        "UNTRUSTED_ANCESTOR_CONTEXT_MARKER",
    )
    .unwrap();
    std::fs::write(
        fixture.cwd.join("AGENTS.md"),
        "UNTRUSTED_PROJECT_CONTEXT_MARKER",
    )
    .unwrap();
    std::fs::create_dir_all(fixture.cwd.join(".agents")).unwrap();
    std::fs::write(
        fixture.cwd.join(".agents/config.toml"),
        "model = 'ignored/model'\n",
    )
    .unwrap();
    let output = fixture.run("Hello", &[]).await;
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("trust: skipping")
    );
    let store = fixture.store();
    let data = store
        .entries()
        .iter()
        .find_map(|entry| match &entry.kind {
            tiechui_session::EntryKind::Custom { kind, data } if kind == "context_files" => {
                Some(data)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(fixture.context_files(data).len(), 3);
    assert_eq!(
        fixture.context_files(data)[2]["bytes"],
        "UNTRUSTED_PROJECT_CONTEXT_MARKER".len()
    );
    assert!(
        !serde_json::to_string(data)
            .unwrap()
            .contains("UNTRUSTED_PROJECT_CONTEXT_MARKER")
    );
    fixture.server.verify().unwrap();
}

#[tokio::test]
async fn provider_failure_exits_one_and_keeps_json_valid() {
    let fixture = Fixture::new("anthropic-messages", "failure.json").await;
    let output = fixture.run("Fail", &["--json"]).await;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(events(&output).iter().any(|event| matches!(&event.event, EventKind::TurnCompleted { turn } if turn.status == TurnStatus::Failed)));
    fixture.server.verify().unwrap();
}

#[tokio::test]
async fn usage_and_config_errors_exit_two() {
    let fixture = Fixture::new("anthropic-messages", "failure.json").await;
    for args in [
        vec!["--model", "missing/model"],
        vec!["--resume", "missing"],
        vec!["--model", "bad"],
        vec!["--unknown"],
    ] {
        let output = fixture.run("Hello", &args).await;
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty());
    }
    std::fs::write(fixture.home.join("config.toml"), "unexpected = true").unwrap();
    assert_eq!(fixture.run("Hello", &[]).await.status.code(), Some(2));
}

#[tokio::test]
async fn locked_session_is_a_runtime_failure() {
    let fixture = Fixture::new("anthropic-messages", "failure.json").await;
    let store =
        SessionStore::create(&fixture.home, &fixture.cwd, &CancellationToken::new()).unwrap();
    let output = fixture
        .run("Hello", &["--resume", &store.header().id])
        .await;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
}

#[tokio::test]
async fn model_override_is_saved_and_restored_on_resume() {
    let fixture = Fixture::new("anthropic-messages", "model.json").await;
    let providers_path = fixture.home.join("providers.toml");
    let providers = std::fs::read_to_string(&providers_path).unwrap();
    std::fs::write(
        &providers_path,
        providers.replace(
            "models = [",
            "models = [{ id = 'other', context_window = 200000, max_output = 4096 },",
        ),
    )
    .unwrap();
    let first = fixture.run("Hello", &["--model", "fixture/other"]).await;
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    let id = fixture.store().header().id.clone();
    let second = fixture.run("Continue", &["--resume", &id]).await;
    assert_eq!(second.status.code(), Some(0), "{second:?}");
    fixture.server.verify().unwrap();
}

#[cfg(unix)]
fn sigint(pid: u32) {
    // SAFETY: pid belongs to our live child; SIGINT has no pointer arguments.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGINT) }, 0);
}

#[cfg(unix)]
#[tokio::test]
async fn sigint_while_waiting_for_provider_leaves_replayable_session() {
    let fixture = Fixture::new("anthropic-messages", "delayed.json").await;
    let child = fixture
        .command("Wait")
        .arg("--json")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(10),
        fixture
            .server
            .wait_for_requests(1, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap();
    sigint(child.id().unwrap());
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    assert!(events(&output).iter().any(|event| matches!(&event.event, EventKind::TurnCompleted { turn } if turn.status == TurnStatus::Interrupted)));
    let id = fixture.store().header().id.clone();
    let resumed = fixture.run("Continue", &["--resume", &id]).await;
    assert_eq!(resumed.status.code(), Some(0), "{resumed:?}");
    fixture.server.verify().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn second_sigint_force_quits_during_tool_cleanup() {
    let fixture = Fixture::new("anthropic-messages", "force.json").await;
    let mut child = fixture
        .command("Run a long bash command")
        .args(["--mode", "yolo"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fixture.cwd.join("started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = child.id().unwrap();
    sigint(pid);
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if line.starts_with("interrupting;") {
                return;
            }
        }
        panic!("child exited before acknowledging first SIGINT");
    })
    .await
    .unwrap();
    // Allow the first interrupt to reach bash's process-group cleanup.
    tokio::time::sleep(Duration::from_millis(30)).await;
    sigint(pid);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap()
            .code(),
        Some(130)
    );
    while let Some(line) = lines.next_line().await.unwrap() {
        assert!(
            !line.starts_with("turn:"),
            "second SIGINT must bypass settlement"
        );
    }
    fixture.server.verify().unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn sigint_during_bash_settles_session_and_can_resume() {
    let fixture = Fixture::new("anthropic-messages", "interrupt.json").await;
    let mut child = fixture
        .command("Run a long bash command")
        .args(["--json", "--mode", "yolo"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut seen = vec![];
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            let event: Event = serde_json::from_str(&line).unwrap();
            let running = matches!(
                &event.event,
                EventKind::ItemStarted {
                    item: Item::Known(KnownItem::ToolResult { .. }),
                    ..
                }
            );
            seen.push(event);
            if running {
                break;
            }
        }
    })
    .await
    .unwrap();
    // The tool-result start is emitted before the bash child starts. Wait for its marker.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fixture.cwd.join("started").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = child.id().unwrap();
    sigint(pid);
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            seen.push(serde_json::from_str(&line).unwrap());
        }
    })
    .await
    .unwrap();
    assert_eq!(child.wait().await.unwrap().code(), Some(130));
    assert!(seen.iter().any(|event| matches!(&event.event, EventKind::TurnCompleted { turn } if turn.status == TurnStatus::Interrupted)));
    let store = fixture.store();
    let id = store.header().id.clone();
    let messages = store.build_context();
    assert!(messages.iter().any(|message| message.content.iter().any(
        |content| matches!(content, tiechui_model::Content::ToolResult(result) if result.is_error)
    )));
    drop(store);
    let resumed = fixture.run("Continue", &["--resume", &id]).await;
    assert_eq!(resumed.status.code(), Some(0), "{resumed:?}");
    fixture.server.verify().unwrap();
}
