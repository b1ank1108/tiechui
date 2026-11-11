//! Permission rules and approvals, implemented solely at the tool interception boundary.

mod agents;
mod bash;
mod classifier;
mod diagnostics;
mod path;
mod policy;
mod protected;
mod readonly;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tiechui_plugin_api::{
    BeforeToolCall, BranchView, CancellationToken, Module, ModuleConfig,
    PermissionAction as Action, PermissionMode, Registry, RuntimeEvent, RuntimeExtension,
    RuntimeOutput, SharedPermissionMode, ToolContext, ToolError, ToolInterceptor, async_trait,
    model::ToolCall, protocol::PERMISSION_MODE_EVENT, serde_json, tools::CustomSessionEntry,
};

use path::Access;
use policy::Rule;

pub const ENTRY_KIND: &str = "permissions.decision";

/// How a session without an approval UI settles asks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HeadlessPolicy {
    /// Every call is denied.
    Deny,
    /// Asks the mode does not settle are denied as unavailable.
    #[default]
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub actor: Option<String>,
    pub call: ToolCall,
    pub surface: String,
    /// What the rule was matched against, such as the command or the paths.
    pub values: Vec<String>,
    pub matched_pattern: Option<String>,
    pub origin: String,
    pub session_patterns: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession { both_directions: bool },
    Deny { reason: Option<String> },
    Unavailable,
}

#[async_trait]
pub trait ApprovalRequester: Send + Sync {
    async fn request(
        &self,
        request: ApprovalRequest,
        cancel: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError>;
}

#[async_trait]
impl ApprovalRequester for HeadlessPolicy {
    async fn request(
        &self,
        _: ApprovalRequest,
        cancel: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError> {
        check(&cancel)?;
        Ok(ApprovalDecision::Unavailable)
    }
}

#[derive(Clone)]
pub struct PermissionsModule {
    policy_source: String,
    project_source: String,
    recent: Arc<Mutex<std::collections::VecDeque<String>>>,
    actor: Option<String>,
    cwd: PathBuf,
    home: PathBuf,
    requester: Arc<dyn ApprovalRequester>,
    headless: Option<HeadlessPolicy>,
    mode: SharedPermissionMode,
}

impl PermissionsModule {
    pub fn with_mode(self, mode: PermissionMode) -> Self {
        self.mode.set(mode);
        self
    }
    /// The live mode shared with child agents, `/permissions` and hooks.
    pub fn mode(&self) -> SharedPermissionMode {
        self.mode.clone()
    }
    pub fn with_sources(mut self, user: &Path, project: Option<&Path>, trusted: bool) -> Self {
        self.policy_source = user.display().to_string();
        self.project_source = if trusted {
            project.map_or_else(|| "none".into(), |p| p.display().to_string())
        } else {
            "not loaded (untrusted)".into()
        };
        self
    }
    pub fn register_child(
        &self,
        registry: &mut Registry,
        config: &ModuleConfig,
        home: &Path,
        trusted: bool,
        name: &str,
    ) -> Result<(), ToolError> {
        if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
            return Err(ToolError("Invalid agent policy name".into()));
        }
        let mut config = config.clone();
        agents::load(&mut config, &home.join("agents").join(format!("{name}.md")));
        if trusted
            && let Some(directory) = self.cwd.ancestors().find(|directory| {
                [".agents/agents", ".claude/agents"]
                    .iter()
                    .any(|name| directory.join(name).is_dir())
            })
        {
            let path = [".agents/agents", ".claude/agents"]
                .iter()
                .map(|source| directory.join(source).join(format!("{name}.md")))
                .find(|path| path.is_file());
            if let Some(path) = path {
                agents::load(&mut config, &path);
            }
        }
        let mut child = self.clone();
        child.actor = Some(format!("Child {name}"));
        child.register(registry, &config)
    }
    pub fn register_default_child(
        &self,
        registry: &mut Registry,
        config: &ModuleConfig,
    ) -> Result<(), ToolError> {
        let mut child = self.clone();
        child.actor = Some("Child default".into());
        child.register(registry, config)
    }
    pub fn headless(cwd: PathBuf, home: PathBuf, policy: HeadlessPolicy) -> Self {
        Self {
            policy_source: home.join(".tiechui/config.toml").display().to_string(),
            project_source: "not loaded".into(),
            recent: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            actor: None,
            cwd,
            home,
            requester: Arc::new(policy),
            headless: Some(policy),
            mode: SharedPermissionMode::default(),
        }
    }

