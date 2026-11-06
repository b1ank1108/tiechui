#![allow(clippy::unwrap_used)]
#![cfg(feature = "test-server")]

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tiechui_module_mcp::{DeclineElicitation, McpModule, ServerState, tool_name};
use tiechui_plugin_api::model::{Content, ToolCall};
use tiechui_plugin_api::protocol::{SessionEpoch, ThreadId, TurnId};
use tiechui_plugin_api::serde_json::{Value, json};
use tiechui_plugin_api::tools::ExecutionScope;
use tiechui_plugin_api::{
    CancellationToken, McpOrigin, McpServerConfig, McpServerDefinition, Module, ModuleConfig,
    Registry, ToolContext,
};

fn definition(name: &str, delay: u64) -> McpServerDefinition {
    McpServerDefinition {
        name: name.into(),
        config: McpServerConfig {
            command: Some(env!("CARGO_BIN_EXE_mcp-test-server").into()),
            args: vec![delay.to_string()],
            startup_timeout_sec: Some(5.0),
            ..Default::default()
        },
        origin: McpOrigin::Extension("fixture".into()),
    }
}

#[cfg(unix)]
#[tokio::test]
async fn session_drop_and_startup_timeout_kill_the_process_group() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/mcp-tests");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(root).unwrap();
    for startup_timeout in [false, true] {
        let path = temp.path().join(if startup_timeout {
            "timeout.pids"
        } else {
            "drop.pids"
        });
        let mut definition = definition("group", if startup_timeout { 3000 } else { 0 });
        definition
            .config
            .env
            .insert("MCP_PID_FILE".into(), path.display().to_string());
        definition
            .config
            .env
            .insert("HOME".into(), temp.path().display().to_string());
        definition
            .config
            .env
            .insert("TIECHUI_HOME".into(), temp.path().display().to_string());
        if startup_timeout {
            definition.config.startup_timeout_sec = Some(1.5);
        }
        let (module, registry, _) = start(vec![definition]);
        ready(&module).await;
        let pids: Vec<i32> = std::fs::read_to_string(&path)
            .unwrap()
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect();
        if startup_timeout {
            assert_eq!(module.statuses()[0].state, ServerState::Failed);
            assert!(
                module.statuses()[0]
                    .error
                    .as_ref()
                    .unwrap()
                    .contains("timed out")
            );
        }
        drop(module);
        drop(registry);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let running = pids.iter().any(|pid| {
                    let missing = unsafe { libc::kill(*pid, 0) } == -1
                        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
                    #[cfg(target_os = "linux")]
                    let zombie =
                        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                            stat.rsplit_once(") ")
                                .is_some_and(|(_, fields)| fields.starts_with("Z "))
                        });
                    #[cfg(not(target_os = "linux"))]
                    let zombie = false;
                    !missing && !zombie
                });
                if !running {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}

fn start(definitions: Vec<McpServerDefinition>) -> (McpModule, Registry, CancellationToken) {
    let cancel = CancellationToken::new();
    let module = McpModule::start(
        definitions,
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
        false,
        cancel.clone(),
        Arc::new(DeclineElicitation),
    );
    let mut registry = Registry::default();
    module
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    (module, registry, cancel)
}

fn context(cancel: CancellationToken) -> ToolContext {
    ToolContext {
        invoker: None,
        branch: Default::default(),
        parent_session: None,
        cwd: env!("CARGO_MANIFEST_DIR").into(),
        cancel,
        updates: Arc::new(|_| {}),
        append_entry: Arc::new(|_| Ok(())),
        epoch: SessionEpoch(0),
        scope: ExecutionScope {
            thread_id: ThreadId("test".into()),
            turn_id: TurnId("turn".into()),
        },
        origin: Default::default(),
    }
}

async fn call(
    registry: &Registry,
    name: &str,
    arguments: Value,
    cancel: CancellationToken,
) -> Result<tiechui_plugin_api::ToolOutput, tiechui_plugin_api::ToolError> {
    registry
        .tools()
        .get(name)
        .unwrap()
        .run(
            ToolCall {
                id: "call".into(),
                name: name.into(),
                arguments,
                opaque: None,
            },
            context(cancel),
        )
        .await
}

async fn ready(module: &McpModule) {
    tokio::time::timeout(
        Duration::from_secs(5),
        module.wait_for_startup(CancellationToken::new()),
    )
    .await
    .unwrap();
}

#[test]
fn tool_names_match_fixtures() {
    let cases: Value =
        tiechui_plugin_api::serde_json::from_str(include_str!("fixtures/tool-names.json")).unwrap();
    for case in cases.as_array().unwrap() {
        assert_eq!(
            tool_name(
                case["server"].as_str().unwrap(),
                case["tool"].as_str().unwrap(),
                case["collision"].as_bool().unwrap()
            ),
            case["name"].as_str().unwrap()
        );
    }
}

