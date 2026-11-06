//! File checkpoints and rewind policy, registered only through plugin-api.

pub mod store;

use std::{
    collections::BTreeMap,
    io::{BufRead, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use store::{Plan, Store};
use tiechui_plugin_api::{
    model::ToolCall,
    protocol::*,
    serde_json::{Value, json},
    *,
};

/// Later control modules can append this entry before changing their own state.
pub const CONTROL_CHECKPOINT: &str = "tiechui.rewind.control";

pub struct RewindModule {
    pub directory: PathBuf,
}
impl RewindModule {
    /// A fresh tracker whose writes belong to the dispatching parent's checkpoint.
    pub fn register_child(&self, registry: &mut Registry, parent: ToolContext) {
        registry.register_tool_interceptor(
            Arc::new(Rewind {
                directory: self.directory.clone(),
                pending: Mutex::new(BTreeMap::new()),
                parent: Some(parent),
            }),
            Duration::from_secs(10),
        );
    }
}
impl Module for RewindModule {
    fn id(&self) -> &str {
        "rewind"
    }
    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        let rewind = Arc::new(Rewind {
            directory: self.directory.clone(),
            pending: Mutex::new(BTreeMap::new()),
            parent: None,
        });
        registry.register_tool_interceptor(rewind.clone(), Duration::from_secs(10));
        registry.register_command(rewind);
        Ok(())
    }
}

struct Rewind {
    parent: Option<ToolContext>,
    directory: PathBuf,
    pending: Mutex<BTreeMap<(String, String), (Store, String)>>,
}

pub fn prompts(entries: &[BranchEntry]) -> Vec<(BranchEntry, Vec<InputContent>)> {
    entries
        .iter()
        .filter_map(|entry| {
            let content = entry.content.clone().or_else(|| {
                let (kind, data) = entry.custom.as_ref()?;
                let text = data["text"].as_str()?;
                (kind == CONTROL_CHECKPOINT && data["version"] == 1 && !text.trim().is_empty())
                    .then(|| vec![InputContent::Text { text: text.into() }])
            })?;
            Some((entry.clone(), content))
        })
        .collect()
}

async fn blocking<T: Send + 'static>(
    cancel: CancellationToken,
    work: impl FnOnce(&CancellationToken) -> Result<T, ToolError> + Send + 'static,
) -> Result<T, ToolError> {
    tokio::task::spawn_blocking(move || {
        store::check(&cancel)?;
        work(&cancel)
    })
    .await
    .map_err(|e| ToolError(e.to_string()))?
}

fn storage(
    base: &Path,
    cwd: &Path,
    id: &str,
    parent: Option<&Path>,
    entries: &[BranchEntry],
    cancel: &CancellationToken,
) -> Result<Store, ToolError> {
    let store = Store::new(base, cwd, id)?;
    if let Some(parent) = parent.filter(|_| !store.directory.join("manifest.json").exists()) {
        let mut header = String::new();
        std::io::BufReader::new(std::fs::File::open(parent)?.take(65536)).read_line(&mut header)?;
        if !header.ends_with('\n') {
            return Err(ToolError(
                "Parent session header is missing or too large.".into(),
            ));
        }
        let header: Value = serde_json::from_str(&header).map_err(|e| ToolError(e.to_string()))?;
        let parent_id = header["id"]
            .as_str()
            .ok_or_else(|| ToolError("Invalid parent session header".into()))?;
        let root = header["cwd"]
            .as_str()
            .ok_or_else(|| ToolError("Invalid parent workspace".into()))?;
        if std::fs::canonicalize(root)? != store.root {
            return Err(ToolError(
                "Parent session belongs to a different workspace.".into(),
            ));
        }
        let source = Store::new(base, &store.root, parent_id)?;
        store.inherit(
            &source,
            &prompts(entries)
                .iter()
                .map(|(entry, _)| entry.id.clone())
                .collect::<Vec<_>>(),
            cancel,
        )?;
    }
    Ok(store)
}

