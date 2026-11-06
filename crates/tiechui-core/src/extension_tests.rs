use super::*;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tiechui_model::ToolSpec;
use tiechui_plugin_api::{
    BeforeToolCall, ContextProvider, Module, ModuleConfig, SessionObserver, StaticContext,
    ToolInterceptor, TurnObserver,
};
use tiechui_tools::{Concurrency, Tool, ToolError};

#[tokio::test]
async fn live_mcp_tools_use_interceptors() {
    use rmcp::{ServerHandler, model::*, service::RequestContext};
    use tiechui_module_mcp::{DeclineElicitation, McpModule};
    use tiechui_plugin_api::{McpOrigin, McpServerConfig, McpServerDefinition};

    struct Server;
    impl ServerHandler for Server {
        fn get_info(&self) -> ServerConfig {
            ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
        }
        async fn list_tools(
            &self,
            _: Option<PaginatedRequestParams>,
            _: RequestContext<rmcp::RoleServer>,
        ) -> Result<ListToolsResult, rmcp::ErrorData> {
            Ok(serde_json::from_value(json!({"tools":[{"name":"echo","inputSchema":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}]})).unwrap())
        }
        async fn call_tool(
            &self,
            params: CallToolRequestParams,
            _: RequestContext<rmcp::RoleServer>,
        ) -> Result<CallToolResponse, rmcp::ErrorData> {
            Ok(
                CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                    params.arguments.unwrap()["value"].as_str().unwrap(),
                )])
                .into(),
            )
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let service = rmcp::transport::streamable_http_server::StreamableHttpService::new(
        || Ok(Server),
        Arc::new(
            rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
        ),
        Default::default(),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, axum::Router::new().nest_service("/mcp", service))
            .await
            .unwrap();
    });
    let stop = CancellationToken::new();
    let module = McpModule::start(
        vec![McpServerDefinition {
            name: "fixture".into(),
            origin: McpOrigin::Extension("test".into()),
            config: McpServerConfig {
                url: Some(format!("http://{address}/mcp")),
                ..Default::default()
            },
        }],
        std::path::Path::new("."),
        false,
        stop.clone(),
        Arc::new(DeclineElicitation),
    );
    module.wait_for_startup(stop.clone()).await;
    let mut registry = Registry::default();
    module
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    assert!(
        registry.tools().get("mcp__fixture__echo").is_some(),
        "{:?}",
        module.statuses()
    );
    let log = Arc::new(Mutex::new(Vec::new()));
    registry.register_tool_interceptor(
        Arc::new(Interceptor {
            behavior: "rewrite",
            log: log.clone(),
            cancelled: Arc::new(Mutex::new(None)),
        }),
        Duration::from_secs(1),
    );
    let output = interception::execute_call(
        &registry,
        &ToolScheduler::default(),
        tiechui_model::ToolCall {
            id: "mcp".into(),
            name: "mcp__fixture__echo".into(),
            arguments: json!({"value":"original"}),
            opaque: None,
        },
        ToolContext {
            invoker: None,
            branch: Default::default(),
            parent_session: None,
            cwd: ".".into(),
            cancel: stop.clone(),
            updates: Arc::new(|_| {}),
            append_entry: Arc::new(|_| Ok(())),
            epoch: SessionEpoch(0),
            scope: ExecutionScope {
                thread_id: ThreadId("test".into()),
                turn_id: TurnId("turn".into()),
            },
            origin: Default::default(),
        },
    )
    .await;
    assert!(!output.is_error, "{output:?}");
    assert!(
        matches!(&output.content[0], tiechui_model::Content::Text { text, .. } if text == "rewritten")
    );
    assert_eq!(log.lock().unwrap().len(), 2);
    let permissions = tiechui_module_permissions::PermissionsModule::headless(
        std::env::current_dir().unwrap(),
        std::env::current_dir().unwrap(),
        tiechui_module_permissions::HeadlessPolicy::Unavailable,
    )
    .with_mode(tiechui_plugin_api::PermissionMode::Yolo);
    let mut config = ModuleConfig::default();
    config.permissions.rules = Some(
        serde_json::from_value(
            json!([{"surface":"mcp","pattern":"mcp__fixture__echo","action":"deny"}]),
        )
        .unwrap(),
    );
    permissions.register(&mut registry, &config).unwrap();
    let entries = Arc::new(Mutex::new(Vec::new()));
    let recorded = entries.clone();
    let output = interception::execute_call(
        &registry,
        &ToolScheduler::default(),
        tiechui_model::ToolCall {
            id: "denied-mcp".into(),
            name: "mcp__fixture__echo".into(),
            arguments: json!({"value":"original"}),
            opaque: None,
        },
        ToolContext {
            invoker: None,
            branch: Default::default(),
            parent_session: None,
            cwd: std::env::current_dir().unwrap(),
            cancel: stop.clone(),
            updates: Arc::new(|_| {}),
            append_entry: Arc::new(move |entry| {
                recorded.lock().unwrap().push(entry);
                Ok(())
            }),
            epoch: SessionEpoch(0),
            scope: ExecutionScope {
                thread_id: ThreadId("test".into()),
                turn_id: TurnId("turn".into()),
            },
            origin: Default::default(),
        },
    )
    .await;
    assert!(output.is_error, "{output:?}");
    assert!(
        matches!(&output.content[0], tiechui_model::Content::Text { text, .. } if text.contains("Denied by policy"))
    );
    assert_eq!(entries.lock().unwrap()[0].data["surface"], "mcp");
    stop.cancel();
    server.abort();
}

