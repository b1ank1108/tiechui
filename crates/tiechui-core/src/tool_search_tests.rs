use super::*;
#[path = "ptc_tests.rs"]
mod ptc_tests;
use tiechui_module_mcp::{DeclineElicitation, McpModule};
use tiechui_module_tool_search::ToolSearchModule;
use tiechui_plugin_api::{
    BeforeToolCall, InterceptorStage, McpOrigin, McpServerConfig, McpServerDefinition, Module,
    ModuleConfig, PresentationContext, ProjectedRequest, ProjectionContext, RequestProjection,
    ToolInterceptor, ToolPresentation,
};

#[path = "../../../modules/mcp/tests/support/mod.rs"]
mod support;

struct McpFixture {
    module: McpModule,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<()>,
    calls: Arc<AtomicUsize>,
    cancelled: Arc<AtomicUsize>,
}
impl McpFixture {
    async fn new(count: Option<usize>) -> Self {
        let handler = support::TestServer {
            tool_count: count,
            ..Default::default()
        };
        let calls = handler.calls.clone();
        let cancelled = handler.cancelled.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let service = rmcp::transport::streamable_http_server::StreamableHttpService::new(
            move || Ok(handler.clone()), Arc::new(rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default()), Default::default());
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
        Self {
            module,
            stop,
            server,
            calls,
            cancelled,
        }
    }
    fn register(&self, registry: &mut Registry) {
        self.module
            .register(registry, &ModuleConfig::default())
            .unwrap();
        ToolSearchModule
            .register(registry, &ModuleConfig::default())
            .unwrap();
    }
}
impl Drop for McpFixture {
    fn drop(&mut self) {
        self.stop.cancel();
        self.server.abort();
    }
}
fn call_tool(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments,
        opaque: None,
    }
}
fn wrapped(id: &str, name: &str, arguments: serde_json::Value) -> ToolCall {
    call_tool(id, "call_tool", json!({"name":name,"arguments":arguments}))
}
fn context(registry: Arc<Registry>, scheduler: Arc<ToolScheduler>) -> ToolContext {
    ToolContext {
        invoker: Some(Arc::new(interception::Invoker {
            registry,
            scheduler,
            model: None,
            providers: Default::default(),
        })),
        branch: Default::default(),
        parent_session: None,
        cwd: std::env::current_dir().unwrap(),
        cancel: CancellationToken::new(),
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
fn present(registry: &Registry, window: u64) -> Vec<ToolSpec> {
    registry
        .presentation()
        .unwrap()
        .present(PresentationContext {
            tools: registry.tools().callable(),
            window,
            estimate_tools: |tools| tiechui_providers::estimate::input(&[], tools),
        })
}
fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|part| match part {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn small_mcp_catalog_keeps_all_definitions_and_resources_are_never_deferred() {
    let fixture = McpFixture::new(None).await;
    let mut registry = Registry::default();
    fixture
        .module
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let before = registry.tools().specs();
    ToolSearchModule
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    assert_eq!(present(&registry, 200_000), before);
    let tools = registry.tools().callable();
    assert!(
        tools
            .iter()
            .filter(|tool| tool.spec().name.starts_with("mcp__"))
            .all(|tool| tool.deferrable())
    );
    assert!(
        tools
            .iter()
            .any(|tool| !tool.deferrable() && tool.source().is_some())
    );
    let registry = Arc::new(registry);
    let scheduler = Arc::new(ToolScheduler::default());
    let ctx = context(registry.clone(), scheduler.clone());
    let output = interception::execute_call(
        &registry,
        &scheduler,
        wrapped("old", "mcp__fixture__echo", json!({"value":"history"})),
        ctx,
    )
    .await;
    assert_eq!(text(&output), "history");
}

struct PresentationCounter {
    inner: Arc<dyn ToolPresentation>,
    count: Arc<AtomicUsize>,
}
impl ToolPresentation for PresentationCounter {
    fn present(&self, ctx: PresentationContext) -> Vec<ToolSpec> {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.inner.present(ctx)
    }
}
struct Projection(Arc<Mutex<Vec<Vec<ToolSpec>>>>);
#[async_trait]
impl RequestProjection for Projection {
    async fn project(
        &self,
        ctx: ProjectionContext,
        _: CancellationToken,
    ) -> Result<Option<ProjectedRequest>, ToolError> {
        self.0.lock().unwrap().push(ctx.tools);
        Ok(None)
    }
}

#[tokio::test]
async fn thousand_mcp_tools_have_stable_requests_through_search_and_calls_until_disconnect() {
    let fixture = McpFixture::new(Some(1000)).await;
    let projection = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let scripts = vec![
        reply(vec![Content::ToolCall(call_tool(
            "s1",
            "tool_search",
            json!({"names":["mcp__fixture__tool_0001"]}),
        ))]),
        reply(vec![Content::ToolCall(wrapped(
            "c1",
            "mcp__fixture__tool_0001",
            json!({"count":"2","value":"first"}),
        ))]),
        reply(vec![Content::ToolCall(call_tool(
            "s2",
            "tool_search",
            json!({"query":"value", "limit":2}),
        ))]),
        reply(vec![Content::ToolCall(wrapped(
            "c2",
            "mcp__fixture__tool_0002",
            json!({"count":3,"value":"second"}),
        ))]),
        answer("done"),
        answer("disconnected"),
    ];
    let mut harness = Harness::with_contributions(
        scripts,
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| {
            fixture
                .module
                .register(registry, &ModuleConfig::default())
                .unwrap();
            let declarations =
                Registry::from_modules(&[&ToolSearchModule], &ModuleConfig::default()).unwrap();
            for tool in declarations.tools().callable() {
                registry.register_tool(tool).unwrap();
            }
            tiechui_module_context::ContextModule
                .register(registry, &ModuleConfig::default())
                .unwrap();
            registry.register_projection(Arc::new(Projection(projection.clone())));
            // Wrap the registered contribution to count exactly one presentation per request.
            registry
                .register_presentation(Arc::new(PresentationCounter {
                    inner: declarations.presentation().unwrap().clone(),
                    count: count.clone(),
                }))
                .unwrap();
        },
    )
    .await;
    harness
        .input("discover and call", InputDelivery::FollowUp)
        .await;
    let mut requests = Vec::new();
    for _ in 0..5 {
        requests.push(harness.request().await);
    }
    harness.end(TurnStatus::Completed).await;
    assert_eq!(count.load(Ordering::SeqCst), 5);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    let tools = &requests[0].tools;
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        [
            "list_mcp_resource_templates",
            "list_mcp_resources",
            "probe",
            "read_mcp_resource",
            "tool_search",
            "call_tool"
        ]
    );
    assert!(
        tools
            .iter()
            .find(|tool| tool.name == "tool_search")
            .unwrap()
            .description
            .contains("1000 tools")
    );
    let bytes = serde_json::to_vec(tools).unwrap();
    for (request, projected) in requests.iter().zip(projection.lock().unwrap().iter()) {
        assert_eq!(serde_json::to_vec(&request.tools).unwrap(), bytes);
        assert_eq!(&request.tools, projected);
        assert_eq!(
            inspection::prefix(&request.messages, &request.tools),
            inspection::prefix(&requests[0].messages, tools)
        );
    }
    assert_eq!(
        requests[0].input_tokens,
        Some(tiechui_providers::estimate::input(
            &requests[0].messages,
            tools
        ))
    );
    let snapshot = super::inspection_tests::open(&mut harness, "tools").await;
    assert_eq!(
        snapshot.tools.iter().map(|item| item.tokens).sum::<u64>(),
        tiechui_providers::estimate::input(&[], tools)
    );
    assert_eq!(snapshot.tools.len(), tools.len());
    assert!(
        snapshot
            .tools
            .iter()
            .all(|item| !item.label.starts_with("mcp__fixture__"))
    );
    assert!(
        !std::fs::read_to_string(&harness.path)
            .unwrap()
            .contains("\"kind\":\"tool-search")
    );
    assert!(
        results(&requests[4])
            .iter()
            .any(|result| result.content == vec![text_content("second".into())])
    );
    assert_http_bodies(&requests).await;
    fixture.stop.cancel();
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture
            .module
            .statuses()
            .iter()
            .any(|status| !status.tools.is_empty())
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    harness
        .input("after disconnect", InputDelivery::FollowUp)
        .await;
    let disconnected = harness.request().await;
    harness.end(TurnStatus::Completed).await;
    assert_ne!(serde_json::to_vec(&disconnected.tools).unwrap(), bytes);
    assert_eq!(
        disconnected
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["probe"]
    );
    harness.stop().await;
}

async fn assert_http_bodies(requests: &[ModelRequest]) {
    use tiechui_config::{ModelConfig, ProviderConfig, Wire};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};
    for wire in [
        Wire::AnthropicMessages,
        Wire::OpenaiCompletions,
        Wire::OpenaiResponses,
    ] {
        let server = MockServer::start().await;
        let response = match wire {
            Wire::AnthropicMessages => {
                include_str!("../../tiechui-providers/tests/fixtures/anthropic/plain_text.sse")
            }
            Wire::OpenaiCompletions => include_str!(
                "../../tiechui-providers/tests/fixtures/openai-completions/plain_text.sse"
            ),
            Wire::OpenaiResponses => include_str!(
                "../../tiechui-providers/tests/fixtures/openai-responses/plain_text.sse"
            ),
        };
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(response, "text/event-stream"))
            .expect(requests.len() as u64)
            .mount(&server)
            .await;
        let provider = tiechui_providers::testing::provider(
            "fixture",
            &ProviderConfig {
                wire,
                base_url: Some(server.uri()),
                api_key: None,
                api_key_env: None,
                compat: Default::default(),
                models: vec![
                    serde_json::from_value::<ModelConfig>(
                        json!({"id":"model","context_window":200_000,"max_output":4096}),
                    )
                    .unwrap(),
                ],
            },
        )
        .unwrap();
        for request in requests {
            let mut request = request.clone();
            request.model = ModelId("fixture/model".into());
            tokio::time::timeout(Duration::from_secs(5), async {
                let stream = provider
                    .stream(request, CancellationToken::new())
                    .await
                    .unwrap();
                assert!(stream.collect::<Vec<_>>().await.iter().all(Result::is_ok));
            })
            .await
            .expect("HTTP fixture timeout");
        }
        let received = server.received_requests().await.unwrap();
        let mut previous = None;
        for (received, request) in received.iter().zip(requests) {
            let body: serde_json::Value = serde_json::from_slice(&received.body).unwrap();
            assert_eq!(
                tiechui_providers::testing::tool_definitions(wire, &body).unwrap(),
                request.tools
            );
            let bytes = serde_json::to_vec(&body["tools"]).unwrap();
            if let Some(previous) = &previous {
                assert_eq!(&bytes, previous);
            }
            previous = Some(bytes);
        }
    }
}