#[tokio::test]
async fn stdio_listing_calls_errors_elicitation_resources_and_filtering() {
    let (module, registry, cancel) = start(vec![definition("test-server", 0)]);
    ready(&module).await;
    assert_eq!(
        module.statuses()[0].state,
        ServerState::Ready,
        "{:?}",
        module.statuses()
    );
    assert_eq!(module.statuses()[0].tools.len(), 9);
    let names: Vec<_> = registry
        .tools()
        .specs()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert!(names.contains(&tool_name("test-server", "a-b", true)));
    assert!(names.contains(&tool_name("test-server", "a_b", true)));
    let result = call(
        &registry,
        "mcp__test_server__echo",
        json!({"value":"hello"}),
        cancel.clone(),
    )
    .await
    .unwrap();
    assert!(matches!(&result.content[0], Content::Text { text, .. } if text == "hello"));
    let result = call(
        &registry,
        "mcp__test_server__error",
        json!({}),
        cancel.clone(),
    )
    .await
    .unwrap();
    assert!(result.is_error);
    assert_eq!(
        result.structured_content.unwrap()["structuredContent"]["reason"],
        "fixture"
    );
    let result = call(
        &registry,
        "mcp__test_server__elicit",
        json!({}),
        cancel.clone(),
    )
    .await
    .unwrap();
    assert!(matches!(&result.content[0], Content::Text { text, .. } if text == "Decline"));
    let result = call(&registry, "list_mcp_resources", json!({}), cancel.clone())
        .await
        .unwrap();
    let payload = result.structured_content.unwrap();
    assert_eq!(payload["resources"].as_array().unwrap().len(), 2);
    assert_eq!(payload["resources"][0]["server"], "test-server");
    assert!(payload["resources"][0].get("_meta").is_none());
    let result = call(
        &registry,
        "list_mcp_resources",
        json!({"server":"test-server"}),
        cancel.clone(),
    )
    .await
    .unwrap();
    assert_eq!(result.structured_content.unwrap()["nextCursor"], "second");
    let result = call(
        &registry,
        "list_mcp_resource_templates",
        json!({}),
        cancel.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        result.structured_content.unwrap()["resourceTemplates"][0]["name"],
        "Fixture"
    );
    let result = call(
        &registry,
        "read_mcp_resource",
        json!({"server":"test-server","uri":"fixture://first"}),
        cancel.clone(),
    )
    .await
    .unwrap();
    assert!(matches!(&result.content[0], Content::Text { text, .. } if text == "resource body"));
    let result = call(
        &registry,
        "mcp__test_server__long",
        json!({}),
        cancel.clone(),
    )
    .await
    .unwrap();
    let path = result.details.unwrap()["fullOutputPath"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "中".repeat(12_000));
    assert!(
        matches!(&result.content[0], Content::Text { text, .. } if text.contains("chars truncated"))
    );
    std::fs::remove_file(path).unwrap();
    cancel.cancel();

    let mut filtered = definition("filtered", 0);
    filtered.config.enabled_tools = Some(vec!["echo".into(), "error".into()]);
    filtered.config.disabled_tools = vec!["error".into()];
    let (module, _, cancel) = start(vec![filtered]);
    ready(&module).await;
    assert_eq!(module.statuses()[0].tools, vec!["mcp__filtered__echo"]);
    cancel.cancel();
}

