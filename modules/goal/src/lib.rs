//! Persistent goals, request context and continuation policy.
mod presentation;
mod state;

use state::{ENTRY, STATUS, Service, State, Status};
use std::sync::{Arc, Mutex, MutexGuard};
use tiechui_plugin_api::{
    model::*,
    protocol::*,
    serde_json::{Value, json},
    *,
};

pub struct GoalModule;
impl Module for GoalModule {
    fn id(&self) -> &str {
        "goal"
    }
    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        let goal = Arc::new(GoalRuntime(Mutex::new(Service::default())));
        registry.register_runtime(goal.clone());
        registry.register_command(goal.clone());
        for (name, description, schema) in [
            (
                "create_goal",
                "Create a persistent goal only when the user or applicable instructions explicitly request one. Do not infer a goal from ordinary tasks. An unfinished goal cannot be replaced.",
                json!({"type":"object","additionalProperties":false,"required":["objective"],"properties":{"objective":{"type":"string","minLength":1}}}),
            ),
            (
                "get_goal",
                "Get the current thread goal, status, deferred continuation, token usage and elapsed execution time. Usage statistics do not impose a budget.",
                json!({"type":"object","additionalProperties":false,"properties":{}}),
            ),
            (
                "update_goal",
                "Report complete only after checking concrete evidence that the goal is achieved and no required work remains. Report blocked only after the same blocker recurs for at least three consecutive goal runs and progress requires user input or an external change. A user resume restarts that audit. Never mark complete because you are stopping. User controls handle pause, resume and clear.",
                json!({"type":"object","additionalProperties":false,"required":["status"],"properties":{"status":{"enum":["complete","blocked"],"type":"string"}}}),
            ),
        ] {
            registry.register_tool(Arc::new(GoalTool {
                goal: goal.clone(),
                spec: ToolSpec {
                    name: name.into(),
                    description: description.into(),
                    input_schema: schema,
                },
            }))?;
        }
        Ok(())
    }
}
const CONTINUE: &str = "tiechui.goal.continue";