    pub fn with_requester(
        cwd: PathBuf,
        home: PathBuf,
        requester: Arc<dyn ApprovalRequester>,
    ) -> Self {
        Self {
            policy_source: home.join(".tiechui/config.toml").display().to_string(),
            project_source: "not loaded".into(),
            recent: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            actor: None,
            cwd,
            home,
            requester,
            headless: None,
            mode: SharedPermissionMode::default(),
        }
    }
}

impl Module for PermissionsModule {
    fn id(&self) -> &'static str {
        "permissions"
    }
    fn mandatory(&self) -> bool {
        true
    }

    fn register(&self, registry: &mut Registry, config: &ModuleConfig) -> Result<(), ToolError> {
        for rule in config
            .permissions
            .rules
            .iter()
            .flatten()
            .chain(&config.permissions.project_rules)
        {
            rule.validate().map_err(|e| ToolError(e.into()))?;
        }
        let permissions = Arc::new(Permissions {
            actor: self.actor.clone(),
            rules: policy::compose(&config.permissions),
            cwd: path::canonical(&self.cwd)?,
            home: self.home.clone(),
            requester: self.requester.clone(),
            approvals: registry.approval_interceptors().to_vec(),
            headless: self.headless,
            mode: self.mode.clone(),
            denials: Arc::new(Mutex::new(classifier::Denials::default())),
            servers: config.mcp_servers.keys().cloned().collect(),
            sessions: Arc::new(Mutex::new(SessionGrants::default())),
            recent: self.recent.clone(),
            policy_source: self.policy_source.clone(),
            project_source: self.project_source.clone(),
        });
        if self.actor.is_none() {
            registry.register_runtime(Arc::new(ModeFeed {
                mode: self.mode.clone(),
                sent: Mutex::new(None),
            }));
        }
        registry.register_session_observer(permissions.clone());
        registry.register_command(Arc::new(diagnostics::Diagnostics(permissions.clone())));
        registry.register_tool_interceptor(permissions, Duration::from_secs(60 * 60));
        Ok(())
    }
}

/// Tells clients the live mode whenever it differs from what they last saw.
struct ModeFeed {
    mode: SharedPermissionMode,
    sent: Mutex<Option<PermissionMode>>,
}

impl RuntimeExtension for ModeFeed {
    fn event(&self, event: &RuntimeEvent, _: &BranchView) -> Result<RuntimeOutput, ToolError> {
        let mut sent = self.sent.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(event, RuntimeEvent::Restore { .. }) {
            *sent = None;
        }
        let mode = self.mode.get();
        let mut events = Vec::new();
        if sent.replace(mode) != Some(mode) {
            events.push((PERMISSION_MODE_EVENT.into(), serde_json::json!(mode)));
        }
        Ok(RuntimeOutput {
            events,
            ..Default::default()
        })
    }
}

#[derive(Default)]
struct SessionGrants {
    identity: Option<(String, u64)>,
    rules: Vec<Rule>,
}

#[derive(Clone)]
struct Permissions {
    policy_source: String,
    project_source: String,
    recent: Arc<Mutex<std::collections::VecDeque<String>>>,
    actor: Option<String>,
    rules: Vec<Rule>,
    cwd: PathBuf,
    home: PathBuf,
    requester: Arc<dyn ApprovalRequester>,
    approvals: Vec<Arc<dyn tiechui_plugin_api::ApprovalInterceptor>>,
    headless: Option<HeadlessPolicy>,
    mode: SharedPermissionMode,
    denials: Arc<Mutex<classifier::Denials>>,
    servers: Vec<String>,
    sessions: Arc<Mutex<SessionGrants>>,
}

#[derive(Clone)]
struct Gate {
    surface: String,
    values: Vec<String>,
    rule: Rule,
    patterns: Vec<String>,
    flagged: Option<String>,
    boundary: bool,
}

impl Permissions {
    fn gate(
        &self,
        surface: &str,
        values: Vec<String>,
        patterns: Vec<String>,
        flagged: Option<String>,
        boundary: bool,
    ) -> Gate {
        Gate {
            rule: policy::evaluate(surface, &values, &self.rules, &self.home),
            surface: surface.into(),
            values,
            patterns,
            flagged,
            boundary,
        }
    }