#[async_trait]
impl ToolInterceptor for Rewind {
    fn stage(&self) -> InterceptorStage {
        InterceptorStage::AfterPolicy
    }
    async fn before(&self, call: &ToolCall, ctx: ToolContext) -> Result<BeforeToolCall, ToolError> {
        if !matches!(
            call.name.as_str(),
            "edit" | "write" | "replace" | "insert" | "undo_last_change"
        ) {
            return Ok(BeforeToolCall::Allow);
        }
        let Some(path) = call
            .arguments
            .get("path")
            .or_else(|| call.arguments.get("file_path"))
            .and_then(Value::as_str)
        else {
            return Ok(BeforeToolCall::Allow);
        };
        let path = resolve_tool_path(path, &ctx.cwd)?;
        let directory = self.directory.clone();
        let key = (ctx.scope.thread_id.0.clone(), call.id.clone());
        let ctx = if let Some(parent) = &self.parent {
            ToolContext {
                cwd: parent.cwd.clone(),
                branch: parent.branch.clone(),
                parent_session: parent.parent_session.clone(),
                scope: parent.scope.clone(),
                ..ctx
            }
        } else {
            ctx
        };
        let result = blocking(ctx.cancel.clone(), move |cancel| {
            let store = storage(
                &directory,
                &ctx.cwd,
                &ctx.scope.thread_id.0,
                ctx.parent_session.as_deref(),
                &ctx.branch,
                cancel,
            )?;
            let anchor = prompts(&ctx.branch)
                .last()
                .map(|(entry, _)| entry.id.clone())
                .ok_or_else(|| {
                    ToolError("No user or control-operation checkpoint anchor".into())
                })?;
            let file = store.file_key(&path, &ctx.cwd)?;
            store.before(&anchor, &file, cancel)?;
            Ok((store, file))
        })
        .await;
        match result {
            Ok(entry) => {
                self.pending
                    .lock()
                    .map_err(|e| ToolError(e.to_string()))?
                    .insert(key, entry);
                Ok(BeforeToolCall::Allow)
            }
            Err(error) => Ok(BeforeToolCall::Deny(format!(
                "Checkpoint failed; edit blocked: {error}"
            ))),
        }
    }
    async fn settled(&self, call: &ToolCall, ctx: ToolContext) -> Result<(), ToolError> {
        let key = (ctx.scope.thread_id.0, call.id.clone());
        let entry = self
            .pending
            .lock()
            .map_err(|e| ToolError(e.to_string()))?
            .remove(&key);
        if let Some((store, file)) = entry {
            blocking(ctx.cancel, move |cancel| store.after(&file, cancel)).await?;
        }
        Ok(())
    }
}

fn stats(plan: &Result<Plan, ToolError>) -> CheckpointStats {
    match plan {
        Ok(plan) => plan.stats.clone(),
        Err(error) => CheckpointStats {
            error: Some(error.0.clone()),
            ..Default::default()
        },
    }
}
fn accepted(decision: ApprovalDecision) -> Option<Value> {
    match decision {
        ApprovalDecision::Accept { content } => content,
        _ => None,
    }
}
async fn event(host: &dyn CommandHost, name: &str, data: Value) -> Result<(), ToolError> {
    host.apply(SessionEffect::Emit {
        kind: name.into(),
        data,
    })
    .await
}

#[async_trait]
impl Command for Rewind {
    fn info(&self) -> CommandInfo {
        CommandInfo { name: "rewind".into(), description: "Restore conversation, tracked code, or both; confirm recovery of any interrupted restore".into(), argument_hint: Some("[detach]".into()) }
    }
    fn interactive(&self) -> bool {
        true
    }
    async fn run(&self, _: &str, _: CancellationToken) -> Result<String, ToolError> {
        Err(ToolError("Rewind requires an interactive terminal.".into()))
    }
    async fn session_run(
        &self,
        arguments: &str,
        session: CommandSession,
        cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        match arguments.trim() {
            "detach" => {
                session.host.apply(SessionEffect::DetachImages).await?;
                return Ok(String::new());
            }
            "" => {}
            _ => return Err(ToolError("Usage: /rewind [detach]".into())),
        }
        let mut committed = false;
        let identity = session.branch.session_id.clone();
        event(&*session.host, "rewind.open", json!({"sessionId":identity})).await?;
        let result = self.choose(&session, &mut committed, cancel).await;
        event(
            &*session.host,
            "rewind.closed",
            json!({"sessionId":identity,"committed":committed}),
        )
        .await?;
        result
    }
}

