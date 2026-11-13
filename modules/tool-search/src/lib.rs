//! Stable tool presentation, local discovery and delegated invocation.

mod search;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use tiechui_plugin_api::{
    Concurrency, Module, ModuleConfig, PresentationContext, Registry, Tool, ToolContext, ToolError,
    ToolOutput, ToolPresentation, async_trait,
    model::{Content, ToolCall, ToolSpec},
    serde_json::{Value, json},
    tools::output::OutputAccumulator,
};

const SEARCH_DESCRIPTION: &str = "Some tools are kept out of your tool list to save space; they are listed below. To use one, get its\ndefinition with this tool, then call it with call_tool. Pass {\"names\": [...]} with exact names from the list,\nor {\"query\": \"...\"} to search names, descriptions and parameters (`limit` default 5, max 10). Results\ncontain each tool's full description and JSON parameter schema. Finding a tool grants no permission.";
const CALL_DESCRIPTION: &str = "Call a tool from tool_search's list by its exact name, with `arguments` matching the parameter schema that\ntool_search returned. The call is permission-checked, executed and reported exactly as if the tool were in\nyour tool list. Tools that are in your tool list must be called directly.";

pub struct ToolSearchModule;

impl Module for ToolSearchModule {
    fn id(&self) -> &str {
        "tool-search"
    }

    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        let state = Arc::new(State::default());
        registry.register_presentation(state.clone())?;
        registry.register_tool(Arc::new(SearchTool {
            spec: search_spec(),
            state,
        }))?;
        registry.register_tool(Arc::new(CallTool(call_spec())))
    }
}

#[derive(Clone)]
struct Candidate {
    tool: Arc<dyn Tool>,
    source: String,
}

#[derive(Clone, Default)]
struct Snapshot {
    enabled: bool,
    candidates: Vec<Candidate>,
    visible: Vec<ToolSpec>,
}

#[derive(Default)]
struct State(RwLock<Snapshot>);

impl ToolPresentation for State {
    fn present(&self, ctx: PresentationContext) -> Vec<ToolSpec> {
        let candidates: Vec<_> = ctx
            .tools
            .iter()
            .filter(|tool| tool.deferrable())
            .map(|tool| Candidate {
                source: tool.source().unwrap_or_else(|| "Tools".into()),
                tool: tool.clone(),
            })
            .collect();
        let specs: Vec<_> = candidates
            .iter()
            .map(|candidate| candidate.tool.spec().clone())
            .collect();
        let threshold = if ctx.window == 0 {
            20_000
        } else {
            ctx.window / 10
        };
        let enabled = (ctx.estimate_tools)(&specs) > threshold
            && ["tool_search", "call_tool"]
                .iter()
                .all(|name| ctx.tools.iter().any(|tool| tool.spec().name == *name));
        let mut visible: Vec<_> = ctx
            .tools
            .iter()
            .filter(|tool| !matches!(tool.spec().name.as_str(), "tool_search" | "call_tool"))
            .filter(|tool| !enabled || !tool.deferrable())
            .map(|tool| tool.spec().clone())
            .collect();
        if enabled {
            let mut search = search_spec();
            search.description.push_str(&catalog(&candidates));
            visible.extend([search, call_spec()]);
        }
        *self.0.write().unwrap_or_else(|error| error.into_inner()) = Snapshot {
            enabled,
            candidates,
            visible: visible.clone(),
        };
        visible
    }
}

fn catalog(candidates: &[Candidate]) -> String {
    let mut groups: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for candidate in candidates {
        groups
            .entry(&candidate.source)
            .or_default()
            .push(&candidate.tool.spec().name);
    }
    for names in groups.values_mut() {
        names.sort_unstable();
    }
    let header = format!(
        "\n\nAvailable through call_tool ({} tools):\n",
        candidates.len()
    );
    let names = groups
        .iter()
        .map(|(source, names)| format!("{source} — {}", names.join(", ")))
        .collect::<Vec<_>>()
        .join("\n");
    if header.len() + names.len() <= 12 * 1024 {
        return header + &names;
    }
    let counts = groups
        .iter()
        .map(|(source, names)| format!("{source} — {} tools", names.len()))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{header}{counts}\nUse query to search these tools.")
}

fn search_spec() -> ToolSpec {
    ToolSpec {
        name: "tool_search".into(),
        description: SEARCH_DESCRIPTION.into(),
        input_schema: json!({
            "type":"object", "properties": {
                "names":{"type":"array", "items":{"type":"string"}, "minItems":1, "maxItems":20},
                "query":{"type":"string", "minLength":1, "maxLength":512},
                "limit":{"type":"integer", "minimum":1, "maximum":10, "default":5}
            },
            "additionalProperties":false
        }),
    }
}

fn call_spec() -> ToolSpec {
    ToolSpec {
        name: "call_tool".into(),
        description: CALL_DESCRIPTION.into(),
        input_schema: json!({
            "type":"object", "properties":{
                "name":{"type":"string"}, "arguments":{"type":"object"}
            }, "required":["name"], "additionalProperties":false
        }),
    }
}

struct SearchTool {
    spec: ToolSpec,
    state: Arc<State>,
}

