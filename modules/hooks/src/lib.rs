//! Command hooks using Codex's hooks.json and stdin/stdout contract.

mod config;
mod output;
mod runner;

pub use config::{Hook, approve, trust, trust_with_sources};
pub use output::Outcome;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{StreamExt, stream::FuturesUnordered};
use serde::Serialize;
use tiechui_plugin_api::protocol::CommandInfo;
use tiechui_plugin_api::{
    ApprovalInterceptor, ApprovalVerdict, BeforeToolCall, CancellationToken, Command,
    LifecycleContext, LifecycleEvent, LifecycleInterceptor, LifecycleOutcome, Module, ModuleConfig,
    Registry, ToolContext, ToolError, ToolInterceptor, ToolOutput, async_trait,
    model::{Content, ToolCall},
    protocol::InputContent,
    serde_json::{self, Value, json},
    tools::CustomSessionEntry,
};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PermissionRequest,
    PreCompact,
    Stop,
    SessionEnd,
}

impl Event {
    pub fn wire(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::PreCompact => "PreCompact",
            Self::Stop => "Stop",
            Self::SessionEnd => "SessionEnd",
        }
    }
    fn from_wire(name: &str) -> Option<Self> {
        [
            Self::SessionStart,
            Self::UserPromptSubmit,
            Self::PreToolUse,
            Self::PostToolUse,
            Self::PermissionRequest,
            Self::PreCompact,
            Self::Stop,
            Self::SessionEnd,
        ]
        .into_iter()
        .find(|event| event.wire() == name)
    }
}

pub struct HooksModule {
    runtime: Arc<Runtime>,
    pub diagnostics: Vec<String>,
}

struct Runtime {
    hooks: Vec<Hook>,
    permission_mode: tiechui_plugin_api::SharedPermissionMode,
    context: Mutex<Option<LifecycleContext>>,
    tasks: Mutex<JoinSet<Outcome>>,
    limit: Arc<Semaphore>,
    shutdown: CancellationToken,
    pending: Mutex<Outcome>,
}

impl HooksModule {
    pub fn load(
        cwd: &Path,
        home: &Path,
        trusted: bool,
        permission_mode: tiechui_plugin_api::SharedPermissionMode,
        cancel: &CancellationToken,
    ) -> Result<Self, ToolError> {
        Self::load_with_sources(cwd, home, trusted, permission_mode, &[], cancel)
    }

    pub fn load_with_sources(
        cwd: &Path,
        home: &Path,
        trusted: bool,
        permission_mode: tiechui_plugin_api::SharedPermissionMode,
        sources: &[Arc<dyn tiechui_plugin_api::HookSource>],
        cancel: &CancellationToken,
    ) -> Result<Self, ToolError> {
        let cwd = cwd.canonicalize()?;
        let home = if home.is_absolute() {
            home.to_owned()
        } else {
            cwd.join(home)
        };
        let (hooks, diagnostics) = config::discover(&cwd, &home, trusted, sources, cancel)?;
        Ok(Self {
            runtime: Arc::new(Runtime {
                hooks,
                permission_mode,
                context: Mutex::new(None),
                tasks: Mutex::new(JoinSet::new()),
                limit: Arc::new(Semaphore::new(8)),
                shutdown: CancellationToken::new(),
                pending: Mutex::new(Outcome::default()),
            }),
            diagnostics,
        })
    }
    pub fn hooks(&self) -> &[Hook] {
        &self.runtime.hooks
    }

    /// Dispatch has no dependency on the kernel.
    pub async fn dispatch(
        &self,
        event: Event,
        input: Value,
        matcher: Option<&str>,
        cwd: &Path,
        cancel: CancellationToken,
    ) -> Result<Outcome, ToolError> {
        self.runtime
            .dispatch(event, input, matcher, cwd, cancel)
            .await
    }
}

impl Module for HooksModule {
    fn id(&self) -> &'static str {
        "hooks"
    }
    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        registry.register_tool_interceptor(self.runtime.clone(), Duration::MAX);
        registry.register_approval_interceptor(self.runtime.clone());
        registry.register_lifecycle_interceptor(self.runtime.clone());
        registry.register_compactor(self.runtime.clone());
        registry.register_command(Arc::new(ListCommand {
            runtime: self.runtime.clone(),
            diagnostics: self.diagnostics.clone(),
        }));
        Ok(())
    }
}

/// `/hooks`: each command hook with its event, matcher and trust, then load diagnostics.
struct ListCommand {
    runtime: Arc<Runtime>,
    diagnostics: Vec<String>,
}

