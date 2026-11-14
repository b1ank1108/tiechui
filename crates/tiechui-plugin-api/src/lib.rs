//! Typed contributions registered in module order, then registration order.

mod actors;
mod compaction;
pub mod process;
pub use actors::*;
mod projection;
pub use projection::*;
mod session;
pub use compaction::*;
pub use session::*;
pub use tiechui_config::{CompactionConfig, NetworkConfig, PressureBenefit, redact_proxy_secrets};
pub use tiechui_config::{private, register_secret_env_var, scrubbed_secret_env_vars, user_home};
mod runtime;
pub use runtime::*;
mod paths;
pub use paths::resolve_tool_path;
mod inspection;
pub use inspection::*;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub use async_trait::async_trait;
pub use futures;
pub use serde_json;
pub use tiechui_config::Config as ModuleConfig;
pub use tiechui_config::McpServerConfig;
pub use tiechui_config::{PermissionAction, PermissionMode, PermissionRule, PermissionsConfig};
pub use tiechui_model as model;
pub use tiechui_protocol as protocol;
pub use tiechui_tools as tools;
pub use tiechui_tools::{Concurrency, Tool, ToolContext, ToolError, ToolInvoker, ToolOutput};

/// The session's permission mode, shared by the permissions module, its children and hooks.
#[derive(Debug, Clone, Default)]
pub struct SharedPermissionMode(Arc<std::sync::Mutex<PermissionMode>>);

impl SharedPermissionMode {
    pub fn get(&self) -> PermissionMode {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn set(&self, mode: PermissionMode) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = mode;
    }
}
pub use tokio_util::sync::CancellationToken;

use model::ToolCall;
use protocol::{CommandInfo, Event, InputContent, SessionEpoch, ThreadId};
use serde_json::Value;
use tools::ToolRegistry;

/// Session-resolved resources; called by the actor, never by the UI.
pub trait ResourceProvider: Send + Sync {
    fn resources(
        &self,
        active_tools: &[String],
        resolved: &[protocol::Resource],
    ) -> Vec<protocol::Resource>;
}

