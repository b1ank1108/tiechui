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

mod dual_subagents {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};
    use tiechui_protocol::{AgentDispatch, AgentState};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    const BASE_CODING_PROMPT: &str = "You are Tiechui, a coding assistant. Use the available tools to complete the user's task. Read existing files before editing them. Keep answers concise.";
    const VERIFY_SECRET: &str = "secret-tc-verify-key-999";

    enum GatewayReply {
        Text(String),
        DelayedText(String, Duration),
        Tools(Vec<(String, String, Value)>),
    }

    struct ConcurrencyTracker {
        in_flight: AtomicUsize,
        peak: AtomicUsize,
    }

    impl ConcurrencyTracker {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                in_flight: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            })
        }

        fn enter(&self) {
            let current = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(current, Ordering::SeqCst);
        }

        fn leave(&self) {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }
    }

    struct MockGateway {
        addr: SocketAddr,
        requests: Arc<Mutex<Vec<Value>>>,
        shutdown: CancellationToken,
    }

    impl Drop for MockGateway {
        fn drop(&mut self) {
            self.shutdown.cancel();
        }
    }

    fn system_text(body: &Value) -> &str {
        body["messages"]
            .as_array()
            .and_then(|messages| messages.first())
            .filter(|msg| matches!(msg["role"].as_str(), Some("system" | "developer")))
            .and_then(|msg| msg["content"].as_str())
            .unwrap_or_default()
    }

    fn is_child_request(body: &Value) -> bool {
        !system_text(body).contains("<subagent_models>")
    }

    fn has_tool_result(body: &Value) -> bool {
        body["messages"]
            .as_array()
            .and_then(|messages| messages.last())
            .is_some_and(|msg| msg["role"] == "tool")
    }

    fn last_user_text(body: &Value) -> String {
        body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .rev()
            .find(|msg| msg["role"] == "user")
            .map(|msg| match &msg["content"] {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(""),
                _ => String::new(),
            })
            .unwrap_or_default()
    }

    fn encode_sse(reply: GatewayReply) -> (String, Duration) {
        match reply {
            GatewayReply::Text(text) => (sse_text(&text), Duration::ZERO),
            GatewayReply::DelayedText(text, delay) => (sse_text(&text), delay),
            GatewayReply::Tools(calls) => {
                let wire_calls: Vec<_> = calls
                    .into_iter()
                    .enumerate()
                    .map(|(index, (id, name, args))| {
                        json!({
                            "index": index,
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": args.to_string(),
                            }
                        })
                    })
                    .collect();
                let sse = format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    json!({"choices":[{"index":0,"delta":{"tool_calls":wire_calls},"finish_reason":"tool_calls"}]}),
                    json!({"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":8}}),
                );
                (sse, Duration::ZERO)
            }
        }
    }

    fn sse_text(text: &str) -> String {
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":4}}),
        )
    }

    async fn read_post_json(stream: &mut TcpStream) -> Value {
        let mut reader = BufReader::new(stream);
        let mut content_length = 0usize;
        let mut request_line = String::new();
        reader.read_line(&mut request_line).await.unwrap();
        assert!(
            request_line.starts_with("POST /v1/chat/completions "),
            "unexpected request line: {request_line}"
        );
        loop {
            let mut line = String::new();
            let read = reader.read_line(&mut line).await.unwrap();
            assert!(read > 0, "unexpected EOF while reading headers");
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some((name, value)) = trimmed.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    impl MockGateway {
        async fn start(
            tracker: Arc<ConcurrencyTracker>,
            handler: impl Fn(&Value) -> GatewayReply + Send + Sync + 'static,
        ) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let shutdown = CancellationToken::new();
            let handler = Arc::new(handler);
            let recorded = requests.clone();
            let stop = shutdown.clone();
            tokio::spawn(async move {
                loop {
                    let accepted = tokio::select! {
                        () = stop.cancelled() => break,
                        accepted = listener.accept() => accepted,
                    };
                    let Ok((mut stream, _)) = accepted else {
                        break;
                    };
                    let handler = handler.clone();
                    let recorded = recorded.clone();
                    let tracker = tracker.clone();
                    tokio::spawn(async move {
                        let body = read_post_json(&mut stream).await;
                        let child = is_child_request(&body);
                        recorded.lock().unwrap().push(body.clone());
                        let (sse, delay) = encode_sse(handler(&body));
                        if child {
                            tracker.enter();
                        }
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        if child {
                            tracker.leave();
                        }
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            sse.len(),
                            sse
                        );
                        let _ = stream.write_all(response.as_bytes()).await;
                        let _ = stream.shutdown().await;
                    });
                }
            });
            Self {
                addr,
                requests,
                shutdown,
            }
        }

        fn uri(&self) -> String {
            format!("http://{}", self.addr)
        }

        fn requests(&self) -> Vec<Value> {
            self.requests.lock().unwrap().clone()
        }

        fn child_requests(&self) -> Vec<Value> {
            self.requests()
                .into_iter()
                .filter(is_child_request)
                .collect()
        }
    }

    struct DualFixture {
        root: TempDir,
        home: PathBuf,
        cwd: PathBuf,
        alpha: MockGateway,
        beta: MockGateway,
        tracker: Arc<ConcurrencyTracker>,
    }

    impl DualFixture {
        async fn new(
            alpha_handler: impl Fn(&Value) -> GatewayReply + Send + Sync + 'static,
            beta_handler: impl Fn(&Value) -> GatewayReply + Send + Sync + 'static,
        ) -> Self {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().join("home");
            let cwd = root.path().join("work");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&cwd).unwrap();
            let tracker = ConcurrencyTracker::new();
            let alpha = MockGateway::start(tracker.clone(), alpha_handler).await;
            let beta = MockGateway::start(tracker.clone(), beta_handler).await;
            std::fs::write(
                home.join("config.toml"),
                "model = \"alpha/fast\"\nthinking = \"high\"\n\n[[permissions.rules]]\nsurface = \"subagent\"\npattern = \"*\"\naction = \"allow\"\n",
            )
            .unwrap();
            std::fs::write(
                home.join("providers.toml"),
                format!(
                    "[alpha]\nwire = \"openai-completions\"\nbase_url = \"{}\"\napi_key_env = \"TC_VERIFY_KEY\"\nmodels = [{{ id = \"fast\", context_window = 200000, max_output = 4096, reasoning = false }}]\n\n[beta]\nwire = \"openai-completions\"\nbase_url = \"{}\"\napi_key_env = \"TC_VERIFY_KEY\"\nmodels = [{{ id = \"judge\", context_window = 200000, max_output = 4096, reasoning = true }}]\n",
                    alpha.uri(),
                    beta.uri(),
                ),
            )
            .unwrap();
            Self {
                root,
                home,
                cwd,
                alpha,
                beta,
                tracker,
            }
        }

        async fn exec_json(&self, prompt: &str) -> Output {
            self.exec_args(prompt, &["--json"]).await
        }

        async fn exec_args(&self, prompt: &str, extra: &[&str]) -> Output {
            let mut command = support::command();
            command
                .env("TIECHUI_HOME", &self.home)
                .env("HOME", self.root.path())
                .env("TC_VERIFY_KEY", VERIFY_SECRET)
                .args(["exec", prompt, "--cwd"])
                .arg(&self.cwd)
                .args(extra)
                .kill_on_drop(true);
            tokio::time::timeout(Duration::from_secs(20), command.output())
                .await
                .unwrap()
                .unwrap()
        }

        fn latest_store(&self) -> SessionStore {
            let sessions =
                tiechui_session::list(&self.home, &self.cwd, &CancellationToken::new()).unwrap();
            assert!(!sessions.sessions.is_empty());
            let store =
                SessionStore::open_read_only(&sessions.sessions[0].path, &CancellationToken::new())
                    .unwrap();
            assert!(store.diagnostics().is_empty());
            store
        }

        fn assert_no_secret_or_config_leak(&self) {
            let providers_path = self.home.join("providers.toml").display().to_string();
            for body in self
                .alpha
                .requests()
                .into_iter()
                .chain(self.beta.requests())
            {
                let raw = serde_json::to_string(&body).unwrap();
                assert!(!raw.contains(VERIFY_SECRET), "secret leaked: {raw}");
                assert!(!raw.contains("TC_VERIFY_KEY"), "env var name leaked: {raw}");
                assert!(!raw.contains("providers.toml"), "config name leaked: {raw}");
                assert!(!raw.contains(&providers_path), "config path leaked: {raw}");
            }
        }
    }

    fn settled_dispatches(store: &SessionStore) -> Vec<AgentDispatch> {
        let mut by_id = std::collections::BTreeMap::new();
        for entry in store.entries() {
            if let tiechui_session::EntryKind::Custom { kind, data } = &entry.kind
                && kind == "tiechui.subagents.record"
            {
                let dispatch: AgentDispatch = serde_json::from_value(data.clone()).unwrap();
                by_id.insert(dispatch.id.clone(), dispatch);
            }
        }
        by_id.into_values().collect()
    }

    fn tool_results(seen: &[Event]) -> Vec<(String, bool)> {
        seen.iter()
            .filter_map(|event| match &event.event {
                EventKind::ItemCompleted {
                    item:
                        Item::Known(KnownItem::ToolResult {
                            output, is_error, ..
                        }),
                    ..
                } => Some((
                    output["content"][0]["text"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    *is_error,
                )),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn subagent_default_single_chain_catalog_and_removed_builtin_preflight() {
        let fixture = DualFixture::new(
            |body| {
                if is_child_request(body) {
                    return match last_user_text(body).as_str() {
                        "single-job" => GatewayReply::Text("SINGLE_CHILD_DONE".into()),
                        "chain-step-1" => GatewayReply::Text("STEP1_VALUE".into()),
                        "chain-step-2:STEP1_VALUE" => GatewayReply::Text("CHAIN_FINAL_DONE".into()),
                        other => panic!("unexpected child prompt: {other}"),
                    };
                }
                if has_tool_result(body) {
                    return GatewayReply::Text("PARENT_FINISHED".into());
                }
                match last_user_text(body).as_str() {
                    "run-single" => GatewayReply::Tools(vec![(
                        "call-single".into(),
                        "subagent".into(),
                        json!({"task": "single-job"}),
                    )]),
                    "run-chain" => GatewayReply::Tools(vec![(
                        "call-chain".into(),
                        "subagent".into(),
                        json!({"chain": [
                            {"task": "chain-step-1"},
                            {"task": "chain-step-2:{previous}"}
                        ]}),
                    )]),
                    "run-scout" => GatewayReply::Tools(vec![(
                        "call-scout".into(),
                        "subagent".into(),
                        json!({"agent": "scout", "task": "explore repo"}),
                    )]),
                    other => panic!("unexpected parent prompt: {other}"),
                }
            },
            |body| panic!("unexpected beta request: {body}"),
        )
        .await;

        let single = fixture.exec_json("run-single").await;
        assert_eq!(single.status.code(), Some(0), "{single:?}");
        let single_results = tool_results(&events(&single));
        assert_eq!(
            single_results,
            vec![("SINGLE_CHILD_DONE".to_owned(), false)]
        );
        let single_records = settled_dispatches(&fixture.latest_store());
        assert_eq!(single_records.len(), 1);
        assert_eq!(single_records[0].tasks[0].agent, "default");
        assert_eq!(single_records[0].tasks[0].model, "alpha/fast");

        let chain = fixture.exec_json("run-chain").await;
        assert_eq!(chain.status.code(), Some(0), "{chain:?}");
        let chain_results = tool_results(&events(&chain));
        assert_eq!(chain_results, vec![("CHAIN_FINAL_DONE".to_owned(), false)]);
        let chain_records = settled_dispatches(&fixture.latest_store());
        assert_eq!(chain_records.len(), 1);
        assert!(
            chain_records[0]
                .tasks
                .iter()
                .all(|t| t.agent == "default" && t.model == "alpha/fast")
        );

        let alpha_children = fixture.alpha.child_requests();
        assert_eq!(alpha_children.len(), 3);
        for child_body in &alpha_children {
            let sys = system_text(child_body);
            assert_eq!(sys, BASE_CODING_PROMPT);
            assert!(!sys.contains("<subagent_models>"));
            let has_subagent_tool = child_body["tools"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|tool| tool["function"]["name"] == "subagent");
            assert!(!has_subagent_tool);
        }

        let parent_sys = system_text(&fixture.alpha.requests()[0]).to_owned();
        assert!(parent_sys.contains(BASE_CODING_PROMPT), "{parent_sys}");
        assert!(
            parent_sys.contains("Active session model: alpha/fast"),
            "{parent_sys}"
        );
        assert!(
            parent_sys.contains("- beta/judge (thinking: off, minimal, low, medium, high)"),
            "{parent_sys}"
        );

        let child_count_before_scout = fixture.alpha.child_requests().len();
        let scout = fixture.exec_json("run-scout").await;
        assert_eq!(scout.status.code(), Some(0), "{scout:?}");
        let scout_results = tool_results(&events(&scout));
        assert_eq!(scout_results.len(), 1);
        assert!(scout_results[0].1, "{scout_results:?}");
        assert!(
            scout_results[0].0.contains("Unknown agent: scout"),
            "{}",
            scout_results[0].0
        );
        assert_eq!(
            fixture.alpha.child_requests().len(),
            child_count_before_scout
        );
        assert!(fixture.beta.requests().is_empty());
        fixture.assert_no_secret_or_config_leak();
    }

    #[tokio::test]
    async fn subagent_cross_provider_tasks_override_model_and_clamp_requested_thinking() {
        let fixture = DualFixture::new(
            |body| {
                if is_child_request(body) {
                    return match last_user_text(body).as_str() {
                        "custom-inherit-task" => GatewayReply::Text("ALPHA_CUSTOM_OK".into()),
                        "preset-inherit-task" => GatewayReply::Text("ALPHA_PRESET_OK".into()),
                        other => panic!("unexpected alpha child prompt: {other}"),
                    };
                }
                if has_tool_result(body) {
                    return GatewayReply::Text("CROSS_DONE".into());
                }
                GatewayReply::Tools(vec![(
                    "call-cross".into(),
                    "subagent".into(),
                    json!({"tasks": [
                        {"task": "beta-default-high", "model": "beta/judge"},
                        {"agent": "custom", "task": "custom-inherit-task", "model": "inherit"},
                        {"agent": "judge_preset", "task": "preset-inherit-task", "model": "inherit"},
                        {"task": "beta-explicit-low", "model": "beta/judge", "thinking": "low"}
                    ]}),
                )])
            },
            |body| match last_user_text(body).as_str() {
                "beta-default-high" => GatewayReply::Text("BETA_HIGH_OK".into()),
                "beta-explicit-low" => GatewayReply::Text("BETA_LOW_OK".into()),
                other => panic!("unexpected beta child prompt: {other}"),
            },
        )
        .await;

        let agents_dir = fixture.home.join("agents");
        std::fs::create_dir_all(&agents_dir).unwrap();
        std::fs::write(
            agents_dir.join("custom.md"),
            "---\nname: custom\ndescription: Alpha custom agent\nmodel: alpha/fast\n---\nYou are CustomHelper.",
        )
        .unwrap();
        std::fs::write(
            agents_dir.join("judge_preset.md"),
            "---\nname: judge_preset\ndescription: Beta preset agent\nmodel: beta/judge\nthinking: medium\n---\nYou are JudgePreset.",
        )
        .unwrap();

        let output = fixture.exec_json("dispatch-cross").await;
        assert_eq!(output.status.code(), Some(0), "{output:?}");

        for parent_req in fixture
            .alpha
            .requests()
            .into_iter()
            .filter(|req| !is_child_request(req))
        {
            assert_eq!(parent_req["model"], "fast");
            assert!(parent_req.get("reasoning_effort").is_none());
        }

        let alpha_children = fixture.alpha.child_requests();
        assert_eq!(alpha_children.len(), 2);
        for req in &alpha_children {
            assert_eq!(req["model"], "fast");
            assert!(req.get("reasoning_effort").is_none());
        }
        assert!(
            alpha_children
                .iter()
                .any(|req| last_user_text(req) == "custom-inherit-task"
                    && system_text(req) == "You are CustomHelper.")
        );
        assert!(
            alpha_children
                .iter()
                .any(|req| last_user_text(req) == "preset-inherit-task"
                    && system_text(req) == "You are JudgePreset.")
        );

        let beta_children = fixture.beta.child_requests();
        assert_eq!(beta_children.len(), 2);
        let high_req = beta_children
            .iter()
            .find(|req| last_user_text(req) == "beta-default-high")
            .unwrap();
        assert_eq!(high_req["model"], "judge");
        assert_eq!(high_req["reasoning_effort"], "high");
        assert_eq!(system_text(high_req), BASE_CODING_PROMPT);

        let low_req = beta_children
            .iter()
            .find(|req| last_user_text(req) == "beta-explicit-low")
            .unwrap();
        assert_eq!(low_req["model"], "judge");
        assert_eq!(low_req["reasoning_effort"], "low");
        assert_eq!(system_text(low_req), BASE_CODING_PROMPT);

        let results = tool_results(&events(&output));
        assert_eq!(results.len(), 1);
        assert!(!results[0].1, "{results:?}");
        let summary = &results[0].0;
        for expected in [
            "### [default · beta/judge] Completed",
            "### [custom · alpha/fast] Completed",
            "### [judge_preset · alpha/fast] Completed",
            "BETA_HIGH_OK",
            "ALPHA_CUSTOM_OK",
            "ALPHA_PRESET_OK",
            "BETA_LOW_OK",
        ] {
            assert!(
                summary.contains(expected),
                "missing {expected} in:\n{summary}"
            );
        }

        let dispatches = settled_dispatches(&fixture.latest_store());
        assert_eq!(dispatches.len(), 1);
        let pairs: Vec<_> = dispatches[0]
            .tasks
            .iter()
            .map(|t| (t.agent.as_str(), t.model.as_str(), t.state))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("default", "beta/judge", AgentState::Completed),
                ("custom", "alpha/fast", AgentState::Completed),
                ("judge_preset", "alpha/fast", AgentState::Completed),
                ("default", "beta/judge", AgentState::Completed),
            ]
        );
        fixture.assert_no_secret_or_config_leak();
    }

    #[tokio::test]
    async fn subagent_missing_model_preflight_lists_available_models_without_child_requests() {
        let fixture = DualFixture::new(
            |body| {
                assert!(!is_child_request(body), "unexpected child request: {body}");
                if has_tool_result(body) {
                    return GatewayReply::Text("RECOVERED_AFTER_PREFLIGHT".into());
                }
                GatewayReply::Tools(vec![(
                    "call-missing".into(),
                    "subagent".into(),
                    json!({"task": "do not spawn", "model": "missing/nope"}),
                )])
            },
            |body| panic!("unexpected beta request: {body}"),
        )
        .await;

        let output = fixture.exec_json("try-missing").await;
        assert_eq!(output.status.code(), Some(0), "{output:?}");
        assert!(fixture.alpha.child_requests().is_empty());
        assert!(fixture.beta.requests().is_empty());

        let results = tool_results(&events(&output));
        assert_eq!(results.len(), 1);
        assert!(results[0].1, "{results:?}");
        assert!(results[0].0.contains("missing/nope"), "{}", results[0].0);
        assert!(results[0].0.contains("alpha/fast"), "{}", results[0].0);
        assert!(results[0].0.contains("beta/judge"), "{}", results[0].0);
        fixture.assert_no_secret_or_config_leak();
    }

    #[tokio::test]
    async fn subagent_resume_on_non_reasoning_parent_preserves_requested_thinking() {
        let fixture = DualFixture::new(
            |body| {
                assert!(!is_child_request(body), "unexpected alpha child: {body}");
                if has_tool_result(body) {
                    return GatewayReply::Text("RESUMED_PARENT_DONE".into());
                }
                match last_user_text(body).as_str() {
                    "turn-one" => GatewayReply::Text("TURN_ONE_DONE".into()),
                    "turn-two-resume" => GatewayReply::Tools(vec![(
                        "call-resumed-judge".into(),
                        "subagent".into(),
                        json!({"task": "resumed-judge-task", "model": "beta/judge"}),
                    )]),
                    other => panic!("unexpected parent prompt: {other}"),
                }
            },
            |body| match last_user_text(body).as_str() {
                "resumed-judge-task" => GatewayReply::Text("RESUMED_BETA_HIGH_OK".into()),
                other => panic!("unexpected beta child prompt: {other}"),
            },
        )
        .await;

        let turn_one = fixture.exec_json("turn-one").await;
        assert_eq!(turn_one.status.code(), Some(0), "{turn_one:?}");
        let session_id = events(&turn_one)[0].thread_id.0.clone();

        std::fs::write(
            fixture.home.join("config.toml"),
            "model = \"alpha/fast\"\n\n[[permissions.rules]]\nsurface = \"subagent\"\npattern = \"*\"\naction = \"allow\"\n",
        )
        .unwrap();

        let turn_two = fixture
            .exec_args("turn-two-resume", &["--resume", &session_id, "--json"])
            .await;
        assert_eq!(turn_two.status.code(), Some(0), "{turn_two:?}");
        let results = tool_results(&events(&turn_two));
        assert_eq!(results, vec![("RESUMED_BETA_HIGH_OK".to_owned(), false)]);

        for parent_req in fixture.alpha.requests() {
            assert_eq!(parent_req["model"], "fast");
            assert!(parent_req.get("reasoning_effort").is_none());
        }
        let beta_children = fixture.beta.child_requests();
        assert_eq!(beta_children.len(), 1);
        assert_eq!(beta_children[0]["model"], "judge");
        assert_eq!(beta_children[0]["reasoning_effort"], "high");
        fixture.assert_no_secret_or_config_leak();
    }

    #[tokio::test]
    async fn subagent_ten_same_turn_calls_share_eight_slot_semaphore() {
        let fixture = DualFixture::new(
            |body| {
                if is_child_request(body) {
                    let prompt = last_user_text(body);
                    return GatewayReply::DelayedText(
                        format!("ACK:{prompt}"),
                        Duration::from_millis(150),
                    );
                }
                if has_tool_result(body) {
                    return GatewayReply::Text("ALL_TEN_DONE".into());
                }
                let calls = (0..10)
                    .map(|index| {
                        (
                            format!("call-ten-{index}"),
                            "subagent".to_owned(),
                            json!({"task": format!("slot-job-{index}")}),
                        )
                    })
                    .collect();
                GatewayReply::Tools(calls)
            },
            |body| panic!("unexpected beta request: {body}"),
        )
        .await;

        let output = fixture.exec_json("fanout-ten").await;
        assert_eq!(output.status.code(), Some(0), "{output:?}");

        let results = tool_results(&events(&output));
        assert_eq!(results.len(), 10);
        for (index, (text, is_error)) in results.iter().enumerate() {
            assert!(!is_error, "call {index} failed: {text}");
            assert_eq!(text, &format!("ACK:slot-job-{index}"));
        }

        assert_eq!(fixture.alpha.child_requests().len(), 10);
        let peak = fixture.tracker.peak();
        assert!(
            peak > 1 && peak <= 8,
            "expected 1 < peak <= 8 concurrent in-flight child requests, got {peak}"
        );

        let dispatches = settled_dispatches(&fixture.latest_store());
        assert_eq!(dispatches.len(), 10);
        assert!(dispatches.iter().all(|d| {
            d.tasks.len() == 1
                && d.tasks[0].agent == "default"
                && d.tasks[0].model == "alpha/fast"
                && d.tasks[0].state == AgentState::Completed
        }));
        fixture.assert_no_secret_or_config_leak();
    }
}