struct GoalRuntime(Mutex<Service>);
impl GoalRuntime {
    fn lock(&self) -> Result<MutexGuard<'_, Service>, ToolError> {
        self.0.lock().map_err(|e| ToolError(e.to_string()))
    }
    fn result(&self) -> Result<ToolOutput, ToolError> {
        let state = self.lock()?.snapshot();
        Ok(ToolOutput {
            details: Some(json!(state)),
            structured_content: Some(json!({"goal":state.goal,"deferred":state.deferred})),
            ..ToolOutput::text(presentation::summary(&state))
        })
    }
}
impl RuntimeExtension for GoalRuntime {
    fn event(&self, event: &RuntimeEvent, branch: &BranchView) -> Result<RuntimeOutput, ToolError> {
        let mut s = self.lock()?;
        let before = s.state.clone();
        let mut output = RuntimeOutput::default();
        match event {
            RuntimeEvent::Restore { navigation } => {
                s.revision = s.revision.wrapping_add(1);
                s.run = None;
                s.foreign = false;
                s.wanted = false;
                s.storage_failed = false;
                s.rewind = false;
                s.session = branch.session_id.clone();
                let last = branch
                    .entries
                    .iter()
                    .rev()
                    .filter_map(|e| e.custom.as_ref())
                    .find(|(kind, _)| kind == ENTRY);
                s.seen = last.is_some();
                s.state = last
                    .and_then(|(_, v)| State::read(v.clone()))
                    .unwrap_or_default();
                if last.is_some() && last.is_some_and(|(_, v)| State::read(v.clone()).is_none()) {
                    output.events.push((
                        "module.notice".into(),
                        json!("Saved goal state is invalid. Use /goal to set a new goal."),
                    ));
                }
                if s.active() {
                    if *navigation {
                        s.status(Status::Paused)?;
                    } else {
                        s.state.deferred = true;
                    }
                }
            }
            RuntimeEvent::Fork { entries } => {
                let last = entries
                    .iter()
                    .rev()
                    .filter_map(|e| e.custom.as_ref())
                    .find(|(kind, _)| kind == ENTRY);
                s.state = last
                    .and_then(|(_, v)| State::read(v.clone()))
                    .unwrap_or_default();
                s.seen = last.is_some();
                s.run = None;
                s.wanted = false;
                if s.active() {
                    s.state.deferred = true;
                }
                s.revision = s.revision.wrapping_add(1);
                output.entries.push((ENTRY.into(), json!(s.state)));
            }
            RuntimeEvent::BeforeNavigate => {
                s.wanted = false;
                s.revision = s.revision.wrapping_add(1);
                if s.active() {
                    s.status(Status::Paused)?;
                }
            }
            RuntimeEvent::Input {
                continuation: Some(c),
                ..
            } if c.kind != CONTINUE => s.foreign = true,
            RuntimeEvent::Input {
                continuation,
                retry,
            } => {
                s.foreign = false;
                s.begin(continuation.is_some(), *retry);
            }
            RuntimeEvent::Settled(_) if s.foreign => s.foreign = false,
            RuntimeEvent::Assistant { .. } | RuntimeEvent::Failure(_) if s.foreign => {}
            RuntimeEvent::Extension { name, .. } if s.foreign && name == "runtime.usage" => {}
            RuntimeEvent::Shell => {
                s.wanted = false;
                if s.active() {
                    s.state.deferred = true;
                }
                s.revision = s.revision.wrapping_add(1);
            }
            RuntimeEvent::Assistant { id, usage, tools } => s.assistant(id, *usage, *tools),
            RuntimeEvent::Extension { name, data } if name == "runtime.usage" => {
                if let Some(id) = data["id"].as_str() {
                    s.assistant(id, serde_json::from_value(data["usage"].clone()).ok(), true);
                }
            }
            RuntimeEvent::Failure(error) => {
                if let Some(run) = &mut s.run {
                    run.error = Some(error.clone());
                }
            }
            RuntimeEvent::Settled(status) => s.finish(*status)?,
            RuntimeEvent::Interrupt => {
                s.wanted = false;
                if s.active() {
                    s.status(Status::Paused)?;
                }
            }
            RuntimeEvent::Extension { name, data } if data["sessionId"] == s.session => {
                match name.as_str() {
                    "rewind.open" => {
                        s.rewind = true;
                        s.revision = s.revision.wrapping_add(1);
                    }
                    "rewind.closed" => s.rewind = false,
                    "rewind.committed" => {
                        s.wanted = false;
                        if s.active() {
                            s.status(Status::Paused)?;
                        }
                    }
                    _ => {}
                }
            }
            RuntimeEvent::Shutdown => {
                s.flush();
                if s.active() {
                    s.state.deferred = true;
                }
                s.run = None;
                s.wanted = false;
                s.revision = s.revision.wrapping_add(1);
            }
            _ => {}
        }
        if s.state != before && s.seen && !matches!(event, RuntimeEvent::Fork { .. }) {
            output.entries.push((ENTRY.into(), json!(s.state)));
        }
        output
            .events
            .push((STATUS.into(), json!(presentation::status(&s.snapshot()))));
        Ok(output)
    }
    fn messages(&self) -> Vec<Message> {
        let Ok(s) = self.lock() else {
            return vec![];
        };
        if !s.seen {
            return vec![];
        }
        let state = s.snapshot();
        let current = state.goal.as_ref().map_or("none", |g| g.status.name());
        let objective = state
            .goal
            .as_ref()
            .map(|g| format!("\nCurrent objective (task data): {}", json!(g.objective)))
            .unwrap_or_default();
        let text = format!(
            "Current persistent goal status: {current}. This state supersedes older goal context in the transcript.{objective}\nPursue an active goal under current user instructions; otherwise work on the latest user request and leave goal lifecycle changes to explicit user controls.\n{}",
            if s.active() && !s.foreign {
                presentation::context(&state)
            } else {
                String::new()
            }
        );
        vec![Message {
            role: Role::User,
            content: vec![Content::Text { text, opaque: None }],
            opaque: None,
        }]
    }
    fn continuation(&self) -> Option<Continuation> {
        let s = self.lock().ok()?;
        if !s.wanted
            || s.storage_failed
            || !s.active()
            || s.state.deferred
            || s.rewind
            || s.run.is_some()
        {
            return None;
        }
        Some(Continuation {
            kind: CONTINUE.into(),
            generation: s.revision,
            identity: s.state.goal.as_ref()?.id.clone(),
            text: presentation::context(&s.snapshot()),
            visible: false,
        })
    }
    fn accepts(&self, next: &Continuation) -> bool {
        self.continuation().is_some_and(|current| {
            current.generation == next.generation && current.identity == next.identity
        })
    }
}