#[async_trait]
impl Command for ListCommand {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "hooks".into(),
            description: "List command hooks".into(),
            argument_hint: None,
        }
    }

    async fn run(&self, _: &str, _: CancellationToken) -> Result<String, ToolError> {
        let mut lines: Vec<String> = self
            .runtime
            .hooks
            .iter()
            .map(|hook| {
                let matcher = hook
                    .matcher
                    .as_deref()
                    .map(|m| format!(" {m}"))
                    .unwrap_or_default();
                let state = if hook.enabled {
                    hook.trust.as_str()
                } else {
                    "disabled"
                };
                format!(
                    "{}{matcher} · {state}\n  {}\n  {}",
                    hook.event.wire(),
                    hook.command,
                    hook.path.display()
                )
            })
            .collect();
        if lines.is_empty() {
            lines.push("No hooks configured.".into());
        }
        lines.extend(self.diagnostics.iter().cloned());
        Ok(lines.join("\n"))
    }
}

impl Runtime {
    async fn dispatch(
        &self,
        event: Event,
        mut input: Value,
        matcher: Option<&str>,
        cwd: &Path,
        cancel: CancellationToken,
    ) -> Result<Outcome, ToolError> {
        check(&cancel)?;
        input["hook_event_name"] = json!(event.wire());
        let canonical = input["tool_name"].as_str().map(str::to_owned);
        let input = serde_json::to_string(&input).map_err(|e| ToolError(e.to_string()))?;
        let mut pending = FuturesUnordered::new();
        let mut result = Outcome::default();
        for (index, hook) in self.hooks.iter().enumerate().filter(|(_, hook)| {
            hook.enabled
                && hook.event == event
                && (hook.matches(matcher)
                    || canonical
                        .as_deref()
                        .is_some_and(|name| hook.matches(Some(name)))
                    || matcher.is_some_and(|name| {
                        aliases(name).iter().any(|alias| hook.matches(Some(alias)))
                    }))
        }) {
            if hook.asynchronous {
                let hook = hook.clone();
                let input = input.clone();
                let cwd = cwd.to_owned();
                let cancel = self.shutdown.child_token();
                let limit = self.limit.clone();
                self.tasks
                    .lock()
                    .map_err(|_| ToolError("Hook task state unavailable".into()))?
                    .spawn(async move {
                        let permit = tokio::select! {
                            _ = cancel.cancelled() => return Outcome::default(),
                            permit = limit.acquire_owned() => permit,
                        };
                        let Ok(_permit) = permit else {
                            return Outcome::default();
                        };
                        let mut output = execute(&hook, &input, &cwd, cancel).await;
                        output.block = None;
                        output.stop = None;
                        output.updated_input = None;
                        output.approval = None;
                        output
                    });
            } else {
                let input = &input;
                let cancel = cancel.child_token();
                pending.push(async move { (index, execute(hook, input, cwd, cancel).await) });
            }
        }
        // Completion order resolves competing rewrites; any block or denial wins.
        let mut completed = Vec::new();
        let mut updated = None;
        while let Some((index, outcome)) = pending.next().await {
            if outcome.updated_input.is_some() {
                updated = outcome.updated_input.clone();
            }
            completed.push((index, outcome));
        }
        completed.sort_by_key(|(index, _)| *index);
        for (_, outcome) in completed {
            merge(&mut result, outcome);
        }
        result.updated_input = updated;
        check(&cancel)?;
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| ToolError("Hook task state unavailable".into()))?;
        while let Some(Ok(outcome)) = tasks.try_join_next() {
            merge(&mut result, outcome);
        }
        if result.block.is_some() {
            result.updated_input = None;
        }
        Ok(result)
    }

    fn input(&self, ctx: &LifecycleContext) -> Value {
        json!({"session_id": ctx.session_id, "turn_id": ctx.turn_id, "transcript_path": ctx.transcript_path,
        "cwd": ctx.cwd, "model": ctx.model, "permission_mode": match self.permission_mode.get() {
            tiechui_plugin_api::PermissionMode::Rules => "default",
            tiechui_plugin_api::PermissionMode::Auto => "auto",
            tiechui_plugin_api::PermissionMode::Yolo => "bypassPermissions",
        }})
    }

    fn tool_input(&self, call: &ToolCall, ctx: &ToolContext) -> Result<Value, ToolError> {
        let context = self
            .context
            .lock()
            .map_err(|_| ToolError("Hook context unavailable".into()))?;
        let mut input = self.input(&context.clone().unwrap_or_else(|| LifecycleContext {
            session_id: ctx.scope.thread_id.0.clone(),
            turn_id: ctx.scope.turn_id.0.clone(),
            cwd: ctx.cwd.clone(),
            transcript_path: None,
            model: String::new(),
        }));
        input["session_id"] = json!(ctx.scope.thread_id.0);
        input["turn_id"] = json!(ctx.scope.turn_id.0);
        input["cwd"] = json!(ctx.cwd);
        input["tool_name"] = json!(if matches!(call.name.as_str(), "bash" | "exec_command") {
            "Bash"
        } else {
            &call.name
        });
        input["tool_input"] = call.arguments.clone();
        Ok(input)
    }
}