    fn access_gates(&self, access: &Access, cwd: &Path, gates: &mut Vec<Gate>, tool: Option<&str>) {
        if protected::protected(access, &self.cwd, cwd) {
            let mut gate = self.gate(
                "protected_path",
                access.values.clone(),
                vec![access.session_pattern()],
                Some(access.values[0].clone()),
                false,
            );
            if !keeps_protected_rule(&gate.rule) {
                gate.rule = Rule::new(
                    "protected_path",
                    "*",
                    Action::Ask,
                    synthetic_origin(gate.rule.origin),
                );
            }
            gates.push(gate);
        }
        for family in ["path", "external_directory"] {
            if family == "external_directory" && !access.outside(&self.cwd, cwd) {
                continue;
            }
            let mut family_gates = Vec::new();
            for direction in ["read", "write"] {
                if access.direction != "both" && access.direction != direction {
                    continue;
                }
                let surface = format!("{family}_{direction}");
                let gate = self.gate(
                    &surface,
                    access.values.clone(),
                    vec![access.session_pattern()],
                    Some(access.values[0].clone()),
                    family == "external_directory",
                );
                // The cross-cutting path gate exists only for explicit path rules.
                if family == "path" && gate.rule.surface == "*" {
                    continue;
                }
                family_gates.push(gate);
            }
            if access.direction == "both" {
                if let Some(mut worst) =
                    family_gates.into_iter().max_by_key(|gate| gate.rule.action)
                {
                    worst.surface = family.into();
                    gates.push(worst);
                }
            } else {
                gates.extend(family_gates);
            }
        }
        if let Some(tool) = tool {
            // Allowing one change inside the workspace for the session allows all of them.
            let pattern = if matches!(tool, "write" | "edit") && !access.outside(&self.cwd, cwd) {
                self.cwd.join("*").to_string_lossy().into_owned()
            } else {
                access.session_pattern()
            };
            gates.push(self.gate(
                tool,
                access.values.clone(),
                vec![pattern],
                Some(access.values[0].clone()),
                false,
            ));
        }
    }

    fn gates(
        &self,
        call: &ToolCall,
        cwd: &Path,
        cancel: &CancellationToken,
    ) -> Result<Vec<Gate>, ToolError> {
        check(cancel)?;
        let mut gates = Vec::new();
        if matches!(call.name.as_str(), "bash" | "exec_command") {
            let key = if call.name == "bash" {
                "command"
            } else {
                "cmd"
            };
            let command = call.arguments[key].as_str().unwrap_or("");
            let shell_cwd = call.arguments["workdir"]
                .as_str()
                .map(|dir| cwd.join(dir))
                .unwrap_or_else(|| cwd.to_owned());
            let program = bash::inspect(command, &shell_cwd, &self.home, cancel)?;
            let writes = program
                .paths
                .iter()
                .any(|access| access.direction == "write");
            for access in program.paths {
                self.access_gates(&access, &shell_cwd, &mut gates, None);
            }
            let fallback_rule = policy::evaluate("bash", &["*".into()], &self.rules, &self.home);
            let fallback = fallback_rule.action;
            let origin = synthetic_origin(fallback_rule.origin);
            let mut units = Vec::new();
            for unit in program.units {
                let pattern = command_pattern(&unit.values[0]);
                let mut gate = self.gate("bash", unit.values, vec![pattern], None, false);
                if fallback != Action::Allow && unit.wrapper && gate.rule.action == Action::Allow {
                    gate.rule = Rule::new("bash", "<opaque-bash-wrapper>", Action::Ask, origin);
                }
                if gate.rule.surface == "*"
                    && gate.rule.action == Action::Ask
                    && gate.rule.origin != "fail-closed"
                    && !writes
                    && unit.argv.as_deref().is_some_and(readonly::read_only)
                {
                    gate.rule = Rule::new("bash", "<read-only>", Action::Allow, "builtin");
                }
                units.push(gate);
            }
            if units.is_empty() {
                units.push(self.gate(
                    "bash",
                    vec![command.into()],
                    vec![command_pattern(command)],
                    None,
                    false,
                ));
            }
            if fallback != Action::Allow && program.unresolved {
                units.push(Gate {
                    rule: Rule::new("bash", "<unparsed-bash-subtree>", Action::Ask, origin),
                    ..self.gate(
                        "bash",
                        vec![command.into()],
                        vec![command.into()],
                        None,
                        false,
                    )
                });
            }
            gates.extend(units);
        } else if call.name == "mcp" || call.name.starts_with("mcp__") {
            let raw = if call.name == "mcp" {
                call.arguments["arguments"]["path"].as_str()
            } else {
                call.arguments["path"].as_str()
            };
            if let Some(raw) = raw {
                let access = Access::new(raw, cwd, &self.home, None, "both")?;
                self.access_gates(&access, cwd, &mut gates, None);
            }
            let values = mcp_targets(call, &self.servers);
            gates.push(self.gate(
                "mcp",
                values,
                vec![call.name.clone()],
                Some(call.name.clone()),
                false,
            ));
        } else {
            let path_tool = matches!(
                call.name.as_str(),
                "read" | "write" | "edit" | "find" | "grep" | "ls"
            );
            let raw = call.arguments["path"]
                .as_str()
                .or_else(|| path_tool.then_some("."));
            if let Some(raw) = raw {
                let direction = match call.name.as_str() {
                    "read" | "find" | "grep" | "ls" => "read",
                    "write" => "write",
                    _ => "both",
                };
                let access = Access::new(
                    raw,
                    cwd,
                    &self.home,
                    path_tool.then_some(call.name.as_str()),
                    direction,
                )?;
                self.access_gates(
                    &access,
                    cwd,
                    &mut gates,
                    path_tool.then_some(call.name.as_str()),
                );
            }
            if !path_tool || raw.is_none() {
                gates.push(self.gate(&call.name, vec!["*".into()], vec!["*".into()], None, false));
            }
        }
        Ok(gates)
    }