#[derive(Default)]
struct Approvals(Mutex<Vec<tiechui_module_permissions::ApprovalRequest>>);
#[async_trait]
impl tiechui_module_permissions::ApprovalRequester for Approvals {
    async fn request(
        &self,
        request: tiechui_module_permissions::ApprovalRequest,
        _: CancellationToken,
    ) -> Result<tiechui_module_permissions::ApprovalDecision, ToolError> {
        self.0.lock().unwrap().push(request);
        Ok(tiechui_module_permissions::ApprovalDecision::AllowOnce)
    }
}
type HookLog = Arc<Mutex<Vec<(InterceptorStage, String, String)>>>;
struct Recorder {
    stage: InterceptorStage,
    log: HookLog,
}
#[async_trait]
impl ToolInterceptor for Recorder {
    fn stage(&self) -> InterceptorStage {
        self.stage
    }
    async fn before(&self, call: &ToolCall, _: ToolContext) -> Result<BeforeToolCall, ToolError> {
        self.log
            .lock()
            .unwrap()
            .push((self.stage, "before".into(), call.name.clone()));
        Ok(BeforeToolCall::Allow)
    }
    async fn after(
        &self,
        call: &ToolCall,
        _: &ToolOutput,
        _: ToolContext,
    ) -> Result<Option<ToolOutput>, ToolError> {
        self.log
            .lock()
            .unwrap()
            .push((self.stage, "after".into(), call.name.clone()));
        Ok(None)
    }
    async fn settled(&self, call: &ToolCall, _: ToolContext) -> Result<(), ToolError> {
        self.log
            .lock()
            .unwrap()
            .push((self.stage, "settled".into(), call.name.clone()));
        Ok(())
    }
}

