#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rmcp::model::*;
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};
use tiechui_plugin_api::serde_json::{self, json};

#[derive(Clone, Default)]
pub struct TestServer {
    pub delay: Duration,
    pub cancelled: Arc<AtomicUsize>,
    pub tool_count: Option<usize>,
    pub calls: Arc<AtomicUsize>,
}

impl ServerHandler for TestServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
    }

    async fn list_tools(
        &self,
        page: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        tokio::time::sleep(self.delay).await;
        if let Some(count) = self.tool_count {
            return Ok(serde_json::from_value(json!({"tools": (0..count).map(|index| json!({
                "name":format!("tool_{index:04}"),
                "description":"Echo a value for a numbered fixture. Search by count or value.",
                "inputSchema":{"type":"object","properties":{"value":{"type":"string"},"count":{"type":"integer"}},"required":["count"]}
            })).collect::<Vec<_>>()})).unwrap());
        }
        let (names, next) = if page.and_then(|p| p.cursor).is_some() {
            (vec!["a-b", "a_b"], None)
        } else {
            (
                vec![
                    "echo",
                    "error",
                    "wait",
                    "crash",
                    "elicit",
                    "environment",
                    "long",
                ],
                Some("second"),
            )
        };
        Ok(
            serde_json::from_value(json!({"tools":names.into_iter().map(|name| {
            let mut tool = json!({"name":name,"inputSchema":{"type":"object","properties":{}}});
            if matches!(name, "echo" | "error") {
                tool["annotations"] = json!({"readOnlyHint":name == "echo"});
            }
            tool
        }).collect::<Vec<_>>(),"nextCursor":next}))
            .unwrap(),
        )
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = match request.name.as_ref() {
            "error" => {
                json!({"content":[],"isError":true,"structuredContent":{"reason":"fixture"},"_meta":{"ignored":true}})
            }
            "wait" => {
                context.ct.cancelled().await;
                self.cancelled.fetch_add(1, Ordering::SeqCst);
                json!({"content":[{"type":"text","text":"cancelled"}]})
            }
            "crash" => std::process::exit(17),
            "elicit" => {
                let response = context.peer.create_elicitation(serde_json::from_value(json!({"message":"Input?","requestedSchema":{"type":"object","properties":{}}})).unwrap()).await.unwrap();
                json!({"content":[{"type":"text","text":format!("{:?}",response.action)}]})
            }
            "environment" => {
                json!({"content":[{"type":"text","text":json!({"cwd":std::env::current_dir().unwrap(),"explicit":std::env::var("MCP_EXPLICIT").ok(),"forwarded":std::env::var("CARGO").ok(),"unrequested":std::env::var("CARGO_MANIFEST_DIR").ok()}).to_string()}]})
            }
            "long" => json!({"content":[{"type":"text","text":"中".repeat(12_000)}]}),
            _ => {
                json!({"content":[{"type":"text","text":request.arguments.unwrap_or_default().get("value").and_then(|v| v.as_str()).unwrap_or("echo")}]})
            }
        };
        Ok(serde_json::from_value::<CallToolResult>(result)
            .unwrap()
            .into())
    }

    async fn list_resources(
        &self,
        page: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        let second = page.and_then(|p| p.cursor).is_some();
        Ok(serde_json::from_value(if second {
            json!({"resources":[{"uri":"fixture://second","name":"Second"}]})
        } else { json!({"resources":[{"uri":"fixture://first","name":"First","_meta":{"ignored":true},"icons":[]},{"uri":"ui://app","name":"App"},{"uri":"fixture://app","name":"App HTML","mimeType":"text/html; profile=mcp-app"}],"nextCursor":"second"}) }).unwrap())
    }

    async fn list_resource_templates(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, ErrorData> {
        Ok(serde_json::from_value(
            json!({"resourceTemplates":[{"uriTemplate":"fixture://{name}","name":"Fixture"}]}),
        )
        .unwrap())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        Ok(serde_json::from_value::<ReadResourceResult>(json!({"contents":[{"uri":request.uri,"text":"resource body","_meta":{"ignored":true}}]})).unwrap().into())
    }
}