    async fn decide(
        &self,
        call: &ToolCall,
        ctx: &ToolContext,
        audit: &mut Audit<'_>,
    ) -> Result<BeforeToolCall, ToolError> {
        let inspector = self.clone();
        let inspected_call = call.clone();
        let inspected_cwd = ctx.cwd.clone();
        let inspected_cancel = ctx.cancel.clone();
        let inspection = tokio::task::spawn_blocking(move || {
            inspector.gates(&inspected_call, &inspected_cwd, &inspected_cancel)
        });
        let mut gates = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Err(ToolError("Permission request cancelled.".into())),
            result = inspection => result.map_err(|error| ToolError(error.to_string()))??,
        };
        if self.headless == Some(HeadlessPolicy::Deny) {
            let mut gate = self.gate(&call.name, vec!["*".into()], vec![], None, false);
            gate.rule = Rule::new(&call.name, "*", Action::Deny, "headless");
            gates.insert(0, gate);
        }
        let mode = self.mode.get();
        if mode == PermissionMode::Auto {
            for gate in &mut gates {
                if gate.surface == "bash" && protected::broad_bash_allow(&gate.rule) {
                    gate.rule = Rule::new("bash", "*", Action::Ask, "builtin");
                } else if matches!(gate.surface.as_str(), "write" | "edit")
                    && gate.rule.surface == "*"
                    && gate.rule.action == Action::Ask
                    && gate.rule.origin != "fail-closed"
                {
                    gate.rule = Rule::new(&gate.surface, "<auto-edit>", Action::Allow, "builtin");
                }
            }
        }
        let identity = (ctx.scope.thread_id.0.clone(), ctx.epoch.0);
        {
            let mut sessions = self
                .sessions
                .lock()
                .map_err(|_| ToolError("Permission grants unavailable".into()))?;
            if sessions.identity.as_ref() != Some(&identity) {
                sessions.rules.clear();
                sessions.identity = Some(identity.clone());
            }
            for gate in &mut gates {
                if gate.rule.action != Action::Ask {
                    continue;
                }
                let grant = if matches!(gate.surface.as_str(), "path" | "external_directory") {
                    let resolve = |direction| {
                        let surface = format!("{}_{direction}", gate.surface);
                        let base =
                            policy::evaluate(&surface, &gate.values, &self.rules, &self.home);
                        let session =
                            policy::evaluate(&surface, &gate.values, &sessions.rules, &self.home);
                        if base.action == Action::Ask && session.action == Action::Allow {
                            session
                        } else {
                            base
                        }
                    };
                    let read = resolve("read");
                    let write = resolve("write");
                    if read.action >= write.action {
                        read
                    } else {
                        write
                    }
                } else {
                    policy::evaluate(&gate.surface, &gate.values, &sessions.rules, &self.home)
                };
                if grant.action == Action::Allow {
                    gate.rule = grant;
                }
            }
        }
        // Resolve session grants per command before folding the bash gate, so
        // approving a wrapper never masks an asking command inside it.
        let (commands, mut gates): (Vec<_>, Vec<_>) =
            gates.into_iter().partition(|gate| gate.surface == "bash");
        if let Some(worst) = commands.into_iter().reduce(|current, next| {
            if next.rule.action > current.rule.action {
                next
            } else {
                current
            }
        }) {
            gates.push(worst);
        }
        // Inspect every gate before prompting. A hard denial cannot be approved away.
        if let Some(gate) = gates.iter().find(|gate| gate.rule.action == Action::Deny) {
            audit.write(gate, "denied", "rule")?;
            return Ok(BeforeToolCall::Deny(denial(
                call,
                gate,
                "policy",
                gate.rule.reason.as_deref(),
                &self.cwd,
            )));
        }
        let mut approved: Vec<(String, String)> = Vec::new();
        let mut review = None;
        for gate in &gates {
            check(&ctx.cancel)?;
            if gate.rule.action == Action::Allow {
                audit.write(gate, "allowed", gate.rule.origin)?;
                continue;
            }
            if approved.iter().any(|(surface, pattern)| {
                surface == &gate.surface && gate.values.iter().any(|v| policy::wildcard(pattern, v))
            }) {
                continue;
            }
            let request = ApprovalRequest {
                actor: self.actor.clone(),
                call: call.clone(),
                surface: gate.surface.clone(),
                values: gate.values.clone(),
                matched_pattern: matched_pattern(&gate.rule),
                origin: gate.rule.origin.into(),
                session_patterns: gate.patterns.clone(),
                cwd: ctx.cwd.clone(),
            };
            let mut verdict = None;
            for reviewer in &self.approvals {
                check(&ctx.cancel)?;
                if let Some(review) = reviewer.review(call, ctx.clone()).await? {
                    let denied = matches!(review, tiechui_plugin_api::ApprovalVerdict::Deny(_));
                    verdict = Some(review);
                    if denied {
                        break;
                    }
                }
            }
            let settled = gate.surface != "protected_path" && gate.rule.origin != "fail-closed";
            let mut source = "approval";
            let decision = if let Some(verdict) = verdict {
                match verdict {
                    tiechui_plugin_api::ApprovalVerdict::Allow => ApprovalDecision::AllowOnce,
                    tiechui_plugin_api::ApprovalVerdict::Deny(reason) => ApprovalDecision::Deny {
                        reason: Some(reason),
                    },
                }
            } else if mode == PermissionMode::Yolo {
                if gate.rule.origin == "fail-closed" {
                    audit.write(gate, "denied", "fail-closed")?;
                    return Ok(BeforeToolCall::Deny(denial(
                        call,
                        gate,
                        "policy",
                        Some("Invalid project permission configuration."),
                        &self.cwd,
                    )));
                }
                source = "yolo";
                ApprovalDecision::AllowOnce
            } else if !settled && self.headless.is_some() {
                ApprovalDecision::Unavailable
            } else {
                if settled && mode == PermissionMode::Auto {
                    if review.is_none() {
                        review = Some(tokio::select! {
                            biased;
                            _ = ctx.cancel.cancelled() => return Err(ToolError("Permission request cancelled.".into())),
                            review = classifier::review(call, ctx, &self.cwd) => review,
                        });
                    }
                    let mut denials = self
                        .denials
                        .lock()
                        .map_err(|_| ToolError("Permission classifier unavailable".into()))?;
                    match &review {
                        Some(classifier::Review::Allow) => {
                            denials.consecutive = 0;
                            audit.write(gate, "approved", "classifier")?;
                            continue;
                        }
                        Some(classifier::Review::Block(reason)) if !denials.block() => {
                            drop(denials);
                            audit.write(gate, "denied", "classifier")?;
                            return Ok(BeforeToolCall::Deny(denial(
                                call,
                                gate,
                                "classifier",
                                Some(reason),
                                &self.cwd,
                            )));
                        }
                        _ => {}
                    }
                }
                tokio::select! {
                    biased;
                    _ = ctx.cancel.cancelled() => { audit.write(gate, "denied", "cancelled")?; return Ok(BeforeToolCall::Deny("Permission request cancelled.".into())); }
                    result = self.requester.request(request, ctx.cancel.child_token()) => result?,
                }
            };
            check(&ctx.cancel)?;
            match decision {
                ApprovalDecision::AllowOnce => audit.write(gate, "approved", source)?,
                ApprovalDecision::AllowSession { both_directions } => {
                    let mut sessions = self
                        .sessions
                        .lock()
                        .map_err(|_| ToolError("Permission grants unavailable".into()))?;
                    if sessions.identity.as_ref() != Some(&identity) {
                        return Err(ToolError("Permission session changed.".into()));
                    }
                    for pattern in &gate.patterns {
                        let surface = if both_directions {
                            gate.surface
                                .strip_suffix("_read")
                                .or_else(|| gate.surface.strip_suffix("_write"))
                                .unwrap_or(&gate.surface)
                        } else {
                            &gate.surface
                        };
                        if matches!(surface, "path" | "external_directory") {
                            for direction in ["read", "write"] {
                                sessions.rules.push(Rule::new(
                                    &format!("{surface}_{direction}"),
                                    pattern,
                                    Action::Allow,
                                    "session",
                                ));
                            }
                        } else if matches!(surface, "write" | "edit") {
                            // Writing and editing a file are one permission to the user.
                            for tool in ["write", "edit"] {
                                sessions.rules.push(Rule::new(
                                    tool,
                                    pattern,
                                    Action::Allow,
                                    "session",
                                ));
                            }
                        } else {
                            sessions.rules.push(Rule::new(
                                surface,
                                pattern,
                                Action::Allow,
                                "session",
                            ));
                        }
                        approved.push((gate.surface.clone(), pattern.clone()));
                    }
                    audit.write(gate, "approved_for_session", "approval")?;
                }
                ApprovalDecision::Deny { reason } => {
                    audit.write(gate, "denied", "approval")?;
                    return Ok(BeforeToolCall::Deny(denial(
                        call,
                        gate,
                        "user",
                        reason.as_deref(),
                        &self.cwd,
                    )));
                }
                ApprovalDecision::Unavailable => {
                    audit.write(gate, "denied", "unavailable")?;
                    return Ok(BeforeToolCall::Deny(denial(
                        call,
                        gate,
                        "unavailable",
                        Some("Approval requires an interactive parent session."),
                        &self.cwd,
                    )));
                }
            }
        }
        Ok(BeforeToolCall::Allow)
    }
}