#[async_trait]
impl Tool for SearchTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Shared
    }

    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError("Operation aborted".into()));
        }
        let snapshot = self
            .state
            .0
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if !snapshot.enabled {
            return Ok(ToolOutput {
                details: Some(json!({"found":[], "visible":[], "not_found":[]})),
                ..ToolOutput::text("All tools are already in your tool list; call them directly.")
            });
        }
        let mut found = Vec::new();
        let mut visible = Vec::new();
        let mut not_found = Vec::new();
        match (call.arguments.get("names"), call.arguments.get("query")) {
            (Some(Value::Array(names)), None) if (1..=20).contains(&names.len()) => {
                for name in names {
                    let name = name
                        .as_str()
                        .ok_or_else(|| ToolError("names must contain strings".into()))?;
                    if let Some(candidate) = snapshot
                        .candidates
                        .iter()
                        .find(|candidate| candidate.tool.spec().name == name)
                    {
                        if !found.contains(&candidate.tool.spec().name) {
                            found.push(name.to_owned());
                        }
                    } else if snapshot.visible.iter().any(|visible| visible.name == name) {
                        visible.push(name.to_owned());
                    } else {
                        not_found.push(name.to_owned());
                    }
                }
            }
            (None, Some(Value::String(query)))
                if !query.trim().is_empty() && query.chars().count() <= 512 =>
            {
                let limit = call
                    .arguments
                    .get("limit")
                    .map_or(Some(5), Value::as_u64)
                    .filter(|limit| (1..=10).contains(limit))
                    .ok_or_else(|| ToolError("limit must be between 1 and 10".into()))?;
                let deferred: Vec<_> = snapshot
                    .candidates
                    .iter()
                    .map(|candidate| candidate.tool.spec())
                    .collect();
                let direct: Vec<_> = snapshot
                    .visible
                    .iter()
                    .filter(|spec| !matches!(spec.name.as_str(), "tool_search" | "call_tool"))
                    .collect();
                found = search::rank(&deferred, query, limit as usize, &ctx.cancel)?;
                visible = search::rank(&direct, query, limit as usize, &ctx.cancel)?;
            }
            _ => {
                return Err(ToolError(
                    "Pass exactly one of names (1–20 names) or query (1–512 nonblank characters)."
                        .into(),
                ));
            }
        }
        let mut text = match found.len() {
            1 => "1 tool found; call it with call_tool.".to_owned(),
            count => format!("{count} tools found; call them with call_tool."),
        };
        if found.is_empty() && visible.is_empty() && call.arguments.get("query").is_some() {
            text.push_str("\nNo matching tools. Check the list in tool_search's description and try another query.");
        }
        for name in &found {
            if let Some(candidate) = snapshot
                .candidates
                .iter()
                .find(|candidate| candidate.tool.spec().name == *name)
            {
                let spec = candidate.tool.spec();
                text.push_str(&format!(
                    "\n\n{} ({})\n{}\n{}",
                    spec.name, candidate.source, spec.description, spec.input_schema
                ));
            }
        }
        if !not_found.is_empty() {
            text.push_str(&format!("\n\nNot found: {}", not_found.join(", ")));
        }
        if !visible.is_empty() {
            text.push_str(&format!(
                "\n\nIn your tool list, call directly: {}",
                visible.join(", ")
            ));
        }
        let mut output = OutputAccumulator::new();
        output.append(text.as_bytes())?;
        output.finish()?;
        Ok(ToolOutput {
            details: Some(json!({"found":found, "visible":visible, "not_found":not_found})),
            ..ToolOutput::text(output.snapshot().text(""))
        })
    }
}

struct CallTool(ToolSpec);

#[async_trait]
impl Tool for CallTool {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Delegated
    }

    async fn run(&self, call: ToolCall, mut ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let invoker = ctx
            .invoker
            .clone()
            .ok_or_else(|| ToolError("Tool invocation is unavailable.".into()))?;
        let name = call.arguments["name"]
            .as_str()
            .ok_or_else(|| ToolError("name must be a string".into()))?;
        let Some(tool) = invoker.get(name) else {
            return Ok(failure(format!(
                "Unknown tool {name}; use tool_search to find tools."
            )));
        };
        if !tool.deferrable() {
            return Ok(failure(format!(
                "{name} is in your tool list; call it directly."
            )));
        }
        let arguments = call
            .arguments
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if !arguments.is_object() {
            return Err(ToolError("arguments must be an object".into()));
        }
        let child = ToolCall {
            id: format!("{}.1", call.id),
            name: name.into(),
            arguments,
            opaque: None,
        };
        let updates = ctx.updates.clone();
        ctx.updates = Arc::new(move |mut update| {
            update.call_id = call.id.clone();
            updates(update);
        });
        let mut output = invoker.invoke(child, ctx).await;
        if output.is_error {
            for content in &mut output.content {
                if let Content::Text { text, .. } = content
                    && text.starts_with(&format!("Validation failed for tool \"{name}\":"))
                {
                    text.push_str(&format!("\n\nParameters:\n{}", tool.spec().input_schema));
                    break;
                }
            }
        }
        Ok(output)
    }
}

fn failure(text: String) -> ToolOutput {
    ToolOutput {
        is_error: true,
        ..ToolOutput::text(text)
    }
}

#[cfg(test)]
mod tests;
