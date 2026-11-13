//! File tools with observation and mutation safeguards.

mod edit;
mod image;
mod list;
mod mutation;
mod read;
mod search;
mod state;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tiechui_plugin_api::resolve_tool_path as resolve;

use async_trait::async_trait;
use serde_json::Value;
use tiechui_plugin_api::model::{ToolCall, ToolSpec};
use tiechui_plugin_api::tools::{
    Concurrency, Tool, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use tokio_util::sync::CancellationToken;
use unicode_normalization::UnicodeNormalization;

pub use state::EditingState;

pub struct EditingModule;
impl EditingModule {
    pub fn register_child(
        &self,
        registry: &mut tiechui_plugin_api::Registry,
        config: &tiechui_plugin_api::ModuleConfig,
    ) -> Result<(), ToolError> {
        tiechui_plugin_api::Module::register(self, registry, config)?;
        registry.register_tool(Arc::new(list::ListTool(serde_json::from_value(serde_json::json!({
            "name":"ls", "description":"List directory contents, including hidden entries. Directories end in /. Results are sorted by name.",
            "input_schema":{"type":"object","properties":{"path":{"type":"string"},"limit":{"type":"integer","minimum":1}}}
        })).map_err(|e| ToolError(e.to_string()))?)))
    }
}

impl tiechui_plugin_api::Module for EditingModule {
    fn id(&self) -> &'static str {
        "editing"
    }

    fn register(
        &self,
        registry: &mut tiechui_plugin_api::Registry,
        _: &tiechui_plugin_api::ModuleConfig,
    ) -> Result<(), ToolError> {
        let state = register_tools(|tool| registry.register_tool(tool), true)?;
        registry.register_turn_observer(state.clone());
        registry.register_session_observer(state);
        Ok(())
    }
}

/// One state per session; clear it when context is compacted, switched, or rewound.
pub fn register(registry: &mut ToolRegistry) -> Result<Arc<EditingState>, ToolError> {
    register_with_image_resize(registry, true)
}

/// Host image setting; tool parameters and text remain unchanged.
pub fn register_with_image_resize(
    registry: &mut ToolRegistry,
    auto_resize_images: bool,
) -> Result<Arc<EditingState>, ToolError> {
    register_tools(|tool| registry.register(tool), auto_resize_images)
}

fn register_tools(
    mut register: impl FnMut(Arc<dyn Tool>) -> Result<(), ToolError>,
    auto_resize_images: bool,
) -> Result<Arc<EditingState>, ToolError> {
    let state = Arc::new(EditingState::default());
    let specs: Vec<ToolSpec> =
        serde_json::from_str(include_str!("specs.json")).map_err(|e| ToolError(e.to_string()))?;
    let locks = Arc::new(Mutex::new(HashMap::new()));
    for spec in specs {
        register(Arc::new(FileTool {
            spec,
            state: state.clone(),
            locks: locks.clone(),
            auto_resize_images,
        }))?;
    }
    Ok(state)
}

type FileLocks = Arc<Mutex<HashMap<PathBuf, std::sync::Weak<tokio::sync::Mutex<()>>>>>;
struct FileTool {
    spec: ToolSpec,
    state: Arc<EditingState>,
    locks: FileLocks,
    auto_resize_images: bool,
}