impl Rewind {
    async fn choose(
        &self,
        session: &CommandSession,
        committed: &mut bool,
        cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        let directory = self.directory.clone();
        let branch = session.branch.clone();
        let store = blocking(cancel.clone(), move |cancel| {
            storage(
                &directory,
                &branch.cwd,
                &branch.session_id,
                branch.parent_session.as_deref(),
                &branch.entries,
                cancel,
            )
        })
        .await?;
        let host = &session.host;
        if store.pending()? {
            let panel = RewindPanel {
                rows: vec![],
                selected: 0,
                page: RewindPage::Recovery,
                preview: None,
            };
            if accepted(
                host.ask(ApprovalRequest::Rewind { panel }, cancel.clone())
                    .await?,
            )
            .is_some_and(|value| value["action"] == "recover")
            {
                host.apply(SessionEffect::Validate {
                    session_id: session.branch.session_id.clone(),
                    leaf: session.branch.leaf.clone(),
                })
                .await?;
                let restore = store.clone();
                let result = blocking(cancel, move |cancel| restore.recover(cancel)).await;
                event(
                    &**host,
                    "workspace.files_changed",
                    json!({"sessionId":session.branch.session_id}),
                )
                .await?;
                result?;
                return Ok("Files recovered. Check the current conversation position.".into());
            }
            return Ok(String::new());
        }
        let entries = prompts(&session.branch.entries);
        if entries.is_empty() {
            return Ok("No previous message or control operation to rewind.".into());
        }
        let rows = entries
            .iter()
            .map(|(entry, content)| RewindRow {
                session_id: session.branch.session_id.clone(),
                entry_id: entry.id.clone(),
                timestamp: entry.timestamp,
                text: content
                    .iter()
                    .filter_map(|part| match part {
                        InputContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                stats: None,
            })
            .collect::<Vec<_>>();
        let mut panel = RewindPanel {
            selected: rows.len() - 1,
            rows,
            page: RewindPage::Messages,
            preview: None,
        };
        loop {
            store::check(&cancel)?;
            panel.page = RewindPage::Messages;
            panel.preview = None;
            let response = {
                let response = host.ask(
                    ApprovalRequest::Rewind {
                        panel: panel.clone(),
                    },
                    cancel.clone(),
                );
                tokio::pin!(response);
                let list_cancel = cancel.child_token();
                let summaries = async {
                    let start = panel
                        .selected
                        .saturating_sub(4)
                        .min(panel.rows.len().saturating_sub(8));
                    for index in start..(start + 8).min(panel.rows.len()) {
                        if panel.rows[index].stats.is_some() {
                            continue;
                        }
                        let ids = panel.rows[index..]
                            .iter()
                            .map(|row| row.entry_id.clone())
                            .collect::<Vec<_>>();
                        let store = store.clone();
                        let plan =
                            blocking(list_cancel.clone(), move |cancel| store.plan(&ids, cancel))
                                .await;
                        panel.rows[index].stats = Some(stats(&plan));
                        host.update(ApprovalRequest::Rewind {
                            panel: panel.clone(),
                        })
                        .await?;
                    }
                    std::future::pending::<Result<(), ToolError>>().await
                };
                tokio::pin!(summaries);
                let response = tokio::select! { biased; response = &mut response => response?, result = &mut summaries => { result?; return Ok(String::new()); } };
                list_cancel.cancel();
                response
            };
            let Some(answer) = accepted(response) else {
                return Ok(String::new());
            };
            let Some(index) = entries.iter().position(|(entry, _)| {
                answer["entry_id"] == entry.id && answer["session_id"] == session.branch.session_id
            }) else {
                return Err(ToolError(
                    "The session changed during Rewind. Open it again.".into(),
                ));
            };
            panel.selected = index;
            if answer["action"] == "move" {
                continue;
            }
            if answer["action"] != "preview" {
                return Err(ToolError("Invalid Rewind selection".into()));
            }
            panel.page = RewindPage::Checking;
            let check_cancel = cancel.child_token();
            let answer = host.ask(
                ApprovalRequest::Rewind {
                    panel: panel.clone(),
                },
                check_cancel.clone(),
            );
            tokio::pin!(answer);
            let ids = entries[index..]
                .iter()
                .map(|(entry, _)| entry.id.clone())
                .collect::<Vec<_>>();
            let inspect_store = store.clone();
            let inspection = blocking(check_cancel.clone(), move |cancel| {
                inspect_store.plan(&ids, cancel)
            });
            tokio::pin!(inspection);
            let plan = tokio::select! { biased;
                response = &mut answer => { check_cancel.cancel(); if accepted(response?).is_none() { return Ok(String::new()); } continue; }
                plan = &mut inspection => plan,
            };
            panel.preview = Some(stats(&plan));
            panel.rows[index].stats = panel.preview.clone();
            panel.page = RewindPage::Actions;
            host.update(ApprovalRequest::Rewind {
                panel: panel.clone(),
            })
            .await?;
            let response = answer.await?;
            check_cancel.cancel();
            let Some(answer) = accepted(response) else {
                return Ok(String::new());
            };
            if answer["action"] == "back" {
                continue;
            }
            if answer["entry_id"] != entries[index].0.id
                || answer["session_id"] != session.branch.session_id
            {
                return Err(ToolError(
                    "The session changed during Rewind. Open it again.".into(),
                ));
            }
            let action = answer["action"].as_str().unwrap_or("");
            if !matches!(action, "both" | "conversation" | "code") {
                return Err(ToolError("Invalid Rewind action".into()));
            }
            let code = action != "conversation";
            let conversation = action != "code";
            if code && !plan.as_ref().is_ok_and(|plan| !plan.target.is_empty()) {
                return Err(ToolError("No code restore available".into()));
            }
            store::check(&cancel)?;
            if code {
                host.apply(SessionEffect::Quiesce).await?;
            }
            host.apply(SessionEffect::Validate {
                session_id: session.branch.session_id.clone(),
                leaf: session.branch.leaf.clone(),
            })
            .await?;
            *committed = true;
            event(&**host, "rewind.committed", json!({"sessionId":session.branch.session_id,"entryId":entries[index].0.id,"action":action})).await?;
            let restored = if code {
                let plan = plan.as_ref().map_err(|e| ToolError(e.0.clone()))?.clone();
                let store = store.clone();
                let result =
                    blocking(cancel.clone(), move |cancel| store.restore(&plan, cancel)).await;
                event(
                    &**host,
                    "workspace.files_changed",
                    json!({"sessionId":session.branch.session_id}),
                )
                .await?;
                Some(result.map_err(|error| ToolError(format!("Files: restore failed; conversation: unchanged. {error}. If interrupted, run /rewind to recover.")))?)
            } else {
                None
            };
            if conversation {
                let target = entries[index]
                    .0
                    .parent_id
                    .clone()
                    .unwrap_or_else(|| session.branch.session_id.clone());
                if let Err(error) = host
                    .apply(SessionEffect::Navigate { entry_id: target })
                    .await
                {
                    return Err(ToolError(format!(
                        "Files: {}; conversation: failed. {error}{}",
                        if code { "restored" } else { "unchanged" },
                        if code {
                            "; run /rewind to recover."
                        } else {
                            ""
                        }
                    )));
                }
                host.apply(SessionEffect::Refill {
                    content: entries[index].1.clone(),
                })
                .await
                .map_err(|error| {
                    ToolError(format!(
                        "Files: {}; conversation: moved; composer refill failed: {error}",
                        if code { "restored" } else { "unchanged" }
                    ))
                })?;
            }
            if code {
                store.finish(&plan.map_err(|e| ToolError(e.0))?.target)?;
            }
            drop(restored);
            return Ok(format!(
                "Files: {}. Conversation: {}.",
                if code { "restored" } else { "unchanged" },
                if conversation {
                    "restored; prompt returned unsent"
                } else {
                    "unchanged"
                }
            ));
        }
    }
}