struct EchoTool {
    spec: ToolSpec,
    runs: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for EchoTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &serde_json::Value) -> Concurrency {
        Concurrency::Exclusive
    }
    async fn run(&self, call: ToolCall, _: ToolContext) -> Result<ToolOutput, ToolError> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text(call.arguments["value"].as_str().unwrap()))
    }
}

struct Interceptor {
    behavior: &'static str,
    log: Arc<Mutex<Vec<String>>>,
    cancelled: Arc<Mutex<Option<CancellationToken>>>,
}

#[async_trait]
impl ToolInterceptor for Interceptor {
    async fn before(&self, call: &ToolCall, ctx: ToolContext) -> Result<BeforeToolCall, ToolError> {
        self.log.lock().unwrap().push(format!(
            "before:{}:{}",
            self.behavior, call.arguments["value"]
        ));
        match self.behavior {
            "deny" => Ok(BeforeToolCall::Deny("blocked".into())),
            "rewrite" => Ok(BeforeToolCall::Rewrite(json!({"value":"rewritten"}))),
            "invalid" => Ok(BeforeToolCall::Rewrite(json!({"value":null}))),
            "error" => Err(ToolError("callback failed".into())),
            "timeout" => {
                *self.cancelled.lock().unwrap() = Some(ctx.cancel);
                futures::future::pending().await
            }
            _ => Ok(BeforeToolCall::Allow),
        }
    }
    async fn after(
        &self,
        _: &ToolCall,
        output: &ToolOutput,
        ctx: ToolContext,
    ) -> Result<Option<ToolOutput>, ToolError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("after:{}:{:?}", self.behavior, output.content));
        match self.behavior {
            "replace" => Ok(Some(ToolOutput::text("replacement"))),
            "after-error" => Err(ToolError("result callback failed".into())),
            "after-timeout" => {
                *self.cancelled.lock().unwrap() = Some(ctx.cancel);
                futures::future::pending().await
            }
            _ => Ok(None),
        }
    }
}