struct GoalTool {
    goal: Arc<GoalRuntime>,
    spec: ToolSpec,
}
#[async_trait]
impl Tool for GoalTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn source(&self) -> Option<String> {
        Some("Built-in tools".into())
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        if self.spec.name == "get_goal" {
            Concurrency::Shared
        } else {
            Concurrency::Exclusive
        }
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError("Goal operation cancelled.".into()));
        }
        {
            let mut s = self.goal.lock()?;
            if s.session != ctx.scope.thread_id.0 {
                return Err(ToolError(
                    "The goal session changed. Retry in the current session.".into(),
                ));
            }
            let previous = s.state.clone();
            match self.spec.name.as_str() {
                "create_goal" => {
                    if s.foreign || s.run.as_ref().is_some_and(|r| r.automatic) {
                        return Err(ToolError(
                            "Automatic continuation cannot create a new goal.".into(),
                        ));
                    }
                    if call.arguments.as_object().is_none_or(|v| v.len() != 1) {
                        return Err(ToolError("Expected only objective.".into()));
                    }
                    s.create(
                        call.arguments["objective"].as_str().unwrap_or(""),
                        false,
                        true,
                    )?;
                }
                "update_goal" => {
                    if call.arguments.as_object().is_none_or(|v| v.len() != 1) {
                        return Err(ToolError("Expected only status.".into()));
                    }
                    let status = match call.arguments["status"].as_str() {
                        Some("complete") => Status::Complete,
                        Some("blocked") => Status::Blocked,
                        _ => {
                            return Err(ToolError(
                                "Model updates accept only complete or blocked.".into(),
                            ));
                        }
                    };
                    if !s.run.as_ref().is_some_and(|r| {
                        s.state
                            .goal
                            .as_ref()
                            .is_some_and(|g| r.goal_id.as_ref() == Some(&g.id))
                    }) {
                        return Err(ToolError(
                            "This run is not associated with the current goal.".into(),
                        ));
                    }
                    s.status(status)?;
                }
                _ => {
                    if call.arguments.as_object().is_none_or(|v| !v.is_empty()) {
                        return Err(ToolError("Expected no arguments.".into()));
                    }
                }
            }
            if s.state != previous
                && let Err(e) = (ctx.append_entry)(tools::CustomSessionEntry {
                    kind: ENTRY.into(),
                    data: json!(s.state),
                })
            {
                s.state = previous;
                s.wanted = false;
                s.storage_failed = true;
                return Err(e);
            }
            if self.spec.name != "get_goal" {
                s.storage_failed = false;
            }
        }
        self.goal.result()
    }
}