#[async_trait]
impl ToolInterceptor for Permissions {
    fn stage(&self) -> tiechui_plugin_api::InterceptorStage {
        tiechui_plugin_api::InterceptorStage::Policy
    }
    async fn before(&self, call: &ToolCall, ctx: ToolContext) -> Result<BeforeToolCall, ToolError> {
        let mut audit = Audit {
            ctx: &ctx,
            call_id: &call.id,
            finished: false,
            recent: &self.recent,
            actor: self.actor.as_deref().unwrap_or("Main"),
        };
        let result = self.decide(call, &ctx, &mut audit).await;
        if result.is_err() {
            audit.failure()?;
        }
        audit.finished = true;
        result
    }
}

struct Audit<'a> {
    actor: &'a str,
    recent: &'a Mutex<std::collections::VecDeque<String>>,
    ctx: &'a ToolContext,
    call_id: &'a str,
    finished: bool,
}
impl Audit<'_> {
    fn write(&mut self, gate: &Gate, outcome: &str, source: &str) -> Result<(), ToolError> {
        let mut recent = self.recent.lock().map_err(|e| ToolError(e.to_string()))?;
        recent.push_back(format!(
            "{} · {outcome} · {} · {} · {source}",
            self.actor,
            gate.surface,
            matched_pattern(&gate.rule).unwrap_or_else(|| "default".into())
        ));
        while recent.len() > 100 {
            recent.pop_front();
        }
        drop(recent);
        (self.ctx.append_entry)(CustomSessionEntry {
            kind: ENTRY_KIND.into(),
            data: serde_json::json!({ "version": 1, "call_id": self.call_id, "outcome": outcome, "source": source, "surface": gate.surface, "pattern": matched_pattern(&gate.rule), "origin": gate.rule.origin }),
        })?;
        Ok(())
    }
    fn failure(&mut self) -> Result<(), ToolError> {
        let mut recent = self.recent.lock().map_err(|e| ToolError(e.to_string()))?;
        recent.push_back("denied · gate_error".into());
        while recent.len() > 100 {
            recent.pop_front();
        }
        drop(recent);
        (self.ctx.append_entry)(CustomSessionEntry {
            kind: ENTRY_KIND.into(),
            data: serde_json::json!({ "version": 1, "call_id": self.call_id, "outcome": "denied", "source": "gate_error" }),
        })?;
        Ok(())
    }
}
impl Drop for Audit<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.failure();
        }
    }
}