async fn execute(hook: &Hook, input: &str, cwd: &Path, cancel: CancellationToken) -> Outcome {
    match runner::run(hook, input, cwd, cancel).await {
        Ok(output) => output::parse(hook.event, output),
        Err(error) => Outcome {
            diagnostics: vec![format!("{}: {error}", hook.path.display())],
            ..Outcome::default()
        },
    }
}

fn merge(result: &mut Outcome, outcome: Outcome) {
    result.context.extend(outcome.context);
    result.diagnostics.extend(outcome.diagnostics);
    if result.block.is_none() {
        result.block = outcome.block;
    }
    if result.stop.is_none() {
        result.stop = outcome.stop;
    }
    if outcome.updated_input.is_some() {
        result.updated_input = outcome.updated_input;
    }
    if !matches!(result.approval, Some(ApprovalVerdict::Deny(_))) && outcome.approval.is_some() {
        result.approval = outcome.approval;
    }
}

fn audit(ctx: &ToolContext, event: Event, outcome: &Outcome) -> Result<(), ToolError> {
    (ctx.append_entry)(CustomSessionEntry {
        kind: "hooks.result".into(),
        data: json!({"event": event, "diagnostics": outcome.diagnostics, "context": outcome.context, "block": outcome.block, "stop": outcome.stop}),
    })
}

#[async_trait]
impl ToolInterceptor for Runtime {
    async fn before(&self, call: &ToolCall, ctx: ToolContext) -> Result<BeforeToolCall, ToolError> {
        let mut input = self.tool_input(call, &ctx)?;
        input["tool_use_id"] = json!(call.id);
        let outcome = self
            .dispatch(
                Event::PreToolUse,
                input,
                Some(&call.name),
                &ctx.cwd,
                ctx.cancel.clone(),
            )
            .await?;
        audit(&ctx, Event::PreToolUse, &outcome)?;
        self.pending
            .lock()
            .map_err(|_| ToolError("Hook context unavailable".into()))?
            .context
            .extend(outcome.context);
        Ok(if let Some(reason) = outcome.block {
            BeforeToolCall::Deny(reason)
        } else if let Some(input) = outcome.updated_input {
            BeforeToolCall::Rewrite(input)
        } else {
            BeforeToolCall::Allow
        })
    }
    async fn after(
        &self,
        call: &ToolCall,
        output: &ToolOutput,
        ctx: ToolContext,
    ) -> Result<Option<ToolOutput>, ToolError> {
        if output.is_error {
            return Ok(None);
        }
        let mut input = self.tool_input(call, &ctx)?;
        input["tool_use_id"] = json!(call.id);
        input["tool_response"] =
            serde_json::to_value(output).map_err(|e| ToolError(e.to_string()))?;
        let outcome = self
            .dispatch(
                Event::PostToolUse,
                input,
                Some(&call.name),
                &ctx.cwd,
                ctx.cancel.clone(),
            )
            .await?;
        audit(&ctx, Event::PostToolUse, &outcome)?;
        if outcome.stop.is_some() {
            self.pending
                .lock()
                .map_err(|_| ToolError("Hook context unavailable".into()))?
                .stop = outcome.stop;
        }
        if outcome.context.is_empty() {
            return Ok(None);
        }
        let mut output = output.clone();
        output.content.extend(
            outcome
                .context
                .into_iter()
                .map(|text| Content::Text { text, opaque: None }),
        );
        Ok(Some(output))
    }
}