#[tokio::test]
async fn startup_is_concurrent_failure_is_diagnostic_and_crash_removes_tools() {
    let mut failed = definition("failed", 0);
    failed.config.command = Some("/nonexistent/tiechui-mcp-fixture".into());
    let now = std::time::Instant::now();
    let (module, registry, cancel) = start(vec![
        definition("slow", 1500),
        definition("fast", 0),
        failed,
    ]);
    assert!(now.elapsed() < Duration::from_millis(100));
    tokio::time::timeout(Duration::from_secs(2), async {
        while registry.tools().get("mcp__fast__echo").is_none() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(registry.tools().get("mcp__slow__echo").is_none());
    ready(&module).await;
    let failed = module
        .statuses()
        .into_iter()
        .find(|s| s.name == "failed")
        .unwrap();
    assert_eq!(failed.state, ServerState::Failed);
    assert!(failed.error.is_some());
    assert!(
        call(&registry, "mcp__fast__crash", json!({}), cancel.clone())
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while registry.tools().get("mcp__fast__echo").is_some() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        module
            .statuses()
            .into_iter()
            .find(|s| s.name == "fast")
            .unwrap()
            .state,
        ServerState::Failed
    );
    cancel.cancel();
}

#[tokio::test]
async fn project_sources_require_trust_and_disabled_never_start() {
    let mut project = definition("project", 0);
    project.origin = McpOrigin::Project("project.toml".into());
    let mut disabled = definition("disabled", 0);
    disabled.config.enabled = Some(false);
    let (module, registry, cancel) = start(vec![project.clone(), disabled]);
    ready(&module).await;
    assert!(registry.tools().specs().is_empty());
    assert!(
        module
            .statuses()
            .iter()
            .all(|s| s.state == ServerState::Disabled)
    );
    cancel.cancel();
    let cancel = CancellationToken::new();
    let module = McpModule::start(
        vec![project],
        std::path::Path::new("."),
        true,
        cancel.clone(),
        Arc::new(DeclineElicitation),
    );
    ready(&module).await;
    assert_eq!(module.statuses()[0].state, ServerState::Ready);
    cancel.cancel();
}

#[tokio::test]
async fn streamable_http_calls_cancellation_and_timeout_notify_the_server() {
    use rmcp::transport::streamable_http_server::{
        StreamableHttpService, session::local::LocalSessionManager,
    };
    let handler = support::TestServer::default();
    let cancelled = handler.cancelled.clone();
    let service = StreamableHttpService::new(
        move || Ok(handler.clone()),
        Arc::new(LocalSessionManager::default()),
        Default::default(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let http = tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().nest_service("/mcp", service))
            .await
            .unwrap();
    });
    let mut definition = definition("http", 0);
    definition.config.command = None;
    definition.config.url = Some(format!("http://{addr}/mcp"));
    definition.config.tool_timeout_sec = Some(0.15);
    let (module, registry, cancel) = start(vec![definition]);
    ready(&module).await;
    assert_eq!(
        module.statuses()[0].state,
        ServerState::Ready,
        "{:?}",
        module.statuses()
    );
    let result = call(
        &registry,
        "mcp__http__echo",
        json!({"value":"http"}),
        cancel.clone(),
    )
    .await
    .unwrap();
    assert!(matches!(&result.content[0], Content::Text { text, .. } if text == "http"));
    let call_cancel = CancellationToken::new();
    let cancelling = call_cancel.clone();
    let operation = call(&registry, "mcp__http__wait", json!({}), call_cancel);
    let timer = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancelling.cancel();
    };
    let (result, _) = tokio::join!(operation, timer);
    assert!(result.unwrap_err().0.contains("cancelled"));
    let result = call(&registry, "mcp__http__wait", json!({}), cancel.clone()).await;
    assert!(result.is_err());
    tokio::time::timeout(Duration::from_secs(1), async {
        while cancelled.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    http.abort();
}

#[tokio::test]
async fn file_and_parsed_sources_preserve_environment_cwd_and_trust() {
    use tiechui_module_mcp::FileMcpSource;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/mcp-tests");
    std::fs::create_dir_all(&root).unwrap();
    let temp = tempfile::tempdir_in(root).unwrap();
    let mut definition = definition("source", 0);
    definition.config.cwd = Some(temp.path().canonicalize().unwrap());
    definition.config.env_vars = vec!["CARGO".into()];
    definition
        .config
        .env
        .insert("MCP_EXPLICIT".into(), "fixture".into());
    definition
        .config
        .env
        .insert("HOME".into(), temp.path().display().to_string());
    definition
        .config
        .env
        .insert("TIECHUI_HOME".into(), temp.path().display().to_string());
    let mut registry = Registry::default();
    registry.register_mcp_source(Arc::new(FileMcpSource(vec![definition])));
    let file = temp.path().join("project.toml");
    std::fs::write(
        &file,
        "[mcp_servers.untrusted]\ncommand = '/nonexistent/project'\n",
    )
    .unwrap();
    registry.register_mcp_source(Arc::new(FileMcpSource::load(&file, true).unwrap()));
    let cancel = CancellationToken::new();
    let module = McpModule::from_config(
        &ModuleConfig::default(),
        registry.mcp_sources(),
        temp.path(),
        None,
        false,
        cancel.clone(),
        Arc::new(DeclineElicitation),
        None,
    );
    module
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    ready(&module).await;
    let result = call(
        &registry,
        "mcp__source__environment",
        json!({}),
        cancel.clone(),
    )
    .await
    .unwrap();
    let Content::Text { text, .. } = &result.content[0] else {
        panic!("expected text");
    };
    let environment: Value = tiechui_plugin_api::serde_json::from_str(text).unwrap();
    assert_eq!(
        environment["cwd"],
        json!(temp.path().canonicalize().unwrap())
    );
    assert_eq!(environment["explicit"], "fixture");
    assert_eq!(environment["forwarded"], json!(std::env::var("CARGO").ok()));
    assert_eq!(environment["unrequested"], Value::Null);
    assert_eq!(
        module
            .statuses()
            .into_iter()
            .find(|s| s.name == "untrusted")
            .unwrap()
            .state,
        ServerState::Disabled
    );
    cancel.cancel();
}