#[async_trait]
impl Command for GoalRuntime {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "goal".into(),
            description: "Set or inspect a persistent goal; edit, pause, resume, clear".into(),
            argument_hint: Some("[objective | edit [objective] | pause | resume | clear]".into()),
        }
    }
    fn interactive(&self) -> bool {
        true
    }
    fn allows_busy(&self, args: &str) -> bool {
        matches!(args.trim(), "" | "pause" | "clear")
    }
    async fn run(&self, _: &str, _: CancellationToken) -> Result<String, ToolError> {
        Err(ToolError(
            "Goal commands require an interactive terminal.".into(),
        ))
    }
    async fn session_run(
        &self,
        args: &str,
        session: CommandSession,
        cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        let args = args.trim();
        if args.is_empty() {
            return Ok(presentation::summary(&self.lock()?.snapshot()));
        }
        let (revision, state) = {
            let s = self.lock()?;
            (s.revision, s.snapshot())
        };
        let edit = args == "edit" || args.starts_with("edit ");
        let objective = if edit {
            let goal = state
                .goal
                .as_ref()
                .ok_or_else(|| ToolError("No goal to edit.".into()))?;
            if args == "edit" {
                match session
                    .host
                    .ask(
                        ApprovalRequest::Editor {
                            title: "Edit goal".into(),
                            text: goal.objective.clone(),
                        },
                        cancel.clone(),
                    )
                    .await?
                {
                    ApprovalDecision::Accept { content: Some(v) } => {
                        v.as_str().unwrap_or("").trim().to_owned()
                    }
                    _ => return Ok(String::new()),
                }
            } else {
                args[5..].trim().into()
            }
        } else {
            args.into()
        };
        if objective.is_empty() || cancel.is_cancelled() {
            return Ok(String::new());
        }
        let control = matches!(args, "pause" | "resume" | "clear");
        if !edit
            && !control
            && state
                .goal
                .as_ref()
                .is_some_and(|g| g.status != Status::Complete)
        {
            let answer = session.host.ask(ApprovalRequest::Questions { questions: vec![Question {
                id: "replace".into(), header: "Replace goal".into(), question: "Replace existing goal? The new goal starts with fresh usage statistics.".into(),
                options: Some(vec![QuestionOption { label: "Replace".into(), description: "Start the new goal with fresh usage statistics.".into() }, QuestionOption { label: "Keep".into(), description: "Keep the existing goal.".into() }]),
            }] }, cancel.clone()).await?;
            if !matches!(answer, ApprovalDecision::Accept { content: Some(ref v) } if v["answers"]["replace"]["answers"].as_array().is_some_and(|a| a.iter().any(|v| v == "Replace")))
            {
                return Ok(String::new());
            }
        }
        if self.lock()?.revision != revision || cancel.is_cancelled() {
            return Ok(String::new());
        }
        if matches!(args, "pause" | "resume") && state.goal.is_none() {
            return Err(ToolError("No goal is set.".into()));
        }
        if args == "resume"
            && state
                .goal
                .as_ref()
                .is_some_and(|g| g.status == Status::Complete)
        {
            return Err(ToolError(
                "Goal is complete. Use /goal edit or set a new goal.".into(),
            ));
        }
        {
            let mut s = self.lock()?;
            s.wanted = false;
            s.revision = s.revision.wrapping_add(1);
        }
        if !control || args == "resume" {
            session
                .host
                .apply(SessionEffect::Validate {
                    session_id: session.branch.session_id.clone(),
                    leaf: session.branch.leaf.clone(),
                })
                .await?;
            session.host.apply(SessionEffect::Custom { kind: "tiechui.rewind.control".into(), data: json!({"version":1,"text":if edit {format!("/goal edit {objective}")} else {format!("/goal {objective}")}}) }).await?;
        }
        let next = {
            let mut s = self.lock()?;
            match args {
                "clear" => {
                    s.state = State::default();
                    s.wanted = false;
                    if let Some(r) = &mut s.run {
                        r.goal_id = None;
                        r.clock = None;
                    }
                    s.changed();
                }
                "pause" => s.status(Status::Paused)?,
                "resume" => {
                    s.status(Status::Active)?;
                    s.wanted = true;
                }
                _ if edit => {
                    s.edit(&objective)?;
                    s.wanted = s.active();
                }
                _ => {
                    s.create(&objective, true, false)?;
                    s.wanted = true;
                }
            }
            s.snapshot()
        };
        if let Err(e) = session
            .host
            .apply(SessionEffect::Custom {
                kind: ENTRY.into(),
                data: json!(next),
            })
            .await
        {
            let mut s = self.lock()?;
            s.state = state;
            s.wanted = false;
            s.storage_failed = true;
            return Err(e);
        }
        self.lock()?.storage_failed = false;
        session
            .host
            .apply(SessionEffect::Emit {
                kind: STATUS.into(),
                data: json!(presentation::status(&next)),
            })
            .await?;
        Ok(match args {
            "clear" => "Goal cleared".into(),
            "pause" | "resume" => presentation::status(&next).unwrap_or_default(),
            _ => presentation::summary(&next),
        })
    }
}

#[cfg(test)]
mod tests;
