//! File checkpoints and rewind policy, registered only through plugin-api.

mod snapshot;
pub mod store;

pub use snapshot::{
    SnapshotOptions, end_anchor, record_turn_snapshot, tiechui_home_from_rewind_dir,
};

use std::{
    collections::BTreeMap,
    io::{BufRead, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use store::{Plan, SnapshotRecord, Store};
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
    pub fn register_with_options(
        &self,
        registry: &mut Registry,
        _: &ModuleConfig,
        options: SnapshotOptions,
    ) -> Result<(), ToolError> {
        let rewind = Arc::new(Rewind {
            directory: self.directory.clone(),
            pending: Mutex::new(BTreeMap::new()),
            parent: None,
            snapshot_options: options,
        });
        registry.register_lifecycle_interceptor(rewind.clone());
        registry.register_tool_interceptor(rewind.clone(), Duration::from_secs(10));
        registry.register_command(rewind);
        Ok(())
    }

    /// A fresh tracker whose writes belong to the dispatching parent's checkpoint.
    pub fn register_child(&self, registry: &mut Registry, parent: ToolContext) {
        registry.register_tool_interceptor(
            Arc::new(Rewind {
                directory: self.directory.clone(),
                pending: Mutex::new(BTreeMap::new()),
                parent: Some(parent),
                snapshot_options: SnapshotOptions::default(),
            }),
            Duration::from_secs(10),
        );
    }
}
impl Module for RewindModule {
    fn id(&self) -> &str {
        "rewind"
    }
    fn register(&self, registry: &mut Registry, config: &ModuleConfig) -> Result<(), ToolError> {
        self.register_with_options(registry, config, SnapshotOptions::default())
    }
}

struct Rewind {
    parent: Option<ToolContext>,
    directory: PathBuf,
    pending: Mutex<BTreeMap<(String, String), (Store, String)>>,
    snapshot_options: SnapshotOptions,
}

#[async_trait]
impl LifecycleInterceptor for Rewind {
    async fn on_event(
        &self,
        event: &LifecycleEvent,
        ctx: &LifecycleContext,
        cancel: CancellationToken,
    ) -> Result<LifecycleOutcome, ToolError> {
        if matches!(event, LifecycleEvent::BeforeRequest) && self.parent.is_none() {
            snapshot::capture_before_request(&self.directory, &self.snapshot_options, ctx, cancel)
                .await;
        }
        Ok(LifecycleOutcome::default())
    }
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

/// A session's store, which inherits the parent session's checkpoints on first use.
pub fn storage(
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
        CommandInfo { name: "rewind".into(), description: "Restore conversation, tracked code, or both; confirm recovery of any interrupted restore".into(), argument_hint: Some("[detach]".into()), kind: Default::default() }
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

#[derive(Clone)]
enum TargetRestore {
    Snapshot {
        tree: String,
        stats: CheckpointStats,
    },
    Patch {
        plan: Result<Plan, ToolError>,
        fallback: Option<String>,
    },
    Blocked(String),
}

impl TargetRestore {
    fn stats(&self) -> CheckpointStats {
        match self {
            Self::Snapshot { stats, .. } => stats.clone(),
            Self::Patch { plan, .. } => stats(plan),
            Self::Blocked(reason) => CheckpointStats {
                files: BTreeMap::new(),
                error: Some(reason.clone()),
            },
        }
    }

    fn require_code_available(&self) -> Result<(), ToolError> {
        match self {
            Self::Blocked(reason) => Err(ToolError(reason.clone())),
            Self::Snapshot { stats, .. } if !stats.files.is_empty() => Ok(()),
            Self::Patch { plan: Ok(plan), .. } if !plan.target.is_empty() => Ok(()),
            _ => Err(ToolError("No code restore available".into())),
        }
    }

    fn fallback_reason(&self) -> Option<&str> {
        match self {
            Self::Patch { fallback, .. } => fallback.as_deref(),
            _ => None,
        }
    }
}

#[derive(Clone)]
struct InspectEnv {
    home: Option<PathBuf>,
    options: SnapshotOptions,
    cwd: PathBuf,
    session_id: String,
    store: Store,
}

fn inspect_restore(
    env: &InspectEnv,
    ids: &[String],
    cancel: &CancellationToken,
) -> Result<TargetRestore, ToolError> {
    store::check(cancel)?;
    let Some(home) = env
        .home
        .as_deref()
        .filter(|h| tiechui_workspace::session_worktree(h, &env.cwd).is_some())
    else {
        return Ok(TargetRestore::Patch {
            plan: env.store.plan(ids, cancel),
            fallback: None,
        });
    };
    match tiechui_workspace::check_worktree_isolation(home, &env.cwd, &env.session_id, &[]) {
        Ok(Some(conflict)) => return Ok(TargetRestore::Blocked(conflict.reason)),
        Err(err) => return Ok(TargetRestore::Blocked(err.0)),
        Ok(None) => {}
    }
    match inspect_worktree_snapshot(env, ids, cancel)? {
        Ok(snapshot) => Ok(snapshot),
        Err(reason) => Ok(TargetRestore::Patch {
            plan: env.store.plan(ids, cancel),
            fallback: Some(reason),
        }),
    }
}

fn inspect_worktree_snapshot(
    env: &InspectEnv,
    ids: &[String],
    cancel: &CancellationToken,
) -> Result<Result<TargetRestore, String>, ToolError> {
    let Some(anchor) = ids.first() else {
        return Ok(Err("no checkpoint anchor for this turn".into()));
    };
    match env.store.snapshot(anchor)? {
        Some(SnapshotRecord::Ready { tree, .. }) => match diff_snapshot_paths(env, &tree, cancel) {
            Ok(diff_paths) => {
                let files = diff_paths.into_iter().map(|path| (path, None)).collect();
                Ok(Ok(TargetRestore::Snapshot {
                    tree,
                    stats: CheckpointStats { files, error: None },
                }))
            }
            Err(err) => Ok(Err(err.0)),
        },
        Some(SnapshotRecord::Missing { reason }) => Ok(Err(reason)),
        None => Ok(Err("no worktree snapshot recorded for this turn".into())),
    }
}

fn diff_snapshot_paths(
    env: &InspectEnv,
    tree: &str,
    cancel: &CancellationToken,
) -> Result<Vec<String>, tiechui_workspace::Error> {
    let is_cancelled = || cancel.is_cancelled();
    tiechui_workspace::diff_worktree_snapshot_with_env(
        &env.cwd,
        tree,
        env.options.timeout,
        &env.options.env,
        Some(&is_cancelled),
    )
}

fn build_rewind_rows(
    session_id: &str,
    entries: &[(BranchEntry, Vec<InputContent>)],
) -> Vec<RewindRow> {
    entries
        .iter()
        .map(|(entry, content)| RewindRow {
            session_id: session_id.to_owned(),
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
        .collect()
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
        if store.pending()? {
            return self.recover_pending(session, &store, cancel).await;
        }
        let entries = prompts(&session.branch.entries);
        if entries.is_empty() {
            return Ok("No previous message or control operation to rewind.".into());
        }
        let inspect_env = InspectEnv {
            home: tiechui_home_from_rewind_dir(&self.directory),
            options: self.snapshot_options.clone(),
            cwd: session.branch.cwd.clone(),
            session_id: session.branch.session_id.clone(),
            store,
        };
        self.selection_loop(session, &entries, &inspect_env, committed, cancel)
            .await
    }

    async fn recover_pending(
        &self,
        session: &CommandSession,
        store: &Store,
        cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        let host = &session.host;
        let panel = RewindPanel {
            rows: vec![],
            selected: 0,
            page: RewindPage::Recovery,
            preview: None,
        };
        if !accepted(
            host.ask(ApprovalRequest::Rewind { panel }, cancel.clone())
                .await?,
        )
        .is_some_and(|value| value["action"] == "recover")
        {
            return Ok(String::new());
        }
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
        Ok("Files recovered. Check the current conversation position.".into())
    }

    async fn selection_loop(
        &self,
        session: &CommandSession,
        entries: &[(BranchEntry, Vec<InputContent>)],
        inspect_env: &InspectEnv,
        committed: &mut bool,
        cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        let rows = build_rewind_rows(&session.branch.session_id, entries);
        let mut panel = RewindPanel {
            selected: rows.len() - 1,
            rows,
            page: RewindPage::Messages,
            preview: None,
        };
        loop {
            let Some(index) = self
                .select_preview_index(session, entries, &mut panel, inspect_env, &cancel)
                .await?
            else {
                return Ok(String::new());
            };
            let ids: Vec<String> = entries[index..]
                .iter()
                .map(|(entry, _)| entry.id.clone())
                .collect();
            let Some((action_answer, target_restore)) = self
                .preview_and_ask_action(
                    &session.host,
                    &mut panel,
                    index,
                    inspect_env,
                    &ids,
                    &cancel,
                )
                .await?
            else {
                return Ok(String::new());
            };
            if action_answer["action"] == "back" {
                continue;
            }
            let ctx = ActionContext {
                session,
                target_entry: &entries[index],
                ids: &ids,
                inspect_env,
                target_restore: &target_restore,
                answer: &action_answer,
                cancel: &cancel,
            };
            return execute_action(ctx, committed).await;
        }
    }

    async fn select_preview_index(
        &self,
        session: &CommandSession,
        entries: &[(BranchEntry, Vec<InputContent>)],
        panel: &mut RewindPanel,
        inspect_env: &InspectEnv,
        cancel: &CancellationToken,
    ) -> Result<Option<usize>, ToolError> {
        loop {
            store::check(cancel)?;
            panel.page = RewindPage::Messages;
            panel.preview = None;
            let Some(answer) = self
                .ask_with_row_summaries(&session.host, panel, inspect_env, cancel)
                .await?
            else {
                return Ok(None);
            };
            let index = locate_selected_entry(entries, &session.branch.session_id, &answer)?;
            panel.selected = index;
            match answer["action"].as_str() {
                Some("move") => continue,
                Some("preview") => return Ok(Some(index)),
                _ => return Err(ToolError("Invalid Rewind selection".into())),
            }
        }
    }

    async fn ask_with_row_summaries(
        &self,
        host: &Arc<dyn CommandHost>,
        panel: &mut RewindPanel,
        inspect_env: &InspectEnv,
        cancel: &CancellationToken,
    ) -> Result<Option<Value>, ToolError> {
        let response = host.ask(
            ApprovalRequest::Rewind {
                panel: panel.clone(),
            },
            cancel.clone(),
        );
        tokio::pin!(response);
        let list_cancel = cancel.child_token();
        let summaries = populate_row_summaries(host, panel, inspect_env, list_cancel.clone());
        tokio::pin!(summaries);
        let decision = tokio::select! {
            biased;
            res = &mut response => res?,
            res = &mut summaries => { res?; return Ok(None); }
        };
        list_cancel.cancel();
        Ok(accepted(decision))
    }

    async fn preview_and_ask_action(
        &self,
        host: &Arc<dyn CommandHost>,
        panel: &mut RewindPanel,
        index: usize,
        inspect_env: &InspectEnv,
        ids: &[String],
        cancel: &CancellationToken,
    ) -> Result<Option<(Value, TargetRestore)>, ToolError> {
        panel.page = RewindPage::Checking;
        let check_cancel = cancel.child_token();
        let answer = host.ask(
            ApprovalRequest::Rewind {
                panel: panel.clone(),
            },
            check_cancel.clone(),
        );
        tokio::pin!(answer);
        let env_cloned = inspect_env.clone();
        let ids_cloned = ids.to_vec();
        let inspection = blocking(check_cancel.clone(), move |c| {
            inspect_restore(&env_cloned, &ids_cloned, c)
        });
        tokio::pin!(inspection);
        let target_restore = tokio::select! {
            biased;
            res = &mut answer => {
                check_cancel.cancel();
                if accepted(res?).is_none() { return Ok(None); }
                return Ok(Some((json!({"action": "back"}), TargetRestore::Blocked(String::new()))));
            }
            inspected = &mut inspection => inspected?,
        };
        panel.preview = Some(target_restore.stats());
        panel.rows[index].stats = panel.preview.clone();
        panel.page = RewindPage::Actions;
        host.update(ApprovalRequest::Rewind {
            panel: panel.clone(),
        })
        .await?;
        let decision = answer.await?;
        check_cancel.cancel();
        Ok(accepted(decision).map(|ans| (ans, target_restore)))
    }
}

struct ActionContext<'a> {
    session: &'a CommandSession,
    target_entry: &'a (BranchEntry, Vec<InputContent>),
    ids: &'a [String],
    inspect_env: &'a InspectEnv,
    target_restore: &'a TargetRestore,
    answer: &'a Value,
    cancel: &'a CancellationToken,
}

async fn execute_action(ctx: ActionContext<'_>, committed: &mut bool) -> Result<String, ToolError> {
    let (code, conversation, action) = validate_before_commit(&ctx).await?;
    *committed = true;
    event(
        &*ctx.session.host,
        "rewind.committed",
        json!({"sessionId":ctx.session.branch.session_id,"entryId":ctx.target_entry.0.id,"action":action}),
    )
    .await?;
    commit_and_apply(&ctx, code, conversation).await?;
    Ok(format_completion_message(
        code,
        conversation,
        ctx.target_restore.fallback_reason(),
    ))
}

async fn validate_before_commit<'a>(
    ctx: &ActionContext<'a>,
) -> Result<(bool, bool, &'a str), ToolError> {
    if ctx.answer["entry_id"] != ctx.target_entry.0.id
        || ctx.answer["session_id"] != ctx.session.branch.session_id
    {
        return Err(ToolError(
            "The session changed during Rewind. Open it again.".into(),
        ));
    }
    let (code, conversation, action) = parse_rewind_action(ctx.answer)?;
    store::check(ctx.cancel)?;
    if code {
        prepare_code_action(ctx).await?;
    } else {
        validate_branch_unchanged(ctx.session).await?;
    }
    Ok((code, conversation, action))
}

async fn prepare_code_action(ctx: &ActionContext<'_>) -> Result<(), ToolError> {
    ctx.target_restore.require_code_available()?;
    ctx.session.host.apply(SessionEffect::Quiesce).await?;
    validate_branch_unchanged(ctx.session).await?;
    precheck_code_restore(ctx.inspect_env, ctx.target_restore, ctx.cancel.clone()).await
}

async fn validate_branch_unchanged(session: &CommandSession) -> Result<(), ToolError> {
    session
        .host
        .apply(SessionEffect::Validate {
            session_id: session.branch.session_id.clone(),
            leaf: session.branch.leaf.clone(),
        })
        .await
}

async fn commit_and_apply(
    ctx: &ActionContext<'_>,
    code: bool,
    conversation: bool,
) -> Result<(), ToolError> {
    if conversation {
        navigate_conversation(
            &ctx.session.host,
            &ctx.session.branch.session_id,
            &ctx.target_entry.0,
        )
        .await?;
    }
    if code {
        apply_code_restore(
            &ctx.session.host,
            ctx.inspect_env,
            ctx.target_restore,
            ctx.ids,
            conversation,
            ctx.cancel.clone(),
        )
        .await?;
    }
    if conversation {
        refill_composer(&ctx.session.host, &ctx.target_entry.1, code).await?;
    }
    Ok(())
}

async fn populate_row_summaries(
    host: &Arc<dyn CommandHost>,
    panel: &mut RewindPanel,
    inspect_env: &InspectEnv,
    list_cancel: CancellationToken,
) -> Result<(), ToolError> {
    let start = panel
        .selected
        .saturating_sub(4)
        .min(panel.rows.len().saturating_sub(8));
    for index in start..(start + 8).min(panel.rows.len()) {
        if panel.rows[index].stats.is_some() {
            continue;
        }
        let ids: Vec<String> = panel.rows[index..]
            .iter()
            .map(|row| row.entry_id.clone())
            .collect();
        let env_cloned = inspect_env.clone();
        let inspected = blocking(list_cancel.clone(), move |c| {
            inspect_restore(&env_cloned, &ids, c)
        })
        .await?;
        panel.rows[index].stats = Some(inspected.stats());
        host.update(ApprovalRequest::Rewind {
            panel: panel.clone(),
        })
        .await?;
    }
    std::future::pending::<Result<(), ToolError>>().await
}

fn locate_selected_entry(
    entries: &[(BranchEntry, Vec<InputContent>)],
    session_id: &str,
    answer: &Value,
) -> Result<usize, ToolError> {
    entries
        .iter()
        .position(|(entry, _)| answer["entry_id"] == entry.id && answer["session_id"] == session_id)
        .ok_or_else(|| ToolError("The session changed during Rewind. Open it again.".into()))
}

fn parse_rewind_action(answer: &Value) -> Result<(bool, bool, &str), ToolError> {
    let action = answer["action"].as_str().unwrap_or("");
    match action {
        "both" => Ok((true, true, action)),
        "conversation" => Ok((false, true, action)),
        "code" => Ok((true, false, action)),
        _ => Err(ToolError("Invalid Rewind action".into())),
    }
}

async fn precheck_code_restore(
    inspect_env: &InspectEnv,
    target_restore: &TargetRestore,
    cancel: CancellationToken,
) -> Result<(), ToolError> {
    let env = inspect_env.clone();
    let restore = target_restore.clone();
    blocking(cancel, move |c| match &restore {
        TargetRestore::Blocked(reason) => Err(ToolError(reason.clone())),
        TargetRestore::Snapshot { tree, .. } => {
            if let Some(home) = &env.home
                && let Some(conflict) = tiechui_workspace::check_worktree_isolation(
                    home,
                    &env.cwd,
                    &env.session_id,
                    &[],
                )
                .map_err(|e| ToolError(e.0))?
            {
                return Err(ToolError(conflict.reason));
            }
            if !tiechui_workspace::has_worktree_snapshot_tree(&env.cwd, tree, &env.options.env) {
                return Err(ToolError(format!("Snapshot tree {tree} is missing")));
            }
            env.store.ensure_ready_for_restore(c)
        }
        TargetRestore::Patch { plan, .. } => {
            let plan = plan.as_ref().map_err(|e| ToolError(e.0.clone()))?;
            env.store.precheck_plan(plan, c)
        }
    })
    .await
}

async fn navigate_conversation(
    host: &Arc<dyn CommandHost>,
    session_id: &str,
    entry: &BranchEntry,
) -> Result<(), ToolError> {
    let target = entry
        .parent_id
        .clone()
        .unwrap_or_else(|| session_id.to_owned());
    host.apply(SessionEffect::Navigate { entry_id: target })
        .await
        .map_err(|error| ToolError(format!("Files: unchanged; conversation: failed. {error}")))
}

async fn apply_code_restore(
    host: &Arc<dyn CommandHost>,
    inspect_env: &InspectEnv,
    target_restore: &TargetRestore,
    ids: &[String],
    conversation_moved: bool,
    cancel: CancellationToken,
) -> Result<(), ToolError> {
    let env = inspect_env.clone();
    let restore = target_restore.clone();
    let discarded = ids.to_vec();
    let conv_state = if conversation_moved {
        "moved"
    } else {
        "unchanged"
    };
    let result = blocking(cancel, move |c| match &restore {
        TargetRestore::Snapshot { tree, .. } => {
            let is_cancelled = || c.is_cancelled();
            tiechui_workspace::restore_worktree_snapshot_cancellable(
                &env.cwd,
                tree,
                env.options.timeout,
                &env.options.env,
                Some(&is_cancelled),
            )
            .map_err(|e| ToolError(e.0))?;
            env.store.settle_snapshot_restore(&discarded, c)
        }
        TargetRestore::Patch { plan, .. } => {
            let plan = plan.as_ref().map_err(|e| ToolError(e.0.clone()))?;
            let lock = env.store.restore(plan, c)?;
            env.store.finish(&plan.target)?;
            drop(lock);
            Ok(())
        }
        TargetRestore::Blocked(reason) => Err(ToolError(reason.clone())),
    })
    .await;
    event(
        &**host,
        "workspace.files_changed",
        json!({"sessionId":inspect_env.session_id}),
    )
    .await?;
    result.map_err(|error| {
        ToolError(format!(
            "Files: restore failed; conversation: {conv_state}. {error}. If interrupted, run /rewind to recover."
        ))
    })
}

async fn refill_composer(
    host: &Arc<dyn CommandHost>,
    content: &[InputContent],
    code_restored: bool,
) -> Result<(), ToolError> {
    host.apply(SessionEffect::Refill {
        content: content.to_vec(),
    })
    .await
    .map_err(|error| {
        ToolError(format!(
            "Files: {}; conversation: moved; composer refill failed: {error}",
            if code_restored {
                "restored"
            } else {
                "unchanged"
            }
        ))
    })
}

fn format_completion_message(code: bool, conversation: bool, fallback: Option<&str>) -> String {
    let base = format!(
        "Files: {}. Conversation: {}.",
        if code { "restored" } else { "unchanged" },
        if conversation {
            "restored; prompt returned unsent"
        } else {
            "unchanged"
        }
    );
    match fallback.filter(|_| code) {
        Some(reason) => format!("{base} Used patch rewind ({reason})."),
        None => base,
    }
}