#[tokio::test]
async fn strict_permissions_ask_once_for_real_coerced_call_and_policy_hooks_skip_only_wrapper() {
    let fixture = McpFixture::new(Some(1)).await;
    let mut registry = Registry::default();
    fixture.register(&mut registry);
    let approvals = Arc::new(Approvals::default());
    let cwd = std::env::current_dir().unwrap();
    let mut config = ModuleConfig::default();
    config.permissions.rules = Some(vec![]);
    tiechui_module_permissions::PermissionsModule::with_requester(
        cwd.clone(),
        cwd,
        approvals.clone(),
    )
    .register(&mut registry, &config)
    .unwrap();
    let log: HookLog = Arc::default();
    for stage in [
        InterceptorStage::BeforePolicy,
        InterceptorStage::Policy,
        InterceptorStage::AfterPolicy,
    ] {
        registry.register_tool_interceptor(
            Arc::new(Recorder {
                stage,
                log: log.clone(),
            }),
            Duration::from_secs(1),
        );
    }
    let registry = Arc::new(registry);
    let scheduler = Arc::new(ToolScheduler::default());
    let output = interception::execute_call(
        &registry,
        &scheduler,
        wrapped(
            "parent",
            "mcp__fixture__tool_0000",
            json!({"count":"7","value":"approved"}),
        ),
        context(registry.clone(), scheduler.clone()),
    )
    .await;
    assert!(!output.is_error, "{output:?}");
    {
        let requests = approvals.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].call.name, "mcp__fixture__tool_0000");
        assert_eq!(requests[0].call.id, "parent.1");
        assert_eq!(
            requests[0].call.arguments,
            json!({"count":7,"value":"approved"})
        );
    }
    let wrapped_log = log.lock().unwrap().clone();
    for stage in [
        InterceptorStage::BeforePolicy,
        InterceptorStage::Policy,
        InterceptorStage::AfterPolicy,
    ] {
        for phase in ["before", "after", "settled"] {
            let count = wrapped_log
                .iter()
                .filter(|entry| entry == &&(stage, phase.into(), "call_tool".into()))
                .count();
            assert_eq!(
                count,
                usize::from(stage != InterceptorStage::Policy || phase == "settled")
            );
        }
    }
    log.lock().unwrap().clear();
    let direct = interception::execute_call(
        &registry,
        &scheduler,
        call_tool(
            "direct",
            "mcp__fixture__tool_0000",
            json!({"count":"7","value":"approved"}),
        ),
        context(registry.clone(), scheduler.clone()),
    )
    .await;
    assert_eq!(direct, output);
    assert_eq!(
        wrapped_log
            .into_iter()
            .filter(|(_, _, name)| name == "mcp__fixture__tool_0000")
            .collect::<Vec<_>>(),
        *log.lock().unwrap()
    );
}

