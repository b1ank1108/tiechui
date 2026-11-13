#![allow(clippy::unwrap_used)]

use super::*;
use std::sync::Mutex;
use tiechui_plugin_api::{
    CancellationToken, ToolInvoker,
    protocol::{SessionEpoch, ThreadId, TurnId},
    tools::{ExecutionScope, ToolUpdate},
};

struct FixtureTool {
    spec: ToolSpec,
    source: String,
    deferred: bool,
}
#[async_trait]
impl Tool for FixtureTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn source(&self) -> Option<String> {
        Some(self.source.clone())
    }
    fn deferrable(&self) -> bool {
        self.deferred
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Exclusive
    }
    async fn run(&self, _: ToolCall, _: ToolContext) -> Result<ToolOutput, ToolError> {
        unreachable!()
    }
}

fn fixture(name: &str, source: &str, description: &str) -> Arc<dyn Tool> {
    Arc::new(FixtureTool {
        spec: ToolSpec {
            name: name.into(),
            description: description.into(),
            input_schema: json!({"type":"object","properties":{"pullNumber":{"type":"integer","description":"Issue identifier"}}}),
        },
        source: source.into(),
        deferred: name != "read",
    })
}
fn registry() -> Registry {
    let mut registry =
        Registry::from_modules(&[&ToolSearchModule], &ModuleConfig::default()).unwrap();
    for tool in [
        fixture(
            "mcp__github__addComment",
            "MCP server: github",
            "Add a comment to a pull request. 合并请求评论",
        ),
        fixture(
            "mcp__linear__get_issue",
            "MCP server: linear",
            "Read issue details.",
        ),
        fixture("read", "Built-in", "Read a file."),
    ] {
        registry.register_tool(tool).unwrap();
    }
    registry
}
fn present(
    registry: &Registry,
    window: u64,
    estimate_tools: fn(&[ToolSpec]) -> u64,
) -> Vec<ToolSpec> {
    registry
        .presentation()
        .unwrap()
        .present(PresentationContext {
            tools: registry.tools().callable(),
            window,
            estimate_tools,
        })
}
fn context() -> ToolContext {
    ToolContext {
        invoker: None,
        branch: Default::default(),
        parent_session: None,
        cwd: ".".into(),
        cancel: CancellationToken::new(),
        updates: Arc::new(|_| {}),
        append_entry: Arc::new(|_| panic!("discovery must not persist entries")),
        epoch: SessionEpoch(7),
        scope: ExecutionScope {
            thread_id: ThreadId("test".into()),
            turn_id: TurnId("turn".into()),
        },
        origin: Default::default(),
    }
}
fn call(name: &str, arguments: Value) -> ToolCall {
    ToolCall {
        id: "parent".into(),
        name: name.into(),
        arguments,
        opaque: None,
    }
}
async fn search(registry: &Registry, arguments: Value) -> ToolOutput {
    registry
        .tools()
        .get("tool_search")
        .unwrap()
        .run(call("tool_search", arguments), context())
        .await
        .unwrap()
}
fn text(output: &ToolOutput) -> String {
    output
        .content
        .iter()
        .filter_map(|content| {
            if let Content::Text { text, .. } = content {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn threshold_preserves_direct_definitions_and_unknown_window_uses_twenty_thousand() {
    let mut registry = registry();
    let direct: Vec<_> = registry
        .tools()
        .specs()
        .into_iter()
        .filter(|tool| !matches!(tool.name.as_str(), "call_tool" | "tool_search"))
        .collect();
    assert_eq!(present(&registry, 1000, |_| 100), direct);
    assert_eq!(present(&registry, 0, |_| 20_000), direct);
    for tools in [
        present(&registry, 1000, |_| 101),
        present(&registry, 0, |_| 20_001),
    ] {
        assert_eq!(
            tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["read", "tool_search", "call_tool"]
        );
    }
    assert!(registry.tools().get("mcp__github__addComment").is_some());
    registry.restrict_tools(vec!["mcp__github__addComment".into()]);
    assert_eq!(present(&registry, 1, |_| 1), registry.tools().specs());
}

#[test]
fn catalog_groups_and_sorts_names_and_falls_back_to_counts() {
    let mut registry = registry();
    registry
        .register_tool(fixture("mcp__github__aaa", "MCP server: github", "First"))
        .unwrap();
    let tools = present(&registry, 1, |_| 1);
    assert!(tools[1].description.ends_with("Available through call_tool (3 tools):\nMCP server: github — mcp__github__aaa, mcp__github__addComment\nMCP server: linear — mcp__linear__get_issue"));
    for index in 0..1000 {
        registry
            .register_tool(fixture(
                &format!("mcp__github__tool_{index:04}"),
                "MCP server: github",
                "Fixture",
            ))
            .unwrap();
    }
    let tools = present(&registry, 1, |_| 1);
    assert!(tools[1].description.ends_with("MCP server: github — 1002 tools\nMCP server: linear — 1 tools\nUse query to search these tools."));
    assert!(!tools[1].description.contains("tool_0000"));
}

#[tokio::test]
async fn exact_search_returns_full_definitions_and_separate_missing_and_direct_names() {
    let registry = registry();
    let tools = present(&registry, 1, |_| 1);
    let output = search(
        &registry,
        json!({"names":["mcp__github__addComment", "missing", "read"]}),
    )
    .await;
    assert_eq!(
        output.details,
        Some(
            json!({"found":["mcp__github__addComment"], "visible":["read"], "not_found":["missing"]})
        )
    );
    let spec = registry
        .tools()
        .get("mcp__github__addComment")
        .unwrap()
        .spec()
        .clone();
    assert_eq!(
        text(&output),
        format!(
            "1 tool found; call it with call_tool.\n\nmcp__github__addComment (MCP server: github)\n{}\n{}\n\nNot found: missing\n\nIn your tool list, call directly: read",
            spec.description, spec.input_schema
        )
    );
    assert_eq!(tools, present(&registry, 1, |_| 1));
    eprintln!(
        "tool_search description:\n{}\ncall_tool description:\n{}\nSearch sample:\n{}",
        tools[1].description,
        tools[2].description,
        text(&output)
    );
}

#[tokio::test]
async fn bm25_matches_english_cjk_camel_case_prefix_and_parameter_names_stably() {
    let mut registry = registry();
    registry
        .register_tool(fixture(
            "mcp__github__a_tie",
            "MCP server: github",
            "tie marker",
        ))
        .unwrap();
    registry
        .register_tool(fixture(
            "mcp__github__b_tie",
            "MCP server: github",
            "tie marker",
        ))
        .unwrap();
    present(&registry, 1, |_| 1);
    for query in [
        "comment",
        "合并请求 评论",
        "addComm",
        "mcp__github__addComment",
    ] {
        let output = search(&registry, json!({"query":query})).await;
        assert_eq!(
            output.details.as_ref().unwrap()["found"][0],
            "mcp__github__addComment",
            "{query}"
        );
    }
    for query in ["pullNumber", "identifier"] {
        let output = search(&registry, json!({"query":query})).await;
        assert!(
            output.details.unwrap()["found"]
                .as_array()
                .unwrap()
                .contains(&json!("mcp__github__addComment"))
        );
    }
    let output = search(&registry, json!({"query":"tie marker", "limit":2})).await;
    assert_eq!(
        output.details.unwrap()["found"],
        json!(["mcp__github__a_tie", "mcp__github__b_tie"])
    );
    let output = search(&registry, json!({"query":"no_such_word"})).await;
    assert!(text(&output).contains("No matching tools. Check the list"));
}

#[tokio::test]
async fn query_matching_visible_direct_tool_reports_call_directly_instead_of_no_matching_tools() {
    let mut registry = registry();
    registry
        .register_tool(Arc::new(FixtureTool {
            spec: ToolSpec {
                name: "bash".into(),
                description: "Execute shell commands.".into(),
                input_schema: json!({"type":"object","properties":{"command":{"type":"string"}}}),
            },
            source: "Built-in".into(),
            deferred: false,
        }))
        .unwrap();
    present(&registry, 1, |_| 1);
    let output = search(&registry, json!({"query":"bash"})).await;
    assert_eq!(
        output.details,
        Some(json!({"found":[], "visible":["bash"], "not_found":[]}))
    );
    let rendered = text(&output);
    assert!(!rendered.contains("No matching tools"));
    assert!(rendered.contains("In your tool list, call directly: bash"));

    let output = search(&registry, json!({"query":"read"})).await;
    assert_eq!(output.details.as_ref().unwrap()["visible"], json!(["read"]));
    assert!(text(&output).contains("In your tool list, call directly: read"));
}

#[tokio::test]
async fn state_is_local_to_registration_and_restrictions_apply_to_catalog() {
    let mut first = registry();
    let second = registry();
    present(&first, 1, |_| 1);
    present(&second, 1000, |_| 0);
    assert!(
        text(&search(&second, json!({"query":"comment"})).await)
            .starts_with("All tools are already")
    );
    assert!(text(&search(&first, json!({"query":"comment"})).await).starts_with("1 tool found"));
    first.restrict_tools(vec![
        "call_tool".into(),
        "tool_search".into(),
        "read".into(),
    ]);
    let tools = present(&first, 1, |specs| specs.len() as u64);
    assert_eq!(tools.len(), 1);
    assert!(first.tools().get("mcp__github__addComment").is_none());
    assert!(
        first
            .register_presentation(Arc::new(State::default()))
            .is_err()
    );
}

#[tokio::test]
async fn input_contract_and_generic_truncation() {
    let mut registry = registry();
    present(&registry, 1, |_| 1);
    let tool = registry.tools().get("tool_search").unwrap();
    for arguments in [
        json!({}),
        json!({"names":[]}),
        json!({"names":vec!["x";21]}),
        json!({"names":["x"],"query":"x"}),
        json!({"query":" "}),
        json!({"query":"中".repeat(513)}),
        json!({"query":"x","limit":0}),
        json!({"query":"x","limit":11}),
    ] {
        assert!(
            tool.run(call("tool_search", arguments), context())
                .await
                .is_err()
        );
    }
    registry
        .register_tool(fixture(
            "huge",
            "Fixture",
            &"complete description\n".repeat(4000),
        ))
        .unwrap();
    present(&registry, 1, |_| 1);
    let output = search(&registry, json!({"names":["huge"]})).await;
    let text = text(&output);
    assert!(text.contains("Full output:"));
    let path = text
        .rsplit_once("Full output: ")
        .unwrap()
        .1
        .trim_end_matches(']');
    let full = std::fs::read_to_string(path).unwrap();
    assert!(full.contains(&"complete description\n".repeat(4000)));
    std::fs::remove_file(path).unwrap();
}

struct Invoker {
    registry: Registry,
    calls: Mutex<Vec<ToolCall>>,
    output: ToolOutput,
}
#[async_trait]
impl ToolInvoker for Invoker {
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.registry.tools().get(name)
    }
    async fn invoke(&self, call: ToolCall, ctx: ToolContext) -> ToolOutput {
        (ctx.updates)(ToolUpdate {
            call_id: call.id.clone(),
            epoch: ctx.epoch,
            output: self.output.clone(),
        });
        self.calls.lock().unwrap().push(call);
        if ctx.cancel.is_cancelled() {
            return failure("cancelled".into());
        }
        self.output.clone()
    }
}

#[tokio::test]
async fn delegated_call_preserves_fields_child_identity_updates_defaults_and_cancellation() {
    let output = ToolOutput {
        content: vec![Content::Text {
            text: "echo".into(),
            opaque: None,
        }],
        is_error: true,
        details: Some(json!({"fixture":true})),
        structured_content: Some(json!({"result":42})),
    };
    let invoker = Arc::new(Invoker {
        registry: registry(),
        calls: Mutex::new(vec![]),
        output: output.clone(),
    });
    let mut ctx = context();
    ctx.invoker = Some(invoker.clone());
    let updates = Arc::new(Mutex::new(vec![]));
    let recorded = updates.clone();
    ctx.updates = Arc::new(move |update| recorded.lock().unwrap().push(update));
    let tool = invoker.registry.tools().get("call_tool").unwrap();
    assert_eq!(tool.concurrency(&json!({})), Concurrency::Delegated);
    assert_eq!(
        tool.run(
            call("call_tool", json!({"name":"mcp__github__addComment"})),
            ctx.clone()
        )
        .await
        .unwrap(),
        output
    );
    assert_eq!(invoker.calls.lock().unwrap()[0].id, "parent.1");
    assert_eq!(invoker.calls.lock().unwrap()[0].arguments, json!({}));
    assert_eq!(updates.lock().unwrap()[0].call_id, "parent");
    assert_eq!(updates.lock().unwrap()[0].epoch, SessionEpoch(7));
    ctx.cancel.cancel();
    assert_eq!(
        text(
            &tool
                .run(
                    call("call_tool", json!({"name":"mcp__github__addComment"})),
                    ctx
                )
                .await
                .unwrap()
        ),
        "cancelled"
    );
    eprintln!("Call sample:\n{}", text(&output));
}

#[tokio::test]
async fn unknown_and_nondeferrable_targets_do_not_invoke() {
    let invoker = Arc::new(Invoker {
        registry: registry(),
        calls: Mutex::new(vec![]),
        output: ToolOutput::default(),
    });
    let mut ctx = context();
    ctx.invoker = Some(invoker.clone());
    for (name, message) in [
        (
            "missing",
            "Unknown tool missing; use tool_search to find tools.",
        ),
        ("read", "read is in your tool list; call it directly."),
        (
            "call_tool",
            "call_tool is in your tool list; call it directly.",
        ),
    ] {
        let output = CallTool(call_spec())
            .run(call("call_tool", json!({"name":name})), ctx.clone())
            .await
            .unwrap();
        assert!(output.is_error);
        assert_eq!(text(&output), message);
    }
    assert!(invoker.calls.lock().unwrap().is_empty());
}