#[async_trait]
impl ApprovalInterceptor for Runtime {
    async fn review(
        &self,
        call: &ToolCall,
        ctx: ToolContext,
    ) -> Result<Option<ApprovalVerdict>, ToolError> {
        let input = self.tool_input(call, &ctx)?;
        let outcome = self
            .dispatch(
                Event::PermissionRequest,
                input,
                Some(&call.name),
                &ctx.cwd,
                ctx.cancel.clone(),
            )
            .await?;
        audit(&ctx, Event::PermissionRequest, &outcome)?;
        Ok(outcome.approval)
    }
}

#[async_trait]
impl LifecycleInterceptor for Runtime {
    async fn on_event(
        &self,
        event: &LifecycleEvent,
        ctx: &LifecycleContext,
        cancel: CancellationToken,
    ) -> Result<LifecycleOutcome, ToolError> {
        *self
            .context
            .lock()
            .map_err(|_| ToolError("Hook context unavailable".into()))? = Some(ctx.clone());
        if matches!(event, LifecycleEvent::BeforeRequest) {
            let mut pending = std::mem::take(
                &mut *self
                    .pending
                    .lock()
                    .map_err(|_| ToolError("Hook context unavailable".into()))?,
            );
            let mut tasks = self
                .tasks
                .lock()
                .map_err(|_| ToolError("Hook task state unavailable".into()))?;
            while let Some(Ok(outcome)) = tasks.try_join_next() {
                merge(&mut pending, outcome);
            }
            return Ok(LifecycleOutcome {
                context: pending.context,
                stop: pending.stop,
                block: None,
                diagnostics: pending.diagnostics,
            });
        }
        let mut input = self.input(ctx);
        let (event, matcher) = match event {
            LifecycleEvent::BeforeRequest => unreachable!("handled above"),
            LifecycleEvent::SessionStart { resumed } => {
                input.as_object_mut().map(|map| map.remove("turn_id"));
                let source = if *resumed { "resume" } else { "startup" };
                input["source"] = json!(source);
                (Event::SessionStart, Some(source))
            }
            LifecycleEvent::UserInput { content } => {
                input["prompt"] = json!(
                    content
                        .iter()
                        .filter_map(|part| match part {
                            InputContent::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                (Event::UserPromptSubmit, None)
            }
            LifecycleEvent::BeforeCompaction { automatic } => {
                input
                    .as_object_mut()
                    .map(|map| map.remove("permission_mode"));
                let trigger = if *automatic { "auto" } else { "manual" };
                input["trigger"] = json!(trigger);
                (Event::PreCompact, Some(trigger))
            }
            LifecycleEvent::BeforeStop {
                active,
                last_message,
            } => {
                input["stop_hook_active"] = json!(active);
                input["last_assistant_message"] = json!(last_message);
                (Event::Stop, None)
            }
            LifecycleEvent::SessionEnd => {
                input = json!({"session_id": ctx.session_id, "transcript_path": ctx.transcript_path, "cwd": ctx.cwd, "reason": "other"});
                (Event::SessionEnd, Some("other"))
            }
        };
        let outcome = self
            .dispatch(event, input, matcher, &ctx.cwd, cancel)
            .await?;
        if event == Event::SessionEnd {
            self.shutdown.cancel();
            self.tasks
                .lock()
                .map_err(|_| ToolError("Hook task state unavailable".into()))?
                .abort_all();
        }
        Ok(LifecycleOutcome {
            context: outcome.context,
            stop: outcome.stop,
            block: outcome.block,
            diagnostics: outcome.diagnostics,
        })
    }
}

#[async_trait]
impl tiechui_plugin_api::Compactor for Runtime {
    async fn before_compact(
        &self,
        context: &tiechui_plugin_api::CompactionContext,
        cancel: CancellationToken,
    ) -> Result<tiechui_plugin_api::CompactionOutcome, ToolError> {
        let outcome = self
            .on_event(
                &LifecycleEvent::BeforeCompaction {
                    automatic: context.reason
                        != tiechui_plugin_api::protocol::CompactionReason::Manual,
                },
                &context.lifecycle,
                cancel,
            )
            .await?;
        Ok(if outcome.stop.is_some() || outcome.block.is_some() {
            tiechui_plugin_api::CompactionOutcome::Cancel
        } else {
            tiechui_plugin_api::CompactionOutcome::Continue
        })
    }
}

fn aliases(name: &str) -> &'static [&'static str] {
    match name {
        "read" => &["Read"],
        "write" => &["Write"],
        "edit" => &["Edit"],
        "apply_patch" => &["Write", "Edit"],
        _ => &[],
    }
}

fn check(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError("Operation aborted".into()))
    } else {
        Ok(())
    }
}