#[tokio::test]
async fn real_name_denials_and_allowed_tool_filter_prevent_child_execution() {
    let fixture = McpFixture::new(None).await;
    for restricted in [false, true] {
        let mut registry = Registry::default();
        fixture.register(&mut registry);
        if restricted {
            registry.restrict_tools(vec![
                "tool_search".into(),
                "call_tool".into(),
                "mcp__fixture__error".into(),
            ]);
            let tools = present(&registry, 1);
            assert!(
                !tools
                    .iter()
                    .any(|spec| spec.description.contains("mcp__fixture__echo"))
            );
        } else {
            let mut config = ModuleConfig::default();
            config.permissions.rules = Some(
                serde_json::from_value(
                    json!([{"surface":"mcp","pattern":"mcp__fixture__echo","action":"deny"}]),
                )
                .unwrap(),
            );
            let cwd = std::env::current_dir().unwrap();
            tiechui_module_permissions::PermissionsModule::headless(
                cwd.clone(),
                cwd,
                tiechui_module_permissions::HeadlessPolicy::Unavailable,
            )
            .with_mode(tiechui_plugin_api::PermissionMode::Yolo)
            .register(&mut registry, &config)
            .unwrap();
        }
        let registry = Arc::new(registry);
        let scheduler = Arc::new(ToolScheduler::default());
        let output = interception::execute_call(
            &registry,
            &scheduler,
            wrapped("parent", "mcp__fixture__echo", json!({})),
            context(registry.clone(), scheduler.clone()),
        )
        .await;
        assert!(output.is_error);
        if restricted {
            assert_eq!(
                text(&output),
                "Unknown tool mcp__fixture__echo; use tool_search to find tools."
            );
        }
    }
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn child_validation_includes_schema_and_mcp_result_fields_pass_through() {
    let fixture = McpFixture::new(Some(1)).await;
    let mut registry = Registry::default();
    fixture.register(&mut registry);
    let registry = Arc::new(registry);
    let scheduler = Arc::new(ToolScheduler::default());
    let output = interception::execute_call(
        &registry,
        &scheduler,
        wrapped(
            "parent",
            "mcp__fixture__tool_0000",
            json!({"count":"wrong"}),
        ),
        context(registry.clone(), scheduler.clone()),
    )
    .await;
    assert!(output.is_error);
    assert!(text(&output).starts_with("Validation failed for tool \"mcp__fixture__tool_0000\":"));
    assert!(text(&output).ends_with(&format!(
            "Parameters:\n{}",
            registry
                .tools()
                .get("mcp__fixture__tool_0000")
                .unwrap()
                .spec()
                .input_schema
        )));
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    let fixture = McpFixture::new(None).await;
    let mut registry = Registry::default();
    fixture.register(&mut registry);
    let registry = Arc::new(registry);
    for name in ["mcp__fixture__echo", "mcp__fixture__error"] {
        let direct = interception::execute_call(
            &registry,
            &scheduler,
            call_tool("direct", name, json!({"value":"sample"})),
            context(registry.clone(), scheduler.clone()),
        )
        .await;
        let indirect = interception::execute_call(
            &registry,
            &scheduler,
            wrapped("parent", name, json!({"value":"sample"})),
            context(registry.clone(), scheduler.clone()),
        )
        .await;
        assert_eq!(direct, indirect);
    }
}

#[tokio::test]
async fn delegated_mcp_and_top_level_edit_do_not_deadlock_and_stay_exclusive_on_cancellation() {
    let fixture = McpFixture::new(None).await;
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let path = root.path().join("file.txt");
    std::fs::write(&path, "before").unwrap();
    let mut registry = Registry::default();
    fixture.register(&mut registry);
    tiechui_module_editing::EditingModule
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let registry = Arc::new(registry);
    let scheduler = Arc::new(ToolScheduler::default());
    let ctx = context(registry.clone(), scheduler.clone());
    let cancel = ctx.cancel.clone();
    let child = interception::execute_call(
        &registry,
        &scheduler,
        wrapped("parent", "mcp__fixture__wait", json!({})),
        ctx,
    );
    let edit = interception::execute_call(
        &registry,
        &scheduler,
        call_tool(
            "edit",
            "edit",
            json!({"path":path,"edits":[{"oldText":"before","newText":"after"}]}),
        ),
        context(registry.clone(), scheduler.clone()),
    );
    let verify = async {
        while fixture.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "before",
            "edit ran during the MCP call"
        );
        cancel.cancel();
    };
    let (child, edit, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(child, edit, verify)
    })
    .await
    .unwrap();
    assert!(child.is_error);
    assert!(!edit.is_error, "{edit:?}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "after");
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.cancelled.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn user_hooks_match_real_names_and_wildcards_also_see_the_wrapper() {
    let fixture = McpFixture::new(None).await;
    let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let mut hooks = serde_json::Map::new();
    for event in ["PreToolUse", "PostToolUse"] {
        hooks.insert(event.into(), json!(["*", "mcp__fixture__echo"].map(|matcher| {
            let path = root.path().join(format!("{event}-{}.jsonl", if matcher == "*" {"all"} else {"real"}));
            let quoted = format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
            json!({"matcher":matcher, "hooks":[{"type":"command", "command":format!("cat >> {quoted}; printf '\\n' >> {quoted}")}]})
        })));
    }
    std::fs::write(home.join("hooks.json"), json!({"hooks":hooks}).to_string()).unwrap();
    let hooks = tiechui_module_hooks::HooksModule::load(
        root.path(),
        &home,
        false,
        Default::default(),
        &CancellationToken::new(),
    )
    .unwrap();
    let mut registry = Registry::default();
    fixture.register(&mut registry);
    hooks
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let registry = Arc::new(registry);
    let scheduler = Arc::new(ToolScheduler::default());
    for call in [
        wrapped("parent", "mcp__fixture__echo", json!({"value":"ok"})),
        call_tool("direct", "mcp__fixture__echo", json!({"value":"ok"})),
    ] {
        let output = interception::execute_call(
            &registry,
            &scheduler,
            call,
            context(registry.clone(), scheduler.clone()),
        )
        .await;
        assert!(!output.is_error, "{output:?}");
    }
    for event in ["PreToolUse", "PostToolUse"] {
        for (scope, expected) in [("all", 3), ("real", 2)] {
            let log = std::fs::read_to_string(root.path().join(format!("{event}-{scope}.jsonl")))
                .unwrap();
            let calls: Vec<serde_json::Value> = log
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(calls.len(), expected);
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call["tool_name"] == "mcp__fixture__echo")
                    .count(),
                2
            );
        }
    }
}
