//! The native directory tool exposed by child agent allowlists.
use super::*;
use tiechui_plugin_api::model::ToolSpec;

pub(super) struct ListTool(pub ToolSpec);
#[async_trait]
impl Tool for ListTool {
    fn programmatic(&self) -> bool {
        true
    }
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Shared
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let path = resolve(call.arguments["path"].as_str().unwrap_or("."), &ctx.cwd)?;
        let limit = call.arguments["limit"].as_u64().unwrap_or(1000) as usize;
        tokio::task::spawn_blocking(move || {
            check(&ctx.cancel)?;
            let mut names = std::collections::BTreeSet::new();
            let mut count = 0;
            for entry in std::fs::read_dir(path)? {
                check(&ctx.cancel)?;
                let entry = entry?;
                let name = format!(
                    "{}{}",
                    entry.file_name().to_string_lossy(),
                    if entry.path().is_dir() { "/" } else { "" }
                );
                names.insert(name);
                count += 1;
                if names.len() > limit {
                    names.pop_last();
                }
            }
            let mut output = tiechui_plugin_api::tools::output::TextHead::new(limit);
            for name in names {
                output.append(&format!("{name}\n"));
            }
            let truncated = output.finish();
            let mut text = truncated.content;
            if text.is_empty() {
                text = "(empty directory)".into();
            }
            if truncated.truncated {
                text.push_str("\n[Output truncated at 50 KiB.]");
            }
            if count > limit {
                text.push_str(&format!(
                    "\n[Showing {limit} of {count} entries. Use limit to see more.]"
                ));
            }
            Ok(ToolOutput::text(text))
        })
        .await
        .map_err(|e| ToolError(e.to_string()))?
    }
}