#[async_trait]
impl Tool for FileTool {
    fn programmatic(&self) -> bool {
        true
    }
    fn source(&self) -> Option<String> {
        Some("Built-in tools".into())
    }
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        if matches!(self.spec.name.as_str(), "write" | "edit") {
            Concurrency::Exclusive
        } else {
            Concurrency::Shared
        }
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        check(&ctx.cancel)?;
        match self.spec.name.as_str() {
            "grep" | "find" => search::run(&call.name, call.arguments, &ctx).await,
            "read" => {
                let state = self.state.clone();
                let auto_resize_images = self.auto_resize_images;
                let cancel = ctx.cancel.clone();
                let task = tokio::task::spawn_blocking(move || {
                    read::run(call, ctx, &state, auto_resize_images)
                });
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => Err(ToolError("Operation aborted".into())),
                    result = task => result.map_err(|e| ToolError(e.to_string()))?,
                }
            }
            _ => {
                let path = resolve(string(&call.arguments, "path")?, &ctx.cwd)?;
                let key = std::fs::canonicalize(&path).unwrap_or(path);
                let lock = {
                    let mut locks = self.locks.lock().map_err(|e| ToolError(e.to_string()))?;
                    locks.retain(|_, lock| lock.strong_count() > 0);
                    let lock = locks
                        .get(&key)
                        .and_then(|v| v.upgrade())
                        .unwrap_or_default();
                    locks.insert(key, Arc::downgrade(&lock));
                    lock
                };
                let guard = tokio::select! {
                    biased;
                    _ = ctx.cancel.cancelled() => return Err(ToolError("Operation aborted".into())),
                    guard = lock.lock_owned() => guard,
                };
                let state = self.state.clone();
                tokio::task::spawn_blocking(move || {
                    let _guard = guard;
                    mutation::run(call, ctx, &state)
                })
                .await
                .map_err(|e| ToolError(e.to_string()))?
            }
        }
    }
}

fn check(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError("Operation aborted".into()))
    } else {
        Ok(())
    }
}
fn string<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    input[key]
        .as_str()
        .ok_or_else(|| ToolError(format!("Expected string parameter: {key}")))
}
fn read_path(input: &str, cwd: &Path) -> Result<PathBuf, ToolError> {
    let path = resolve(input, cwd)?;
    if path.exists() {
        return Ok(path);
    }
    let text = path.to_string_lossy();
    let variants = [
        text.replace(" AM.", "\u{202f}AM.")
            .replace(" PM.", "\u{202f}PM."),
        text.nfd().collect(),
        text.replace('\'', "’"),
        text.nfd().collect::<String>().replace('\'', "’"),
    ];
    Ok(variants
        .into_iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .unwrap_or(path))
}

/// Files larger than this get no approval preview.
const PREVIEW_LIMIT: u64 = 256 * 1024;

/// The file a `write` or `edit` call would change, before and after, for approval prompts;
/// `None` when the call would fail, the file is not UTF-8 text or is too large.
pub fn preview(call: &ToolCall, cwd: &Path) -> Option<(String, String)> {
    use std::io::Read;
    let edits = match call.name.as_str() {
        "write" => None,
        "edit" => Some(edit::arguments(&call.arguments).ok()?),
        _ => return None,
    };
    let display = call.arguments["path"].as_str()?;
    let path = resolve(display, cwd).ok()?;
    let mut before = Vec::new();
    match mutation::open_nofollow(std::fs::OpenOptions::new().read(true)).open(&path) {
        Ok(file) => {
            let meta = file.metadata().ok()?;
            if !meta.is_file() || meta.len() > PREVIEW_LIMIT {
                return None;
            }
            file.take(PREVIEW_LIMIT).read_to_end(&mut before).ok()?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && edits.is_none() => {}
        Err(_) => return None,
    }
    let after = match edits {
        None => call.arguments["content"].as_str()?.as_bytes().to_vec(),
        Some(edits) => edit::prepare(&before, edits, display, &[]).ok()?.bytes,
    };
    if after.len() as u64 > PREVIEW_LIMIT {
        return None;
    }
    Some((
        String::from_utf8(before).ok()?,
        String::from_utf8(after).ok()?,
    ))
}

impl tiechui_plugin_api::TurnObserver for EditingState {
    fn observe(&self, event: &tiechui_plugin_api::protocol::Event) {
        if matches!(&event.event, tiechui_plugin_api::protocol::EventKind::Extension { name, .. } if name == "workspace.files_changed")
        {
            self.clear();
        }
    }
}
impl tiechui_plugin_api::SessionObserver for EditingState {
    fn observe(&self, _: &tiechui_plugin_api::SessionEvent) {
        self.clear();
    }
}