pub trait Module: Send + Sync {
    fn id(&self) -> &str;
    fn default_enabled(&self) -> bool {
        true
    }
    fn mandatory(&self) -> bool {
        false
    }
    fn register(&self, registry: &mut Registry, config: &ModuleConfig) -> Result<(), ToolError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnInput {
    pub cwd: PathBuf,
    pub content: Vec<InputContent>,
}

#[async_trait]
pub trait ContextProvider: Send + Sync {
    /// Presentation metadata only; fragments remain the source of request bytes.
    fn describe(&self, fragments: &[String]) -> Vec<PromptFragment> {
        fragments
            .iter()
            .map(|text| PromptFragment {
                label: "System prompt".into(),
                category: "System prompt".into(),
                source: "Registered context provider".into(),
                text: text.clone(),
                references: vec![],
            })
            .collect()
    }
    /// Preserve fragment bytes and order for unchanged inputs; do not add volatile metadata.
    async fn fragments(
        &self,
        input: &TurnInput,
        cancel: CancellationToken,
    ) -> Result<Vec<String>, ToolError>;
}

pub struct StaticContext(pub Vec<String>);

#[async_trait]
impl ContextProvider for StaticContext {
    async fn fragments(
        &self,
        _: &TurnInput,
        _: CancellationToken,
    ) -> Result<Vec<String>, ToolError> {
        Ok(self.0.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeforeToolCall {
    Allow,
    Deny(String),
    Rewrite(Value),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum InterceptorStage {
    #[default]
    BeforePolicy,
    Policy,
    AfterPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalVerdict {
    Allow,
    Deny(String),
}

#[async_trait]
pub trait ApprovalInterceptor: Send + Sync {
    async fn review(
        &self,
        call: &ToolCall,
        ctx: ToolContext,
    ) -> Result<Option<ApprovalVerdict>, ToolError>;
}

#[derive(Debug, Clone)]
pub struct LifecycleContext {
    pub session_id: String,
    pub turn_id: String,
    pub cwd: PathBuf,
    pub transcript_path: Option<PathBuf>,
    pub model: String,
}

#[derive(Debug, Clone)]
pub enum LifecycleEvent {
    BeforeRequest,
    SessionStart {
        resumed: bool,
    },
    UserInput {
        content: Vec<InputContent>,
    },
    BeforeCompaction {
        automatic: bool,
    },
    BeforeStop {
        active: bool,
        last_message: Option<String>,
    },
    SessionEnd,
}

#[derive(Debug, Default)]
pub struct LifecycleOutcome {
    pub context: Vec<String>,
    pub stop: Option<String>,
    pub block: Option<String>,
    pub diagnostics: Vec<String>,
}

#[async_trait]
pub trait LifecycleInterceptor: Send + Sync {
    async fn on_event(
        &self,
        event: &LifecycleEvent,
        ctx: &LifecycleContext,
        cancel: CancellationToken,
    ) -> Result<LifecycleOutcome, ToolError>;
}

#[async_trait]
pub trait ToolInterceptor: Send + Sync {
    /// Runs after every dispatch outcome, including denial, failure and cancellation.
    async fn settled(&self, _call: &ToolCall, _ctx: ToolContext) -> Result<(), ToolError> {
        Ok(())
    }
    fn stage(&self) -> InterceptorStage {
        InterceptorStage::BeforePolicy
    }
    async fn before(
        &self,
        _call: &ToolCall,
        _ctx: ToolContext,
    ) -> Result<BeforeToolCall, ToolError> {
        Ok(BeforeToolCall::Allow)
    }
    async fn after(
        &self,
        _call: &ToolCall,
        _output: &ToolOutput,
        _ctx: ToolContext,
    ) -> Result<Option<ToolOutput>, ToolError> {
        Ok(None)
    }
}

/// A slash command; its text or structured panel is shown to the user, never sent to the model.
#[async_trait]
pub trait Command: Send + Sync {
    fn info(&self) -> CommandInfo;
    fn allows_busy(&self, _arguments: &str) -> bool {
        false
    }
    async fn run(&self, arguments: &str, cancel: CancellationToken) -> Result<String, ToolError>;
    fn interactive(&self) -> bool {
        false
    }
    /// A short command which may retain its epoch-scoped host for background work.
    fn background(&self) -> bool {
        false
    }
    async fn input(
        &self,
        _data: Value,
        _session: CommandSession,
        _cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        Err(ToolError("Command input is unavailable".into()))
    }
    async fn session_run(
        &self,
        _arguments: &str,
        _session: CommandSession,
        _cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        Err(ToolError("Interactive command is unavailable".into()))
    }
    fn needs_session_view(&self) -> bool {
        false
    }
    async fn inspect(
        &self,
        arguments: &str,
        _session: SessionView,
        cancel: CancellationToken,
    ) -> Result<CommandResult, ToolError> {
        self.run(arguments, cancel).await.map(CommandResult::Text)
    }
}

/// Session-owned work that outlives the call which started it, such as background shells.
#[async_trait]
pub trait BackgroundWork: Send + Sync {
    /// Stops all current work and returns once it has settled.
    async fn quiesce(&self);
}

/// Synchronous observers must return promptly.
pub trait TurnObserver: Send + Sync {
    fn observe(&self, event: &Event);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEvent {
    Started {
        thread_id: ThreadId,
        epoch: SessionEpoch,
    },
    Ended {
        thread_id: ThreadId,
        epoch: SessionEpoch,
    },
}

/// Synchronous observers must return promptly.
pub trait SessionObserver: Send + Sync {
    fn observe(&self, event: &SessionEvent);
}

/// Declares roots only; discovery and project trust belong to the host.
pub trait SkillRoot: Send + Sync {
    fn owner(&self) -> Option<String> {
        None
    }
    fn paths(&self) -> Vec<PathBuf>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpOrigin {
    User(PathBuf),
    Project(PathBuf),
    Extension(String),
    Plugin {
        name: String,
        root: PathBuf,
        project: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpServerDefinition {
    pub name: String,
    pub config: McpServerConfig,
    pub origin: McpOrigin,
}

/// Contributes parsed definitions, retaining provenance for trust and diagnostics.
pub trait McpSource: Send + Sync {
    fn servers(&self) -> Vec<McpServerDefinition>;
}

#[derive(Debug, Clone)]
pub struct HookFile {
    pub path: PathBuf,
    /// JSON pointer for declarations embedded in a manifest.
    pub pointer: Option<String>,
}

/// Bundled hooks retain their package identity for approval and execution checks.
pub trait HookSource: Send + Sync + std::fmt::Debug {
    fn files(&self) -> Vec<HookFile>;
    fn root(&self) -> &std::path::Path;
    fn data_root(&self) -> &std::path::Path;
    fn content_hash(&self, cancel: &CancellationToken) -> Result<String, ToolError>;
}

#[derive(Debug, Clone)]
pub struct PluginDirectories {
    pub name: String,
    pub prompts: Vec<PathBuf>,
    pub agents: Vec<PathBuf>,
}

pub struct RegisteredInterceptor {
    pub interceptor: Arc<dyn ToolInterceptor>,
    pub timeout: Duration,
}

pub struct PresentationContext {
    pub tools: Vec<Arc<dyn Tool>>,
    pub window: u64,
    pub estimate_tools: fn(&[model::ToolSpec]) -> u64,
}

pub trait ToolPresentation: Send + Sync {
    /// The definitions this request carries; every tool stays callable.
    fn present(&self, ctx: PresentationContext) -> Vec<model::ToolSpec>;
}

#[derive(Default)]
pub struct Registry {
    presentation: Option<Arc<dyn ToolPresentation>>,
    actors: Vec<Arc<dyn ActorClient>>,
    projections: Vec<Arc<dyn RequestProjection>>,
    compactors: Vec<Arc<dyn Compactor>>,
    runtime: Vec<Arc<dyn RuntimeExtension>>,
    tools: ToolRegistry,
    context: Vec<Arc<dyn ContextProvider>>,
    interceptors: Vec<RegisteredInterceptor>,
    turns: Vec<Arc<dyn TurnObserver>>,
    sessions: Vec<Arc<dyn SessionObserver>>,
    skills: Vec<Arc<dyn SkillRoot>>,
    mcp: Vec<Arc<dyn McpSource>>,
    hooks: Vec<Arc<dyn HookSource>>,
    plugin_directories: Vec<PluginDirectories>,
    approvals: Vec<Arc<dyn ApprovalInterceptor>>,
    lifecycle: Vec<Arc<dyn LifecycleInterceptor>>,
    commands: Vec<Arc<dyn Command>>,
    background: Vec<Arc<dyn BackgroundWork>>,
    resources: Vec<Arc<dyn ResourceProvider>>,
    module_states: Vec<(String, bool)>,
}

impl Registry {
    pub fn register_presentation(
        &mut self,
        presentation: Arc<dyn ToolPresentation>,
    ) -> Result<(), ToolError> {
        if self.presentation.is_some() {
            return Err(ToolError("Tool presentation already registered".into()));
        }
        self.presentation = Some(presentation);
        Ok(())
    }
    pub fn presentation(&self) -> Option<&Arc<dyn ToolPresentation>> {
        self.presentation.as_ref()
    }
    pub fn module_states(&self) -> &[(String, bool)] {
        &self.module_states
    }
    pub fn register_resources(&mut self, provider: Arc<dyn ResourceProvider>) {
        self.resources.push(provider);
    }
    /// Later contributions resolve first; earlier providers may group their metadata.
    pub fn resources(&self) -> Vec<protocol::Resource> {
        let active = self
            .tools
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect::<Vec<_>>();
        let mut resources = Vec::new();
        for provider in self.resources.iter().rev() {
            resources.extend(provider.resources(&active, &resources));
        }
        resources.truncate(1200);
        resources
    }
    pub fn register_actor_client(&mut self, client: Arc<dyn ActorClient>) {
        self.actors.push(client);
    }
    pub fn actor_clients(&self) -> &[Arc<dyn ActorClient>] {
        &self.actors
    }
    pub fn restrict_tools(&mut self, names: Vec<String>) {
        self.tools.restrict(names);
    }
    pub fn register_projection(&mut self, projection: Arc<dyn RequestProjection>) {
        self.projections.push(projection);
    }
    pub fn projections(&self) -> &[Arc<dyn RequestProjection>] {
        &self.projections
    }
    pub fn register_compactor(&mut self, compactor: Arc<dyn Compactor>) {
        self.compactors.push(compactor);
    }
    pub fn compactors(&self) -> &[Arc<dyn Compactor>] {
        &self.compactors
    }
    pub fn register_runtime(&mut self, extension: Arc<dyn RuntimeExtension>) {
        self.runtime.push(extension);
    }
    pub fn runtime_extensions(&self) -> &[Arc<dyn RuntimeExtension>] {
        &self.runtime
    }
    pub fn from_modules(modules: &[&dyn Module], config: &ModuleConfig) -> Result<Self, ToolError> {
        let mut registry = Self::default();
        let mut ids = std::collections::BTreeSet::new();
        for module in modules {
            if !ids.insert(module.id()) {
                return Err(ToolError(format!(
                    "Module already registered: {}",
                    module.id()
                )));
            }
            let enabled = config
                .modules
                .get(module.id())
                .copied()
                .unwrap_or_else(|| module.default_enabled());
            registry.module_states.push((module.id().into(), enabled));
            if enabled {
                module.register(&mut registry, config)?;
            }
        }
        Ok(registry)
    }

    pub fn register_tool(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        self.tools.register(tool)
    }
    pub fn register_tool_source(&mut self, source: Arc<dyn tools::ToolSource>) {
        self.tools.register_source(source);
    }
    pub fn register_context_provider(&mut self, provider: Arc<dyn ContextProvider>) {
        self.context.push(provider);
    }
    pub fn register_tool_interceptor(
        &mut self,
        interceptor: Arc<dyn ToolInterceptor>,
        timeout: Duration,
    ) {
        self.interceptors.push(RegisteredInterceptor {
            interceptor,
            timeout,
        });
        self.interceptors
            .sort_by_key(|entry| entry.interceptor.stage());
    }
    pub fn register_approval_interceptor(&mut self, interceptor: Arc<dyn ApprovalInterceptor>) {
        self.approvals.push(interceptor);
    }
    pub fn approval_interceptors(&self) -> &[Arc<dyn ApprovalInterceptor>] {
        &self.approvals
    }
    pub fn register_lifecycle_interceptor(&mut self, interceptor: Arc<dyn LifecycleInterceptor>) {
        self.lifecycle.push(interceptor);
    }
    pub fn lifecycle_interceptors(&self) -> &[Arc<dyn LifecycleInterceptor>] {
        &self.lifecycle
    }
    /// The first registration of a name wins.
    pub fn register_command(&mut self, command: Arc<dyn Command>) {
        let name = command.info().name;
        if !self.commands.iter().any(|c| c.info().name == name) {
            self.commands.push(command);
        }
    }
    pub fn commands(&self) -> &[Arc<dyn Command>] {
        &self.commands
    }
    pub fn register_background_work(&mut self, work: Arc<dyn BackgroundWork>) {
        self.background.push(work);
    }
    pub fn background_work(&self) -> &[Arc<dyn BackgroundWork>] {
        &self.background
    }
    pub fn register_turn_observer(&mut self, observer: Arc<dyn TurnObserver>) {
        self.turns.push(observer);
    }
    pub fn register_session_observer(&mut self, observer: Arc<dyn SessionObserver>) {
        self.sessions.push(observer);
    }
    pub fn register_skill_root(&mut self, root: Arc<dyn SkillRoot>) {
        self.skills.push(root);
    }
    pub fn register_mcp_source(&mut self, source: Arc<dyn McpSource>) {
        self.mcp.push(source);
    }
    pub fn register_hook_source(&mut self, source: Arc<dyn HookSource>) {
        self.hooks.push(source);
    }
    pub fn hook_sources(&self) -> &[Arc<dyn HookSource>] {
        &self.hooks
    }
    pub fn register_plugin_directories(&mut self, directories: PluginDirectories) {
        self.plugin_directories.push(directories);
    }
    pub fn plugin_directories(&self) -> &[PluginDirectories] {
        &self.plugin_directories
    }
    pub fn tools(&self) -> &ToolRegistry {
        &self.tools
    }
    pub fn context_providers(&self) -> &[Arc<dyn ContextProvider>] {
        &self.context
    }
    pub fn interceptors(&self) -> &[RegisteredInterceptor] {
        &self.interceptors
    }
    pub fn turn_observers(&self) -> &[Arc<dyn TurnObserver>] {
        &self.turns
    }
    pub fn session_observers(&self) -> &[Arc<dyn SessionObserver>] {
        &self.sessions
    }
    pub fn skill_roots(&self) -> &[Arc<dyn SkillRoot>] {
        &self.skills
    }
    pub fn mcp_sources(&self) -> &[Arc<dyn McpSource>] {
        &self.mcp
    }
}