fn keeps_protected_rule(rule: &Rule) -> bool {
    rule.surface == "protected_path" && (rule.action == Action::Deny || rule.origin == "user")
}

fn synthetic_origin(origin: &'static str) -> &'static str {
    if origin == "fail-closed" {
        "fail-closed"
    } else {
        "builtin"
    }
}

fn matched_pattern(rule: &Rule) -> Option<String> {
    (rule.origin != "builtin" && rule.surface != "*" || rule.pattern.starts_with('<'))
        .then(|| rule.pattern.clone())
}

fn denial(call: &ToolCall, gate: &Gate, kind: &str, reason: Option<&str>, cwd: &Path) -> String {
    let mut identity = format!("'{}'", gate.surface);
    if kind != "policy" {
        identity.push_str(" call");
    }
    if call.name == "exec_command" {
        identity.push_str(" (invoked as 'exec_command')");
    }
    if call.name != gate.surface && call.name != "exec_command" {
        identity.push_str(&format!(" for tool '{}'", call.name));
    }
    if let Some(flagged) = &gate.flagged
        && flagged != &call.name
    {
        let label = if gate.surface == "mcp" {
            "target"
        } else {
            "path"
        };
        identity.push_str(&format!(
            " for {label} '{}'",
            flagged.chars().take(200).collect::<String>()
        ));
    }
    if let Some(pattern) = matched_pattern(&gate.rule) {
        identity.push_str(&format!(" (rule '{pattern}')"));
    }
    let boundary = if gate.boundary && kind != "unavailable" {
        format!(": outside working directory '{}'", cwd.display())
    } else {
        String::new()
    };
    let sentence = match kind {
        "policy" => format!("Denied by policy: {identity}{boundary}."),
        "unavailable" => {
            format!("This {identity} requires approval, but no interactive UI is available.")
        }
        "classifier" => format!("Auto mode blocked this {identity}{boundary}."),
        _ => format!("The user denied this {identity}{boundary}."),
    };
    format!(
        "{sentence}{}{}",
        reason
            .map(|reason| format!(" Reason: {}.", reason.trim_end_matches('.')))
            .unwrap_or_default(),
        if kind == "classifier" {
            " Choose a safer approach, or ask the user to switch modes or run it themselves."
        } else {
            ""
        }
    )
}

fn command_pattern(command: &str) -> String {
    let head = command.split_whitespace().next().unwrap_or("");
    format!("{head} *")
}

fn mcp_targets(call: &ToolCall, servers: &[String]) -> Vec<String> {
    let mut values = Vec::new();
    if let Some(rest) = call.name.strip_prefix("mcp__") {
        let configured = servers
            .iter()
            .filter(|server| rest.starts_with(&format!("{}__", sanitize(server))))
            .max_by_key(|server| sanitize(server).len());
        let (server, tool) = if let Some(server) = configured {
            (server.as_str(), &rest[sanitize(server).len() + 2..])
        } else {
            rest.split_once("__").unwrap_or((rest, ""))
        };
        for name in [server.to_owned(), sanitize(server)] {
            values.extend([format!("{name}_{tool}"), format!("{name}:{tool}"), name]);
        }
        values.extend([tool.into(), call.name.clone(), "mcp_call".into()]);
    } else {
        let server = call.arguments["server"].as_str().unwrap_or("");
        let tool = call.arguments["tool"].as_str();
        let describe = call.arguments["describe"].as_str();
        if let Some(reference) = tool.or(describe) {
            let qualified = reference
                .split_once(':')
                .filter(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains(':'));
            let server = if !server.is_empty() {
                server
            } else {
                qualified.map(|(server, _)| server).unwrap_or("")
            };
            let name = qualified.map(|(_, tool)| tool).unwrap_or(reference);
            if !server.is_empty() {
                if name.starts_with(&format!("{server}_")) {
                    values.push(name.into());
                } else {
                    values.extend([format!("{server}_{name}"), format!("{server}:{name}")]);
                }
                values.push(server.into());
            } else if let Some(server) = servers
                .iter()
                .filter(|server| name.starts_with(&format!("{server}_")))
                .max_by_key(|server| server.len())
            {
                values.extend([name.into(), server.clone()]);
            }
            values.extend([
                name.into(),
                reference.into(),
                if tool.is_some() {
                    "mcp_call"
                } else {
                    "mcp_describe"
                }
                .into(),
            ]);
        } else if let Some(connect) = call.arguments["connect"].as_str() {
            values.extend([
                format!("mcp_connect_{connect}"),
                connect.into(),
                "mcp_connect".into(),
            ]);
        } else if let Some(search) = call.arguments["search"].as_str() {
            if !server.is_empty() {
                values.extend([format!("mcp_server_{server}"), server.into()]);
            }
            values.extend([search.into(), "mcp_search".into()]);
        } else if !server.is_empty() {
            values.extend([
                format!("mcp_server_{server}"),
                server.into(),
                "mcp_list".into(),
            ]);
        } else {
            values.push("mcp_status".into());
        }
    }
    values.push("mcp".into());
    values
}

fn sanitize(value: &str) -> String {
    value
        .encode_utf16()
        .map(|c| {
            char::from_u32(u32::from(c))
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .unwrap_or('_')
        })
        .collect()
}

fn check(cancel: &CancellationToken) -> Result<(), ToolError> {
    if cancel.is_cancelled() {
        Err(ToolError("Operation aborted".into()))
    } else {
        Ok(())
    }
}

impl tiechui_plugin_api::SessionObserver for Permissions {
    fn observe(&self, _: &tiechui_plugin_api::SessionEvent) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.identity = None;
        sessions.rules.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiechui_plugin_api::{
        PermissionRule,
        protocol::{SessionEpoch, ThreadId, TurnId},
        serde_json::json,
        tools::ExecutionScope,
    };

    struct CapturingRequester {
        requests: Mutex<Vec<ApprovalRequest>>,
        decision: Mutex<ApprovalDecision>,
    }

    #[async_trait]
    impl ApprovalRequester for CapturingRequester {
        async fn request(
            &self,
            request: ApprovalRequest,
            _: CancellationToken,
        ) -> Result<ApprovalDecision, ToolError> {
            self.requests.lock().unwrap().push(request);
            Ok(self.decision.lock().unwrap().clone())
        }
    }

    fn test_context(cwd: &Path, thread: &str) -> ToolContext {
        ToolContext {
            invoker: None,
            cwd: cwd.to_path_buf(),
            branch: Arc::new(vec![]),
            parent_session: None,
            cancel: CancellationToken::new(),
            updates: Arc::new(|_| {}),
            append_entry: Arc::new(|_| Ok(())),
            epoch: SessionEpoch(0),
            scope: ExecutionScope {
                thread_id: ThreadId(thread.into()),
                turn_id: TurnId("turn".into()),
            },
            origin: Default::default(),
        }
    }

    #[tokio::test]
    async fn default_child_skips_disk_default_agent_policy_and_isolates_child_grants() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(home.join("agents")).unwrap();
        std::fs::create_dir_all(cwd.join(".agents/agents")).unwrap();
        std::fs::create_dir_all(cwd.join(".claude/agents")).unwrap();
        let cwd = cwd.canonicalize().unwrap();
        std::fs::write(
            home.join("agents/default.md"),
            "---\nname: default\ndescription: disk\npermission:\n  read: deny\n---\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join(".agents/agents/default.md"),
            "---\nname: default\ndescription: project\npermission:\n  ls: deny\n---\n",
        )
        .unwrap();
        std::fs::write(
            cwd.join(".claude/agents/default.md"),
            "---\nname: default\ndescription: claude\npermission:\n  grep: deny\n---\n",
        )
        .unwrap();

        let requester = Arc::new(CapturingRequester {
            requests: Mutex::new(vec![]),
            decision: Mutex::new(ApprovalDecision::AllowSession {
                both_directions: false,
            }),
        });
        let module =
            PermissionsModule::with_requester(cwd.clone(), home.clone(), requester.clone());
        let mut config = ModuleConfig::default();
        config.permissions.rules = Some(vec![
            PermissionRule {
                surface: "*".into(),
                pattern: "*".into(),
                action: Action::Allow,
                reason: None,
            },
            PermissionRule {
                surface: "write".into(),
                pattern: "*".into(),
                action: Action::Ask,
                reason: None,
            },
        ]);

        let mut unnamed_a = Registry::default();
        let mut unnamed_b = Registry::default();
        module
            .register_default_child(&mut unnamed_a, &config)
            .unwrap();
        module
            .register_default_child(&mut unnamed_b, &config)
            .unwrap();
        assert!(unnamed_a.runtime_extensions().is_empty());

        let call = |name: &str| ToolCall {
            id: name.into(),
            name: name.into(),
            arguments: json!({"path": "note.txt", "content": "x"}),
            opaque: None,
        };
        for tool in ["read", "ls", "grep"] {
            assert_eq!(
                unnamed_a.interceptors()[0]
                    .interceptor
                    .before(&call(tool), test_context(&cwd, "child-a"))
                    .await
                    .unwrap(),
                BeforeToolCall::Allow,
                "unnamed default child must not load {tool} rule from default.md"
            );
        }

        assert_eq!(
            unnamed_a.interceptors()[0]
                .interceptor
                .before(&call("write"), test_context(&cwd, "child-a"))
                .await
                .unwrap(),
            BeforeToolCall::Allow
        );
        assert_eq!(
            requester.requests.lock().unwrap()[0].actor.as_deref(),
            Some("Child default")
        );

        *requester.decision.lock().unwrap() = ApprovalDecision::Deny { reason: None };
        assert!(matches!(
            unnamed_b.interceptors()[0]
                .interceptor
                .before(&call("write"), test_context(&cwd, "child-b"))
                .await
                .unwrap(),
            BeforeToolCall::Deny(_)
        ));

        let mut named = Registry::default();
        module
            .register_child(&mut named, &config, &home, true, "default")
            .unwrap();
        assert!(matches!(
            named.interceptors()[0]
                .interceptor
                .before(&call("read"), test_context(&cwd, "named"))
                .await
                .unwrap(),
            BeforeToolCall::Deny(_)
        ));
    }
}