#[tokio::test]
async fn interceptors_deny_rewrite_observe_replace_and_fail_closed_for_all_tool_names() {
    for name in ["bash", "edit", "mcp__future__tool"] {
        for behaviors in [
            vec!["deny"],
            vec!["rewrite", "observe"],
            vec!["replace", "observe"],
            vec!["error", "replace"],
            vec!["timeout", "replace"],
            vec!["after-error", "replace"],
            vec!["after-timeout", "replace"],
            vec!["invalid"],
        ] {
            let runs = Arc::new(AtomicUsize::new(0));
            let log = Arc::new(Mutex::new(Vec::new()));
            let cancelled = Arc::new(Mutex::new(None));
            let mut registry = Registry::default();
            registry.register_tool(Arc::new(EchoTool {
                spec: ToolSpec { name: name.into(), description: String::new(), input_schema: json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}) },
                runs: runs.clone(),
            })).unwrap();
            for behavior in &behaviors {
                registry.register_tool_interceptor(
                    Arc::new(Interceptor {
                        behavior,
                        log: log.clone(),
                        cancelled: cancelled.clone(),
                    }),
                    Duration::from_millis(10),
                );
            }
            let call = ToolCall {
                id: "call".into(),
                name: name.into(),
                arguments: json!({"value":"original"}),
                opaque: None,
            };
            let ctx = ToolContext {
                invoker: None,
                branch: Default::default(),
                parent_session: None,
                cwd: PathBuf::from("/project"),
                cancel: CancellationToken::new(),
                updates: Arc::new(|_| {}),
                append_entry: Arc::new(|_| Ok(())),
                epoch: SessionEpoch(0),
                scope: ExecutionScope {
                    thread_id: ThreadId("thread".into()),
                    turn_id: TurnId("turn".into()),
                },
                origin: Default::default(),
            };
            let output =
                interception::execute_call(&registry, &ToolScheduler::default(), call, ctx).await;
            match behaviors[0] {
                "rewrite" => {
                    assert_eq!(output, ToolOutput::text("rewritten"));
                    assert_eq!(log.lock().unwrap()[1], "before:observe:\"rewritten\"");
                    assert_eq!(runs.load(Ordering::SeqCst), 1);
                }
                "replace" => {
                    assert_eq!(output, ToolOutput::text("replacement"));
                    assert!(log.lock().unwrap().last().unwrap().contains("replacement"));
                }
                behavior => {
                    assert!(output.is_error, "{behavior}: {output:?}");
                    assert_eq!(
                        runs.load(Ordering::SeqCst),
                        usize::from(behavior.starts_with("after-"))
                    );
                    if behavior.contains("timeout") {
                        assert!(cancelled.lock().unwrap().as_ref().unwrap().is_cancelled());
                        assert!(format!("{:?}", output.content).contains("timed out"));
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn actor_persists_interceptor_denial_and_returns_it_to_the_model() {
    let mut h = Harness::with_contributions(
        vec![reply(vec![call("denied")]), answer("done")],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| {
            registry.register_tool_interceptor(
                Arc::new(Interceptor {
                    behavior: "deny",
                    log: Arc::default(),
                    cancelled: Arc::default(),
                }),
                Duration::from_secs(1),
            );
        },
    )
    .await;
    h.input("run", InputDelivery::FollowUp).await;
    h.request().await;
    let next = h.request().await;
    assert_eq!(h.tool.runs.load(Ordering::SeqCst), 0);
    let outputs = results(&next);
    assert_eq!(outputs.len(), 1);
    assert!(outputs[0].is_error);
    assert_eq!(outputs[0].content, ToolOutput::text("blocked").content);
    h.end(TurnStatus::Completed).await;
    h.valid_transcript().await;
    h.stop().await;
}

struct InputContext;
#[async_trait]
impl ContextProvider for InputContext {
    async fn fragments(
        &self,
        input: &TurnInput,
        _: CancellationToken,
    ) -> Result<Vec<String>, ToolError> {
        Ok(input
            .content
            .iter()
            .filter_map(|part| match part {
                InputContent::Text { text } => Some(format!("input:{text}")),
                _ => None,
            })
            .collect())
    }
}

struct ContextModule(&'static str);
impl Module for ContextModule {
    fn id(&self) -> &'static str {
        self.0
    }
    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        registry.register_context_provider(Arc::new(StaticContext(vec![
            format!(" {}1\n", self.0),
            String::new(),
        ])));
        registry.register_context_provider(Arc::new(StaticContext(vec![format!("{}2 ", self.0)])));
        Ok(())
    }
}

#[derive(Default)]
struct Observations {
    turns: Mutex<Vec<EventKind>>,
    sessions: Mutex<Vec<SessionEvent>>,
}
impl TurnObserver for Observations {
    fn observe(&self, event: &Event) {
        self.turns.lock().unwrap().push(event.event.clone());
    }
}
impl SessionObserver for Observations {
    fn observe(&self, event: &SessionEvent) {
        self.sessions.lock().unwrap().push(event.clone());
    }
}

#[tokio::test]
async fn context_order_bytes_input_and_observers_survive_repeated_turns() {
    let observations = Arc::new(Observations::default());
    let observed = observations.clone();
    let mut h = Harness::with_contributions(
        vec![answer("one"), answer("two"), answer("three")],
        Arc::new(BoundedRetry::default()),
        json!({}),
        move |registry| {
            *registry = Registry::from_modules(
                &[&ContextModule("b"), &ContextModule("a")],
                &ModuleConfig::default(),
            )
            .unwrap();
            registry.register_context_provider(Arc::new(InputContext));
            registry.register_turn_observer(observed.clone());
            registry.register_session_observer(observed);
        },
    )
    .await;
    let mut prompts = Vec::new();
    for input in ["same", "same", "changed"] {
        h.input(input, InputDelivery::FollowUp).await;
        let request = h.request().await;
        prompts.push(request.messages[0].content.clone());
        h.end(TurnStatus::Completed).await;
    }
    assert_eq!(
        prompts[0],
        vec![text_content(
            " b1\n\n\n\n\nb2 \n\n a1\n\n\n\n\na2 \n\ninput:same".into()
        )]
    );
    assert_eq!(
        serde_json::to_vec(&prompts[0]).unwrap(),
        serde_json::to_vec(&prompts[1]).unwrap()
    );
    assert_ne!(prompts[1], prompts[2]);
    h.stop().await;
    assert_eq!(observations.sessions.lock().unwrap().len(), 2);
    assert_eq!(
        observations
            .turns
            .lock()
            .unwrap()
            .iter()
            .filter(|event| matches!(event, EventKind::TurnCompleted { .. }))
            .count(),
        3
    );
}
