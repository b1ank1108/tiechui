use crate::{
    Plugin,
    rpc::{self, Peer},
};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tiechui_plugin_api::{
    model::{ToolCall, ToolSpec},
    protocol::*,
    serde_json::{self, Value, json},
    tools::{ToolSource, ToolUpdate},
    *,
};

#[async_trait]
pub trait PluginHost: Send + Sync {
    async fn ask(
        &self,
        plugin: &str,
        prompt: PluginPrompt,
        cancel: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError>;
}

struct State {
    plugin: Plugin,
    declaration: PluginInitialize,
    hash: RwLock<Option<String>>,
    freshness: Arc<tokio::sync::Mutex<crate::freshness::Freshness>>,
    peer: RwLock<Option<Arc<Peer>>>,
    authorized: AtomicBool,
    cancel: CancellationToken,
    startup: CancellationToken,
    finished: CancellationToken,
    calls: Mutex<CancellationToken>,
    events: Mutex<Vec<(String, Value)>>,
    host: Option<Arc<dyn PluginHost>>,
}

struct Lifetime(CancellationToken);
impl Drop for Lifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[derive(Clone)]
pub struct Extension {
    state: Arc<State>,
    _lifetime: Arc<Lifetime>,
}

impl Extension {
    pub fn start(
        plugin: &Plugin,
        home: &Path,
        trusted: bool,
        host: Option<Arc<dyn PluginHost>>,
        cancel: CancellationToken,
    ) -> Option<Self> {
        let extension = plugin.extension.as_ref()?;
        if !trusted || !plugin.enabled {
            return None;
        }
        let cancel = cancel.child_token();
        let state = Arc::new(State {
            plugin: plugin.clone(),
            declaration: PluginInitialize {
                api_version: 1,
                capabilities: extension.capabilities.clone(),
                contributions: extension.contributions.clone(),
            },
            hash: RwLock::default(),
            freshness: Arc::default(),
            peer: RwLock::default(),
            authorized: AtomicBool::new(false),
            calls: Mutex::new(cancel.child_token()),
            cancel: cancel.clone(),
            startup: CancellationToken::new(),
            finished: CancellationToken::new(),
            events: Mutex::default(),
            host,
        });
        let module = Self {
            state: state.clone(),
            _lifetime: Arc::new(Lifetime(cancel)),
        };
        let home = home.to_owned();
        tokio::spawn(async move {
            if let Err(error) = state.run(&home).await {
                state.status(format!("failed: {error}"));
            }
            state.startup.cancel();
            *state.peer.write().unwrap_or_else(|e| e.into_inner()) = None;
            state.finished.cancel();
        });
        Some(module)
    }

    pub async fn wait_for_startup(&self, cancel: CancellationToken) {
        tokio::select! {
            _ = cancel.cancelled() => {},
            _ = self.state.startup.cancelled() => {},
        }
    }
}

impl State {
    fn status(&self, text: String) {
        let status = PluginStatus {
            plugin: self.plugin.name.clone(),
            text,
        };
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        // Only the newest status is relevant, even if a plugin floods updates.
        events.retain(|(name, _)| name != PLUGIN_EVENT);
        events.push((PLUGIN_EVENT.into(), json!(status)));
    }

    async fn content_hash(
        &self,
        force: bool,
        cancel: &CancellationToken,
    ) -> Result<String, ToolError> {
        let root = self.plugin.root.clone();
        let hashing = cancel.child_token();
        // Dropping a cancelled call must also stop its blocking filesystem worker.
        let _guard = hashing.clone().drop_guard();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ToolError("Request cancelled".into())),
            _ = self.cancel.cancelled() => Err(ToolError("Plugin stopped".into())),
            result = async {
                let mut freshness = self.freshness.clone().lock_owned().await;
                tokio::task::spawn_blocking(move || freshness.hash(&root, force, &hashing))
                    .await.map_err(rpc::error)?
            } => result,
        }
    }

    async fn fresh(&self, force: bool, cancel: &CancellationToken) -> Result<(), ToolError> {
        let hash = self.content_hash(force, cancel).await;
        crate::check(cancel)?;
        crate::check(&self.cancel)?;
        if hash.as_ref().ok() != self.hash.read().unwrap_or_else(|e| e.into_inner()).as_ref()
            || hash.is_err()
        {
            self.cancel.cancel();
            self.status("revoked: package contents changed; reload to authorize".into());
            return Err(ToolError("Plugin content grant revoked".into()));
        }
        Ok(())
    }

    async fn authorize(&self, home: &Path) -> Result<bool, ToolError> {
        let hash = self.content_hash(true, &self.cancel).await?;
        *self.hash.write().unwrap_or_else(|e| e.into_inner()) = Some(hash.clone());
        let grant = json!({"plugin":self.plugin.name,"root":self.plugin.root,"hash":hash,"capabilities":self.declaration.capabilities});
        let path = home
            .join("plugin-grants")
            .join(format!("{}.json", self.plugin.name));
        let parent = path
            .parent()
            .ok_or_else(|| ToolError("Missing grant directory".into()))?;
        if std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .as_ref()
            == Some(&grant)
        {
            private::create_dir_all(parent)?;
            return Ok(true);
        }
        let Some(host) = &self.host else {
            self.status("disabled: capabilities not granted".into());
            return Ok(false);
        };
        self.status("awaiting capability approval".into());
        let prompt = PluginPrompt::Confirm {
            title: format!("Authorize plugin {}", self.plugin.name),
            text: format!(
                "Package: {}\nContent hash: {hash}\nCapabilities: {}\nAllow this package to execute with these capabilities?",
                self.plugin.root.display(),
                serde_json::to_string(&self.declaration.capabilities).map_err(rpc::error)?
            ),
        };
        let answer = host
            .ask(&self.plugin.name, prompt, self.cancel.clone())
            .await?;
        if !matches!(answer, ApprovalDecision::Accept { .. }) {
            self.status("disabled: capabilities declined".into());
            return Ok(false);
        }
        self.fresh(false, &self.cancel).await?;
        private::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        use std::io::Write;
        file.write_all(&serde_json::to_vec_pretty(&grant).map_err(rpc::error)?)?;
        file.persist(path).map_err(rpc::error)?;
        Ok(true)
    }

    async fn run(self: &Arc<Self>, home: &Path) -> Result<(), ToolError> {
        if !self.authorize(home).await? {
            return Ok(());
        }
        self.authorized.store(true, Ordering::Release);
        let extension = self
            .plugin
            .extension
            .as_ref()
            .ok_or_else(|| ToolError("Missing extension".into()))?;
        for attempt in 0..3 {
            if attempt > 0 {
                self.status(format!("restarting (attempt {})", attempt + 1));
                tokio::select! {
                    _ = self.cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(Duration::from_millis(if attempt == 1 { 100 } else { 400 })) => {},
                }
            }
            self.fresh(true, &self.cancel).await?;
            self.status("starting".into());
            let connection = self.cancel.child_token();
            let mut spawned = None;
            let result = async {
                let group = process::ProcessGroup::spawn(&self.plugin.root.join(&extension.command), &extension.args, &self.plugin.root, &[], &BTreeMap::new())?;
                let peer = Peer::start(group, self.clone(), connection.clone())?;
                spawned = Some(peer.clone());
                process::deadline(Duration::from_secs(10), connection.clone(), async {
                let initialized = peer.request("initialize", json!(self.declaration), process::seconds(None, 10.0)?, connection.clone(), None).await?;
                if serde_json::from_value::<PluginInitialize>(initialized).map_err(rpc::error)? != self.declaration {
                    return Err(ToolError("Plugin API version or contributions mismatch (-32002)".into()));
                }
                for (capability, method, expected) in [
                    (PluginCapability::Tools, "tools/list", json!(self.declaration.contributions.tools)),
                    (PluginCapability::Commands, "commands/list", json!(self.declaration.contributions.commands)),
                ] {
                    if self.declaration.capabilities.contains(&capability) && peer.request(method, json!({}), Duration::from_secs(10), connection.clone(), None).await? != expected {
                        return Err(ToolError(format!("Plugin {method} differs from its declaration")));
                    }
                }
                    Ok(())
                }).await?;
                *self.peer.write().unwrap_or_else(|e| e.into_inner()) = Some(peer.clone());
                self.status("ready".into());
                self.startup.cancel();
                tokio::select! {
                    _ = self.cancel.cancelled() => {},
                    _ = peer.stopped.cancelled() => return Err(ToolError("Plugin process disconnected".into())),
                }
                Ok(())
            }.await;
            *self.peer.write().unwrap_or_else(|e| e.into_inner()) = None;
            connection.cancel();
            if let Some(peer) = spawned {
                peer.stopped.cancelled().await;
            }
            if self.cancel.is_cancelled() {
                return Ok(());
            }
            if let Err(error) = result {
                self.status(format!("failed: {error}"));
                if attempt == 2 {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    async fn call(
        &self,
        method: &str,
        params: Value,
        deadline: Option<Duration>,
        cancel: CancellationToken,
        progress: Option<Arc<dyn Fn(String) + Send + Sync>>,
    ) -> Result<Value, ToolError> {
        self.fresh(false, &cancel).await?;
        let peer = self
            .peer
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| ToolError("Plugin is not ready".into()))?;
        let calls = self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(ToolError("Plugin stopped".into())),
            _ = calls.cancelled() => Err(ToolError("Plugin call interrupted".into())),
            result = async {
                let request = peer.request(method, params, Duration::from_secs(60), cancel.clone(), progress);
                match deadline {
                    Some(timeout) => process::deadline(timeout, cancel, request).await,
                    None => request.await,
                }
            } => result,
        }
    }
}

#[async_trait]
impl rpc::Handler for State {
    async fn request(
        &self,
        method: rpc::Method,
        cancel: CancellationToken,
    ) -> Result<Value, (i32, String)> {
        if self
            .peer
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            return Err((-32001, "Plugin is not initialized".into()));
        }
        self.fresh(false, &cancel)
            .await
            .map_err(|e| (-32001, e.0))?;
        let capability = match method.method.as_str() {
            "ui.notify" => PluginCapability::Notify,
            "ui.select" => PluginCapability::Select,
            "ui.confirm" => PluginCapability::Confirm,
            "ui.input" => PluginCapability::Input,
            "panel.show" => PluginCapability::Panel,
            "status.set" => PluginCapability::Status,
            _ => return Err((-32601, "Unknown plugin method".into())),
        };
        if !self.declaration.capabilities.contains(&capability) {
            return Err((-32001, "Capability not granted".into()));
        }
        if capability == PluginCapability::Notify {
            let PluginRequest::Notify { title, text } =
                serde_json::from_value(json!({"method":"ui.notify","params":method.params}))
                    .map_err(|e| (-32602, e.to_string()))?
            else {
                return Err((-32602, "Expected notification".into()));
            };
            let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
            events.push((
                PLUGIN_NOTICE_EVENT.into(),
                json!(format!("{}: {title}: {text}", self.plugin.name)),
            ));
            return Ok(Value::Null);
        }
        if capability == PluginCapability::Status {
            let text = method.params["text"]
                .as_str()
                .ok_or_else(|| (-32602, "Missing status text".into()))?;
            self.status(text.into());
            return Ok(Value::Null);
        }
        let prompt = if capability == PluginCapability::Panel {
            let panel: PluginPanel =
                serde_json::from_value(method.params).map_err(|e| (-32602, e.to_string()))?;
            validate_panel(&panel).map_err(|e| (-32602, e.0))?;
            PluginPrompt::Panel { panel }
        } else {
            let mut params = method.params;
            let object = params
                .as_object_mut()
                .ok_or_else(|| (-32602, "Expected object".into()))?;
            object.insert(
                "kind".into(),
                method.method.trim_start_matches("ui.").into(),
            );
            serde_json::from_value(params).map_err(|e| (-32602, e.to_string()))?
        };
        if matches!(&prompt, PluginPrompt::Select { options, .. } if options.is_empty() || options.len() > 256)
        {
            return Err((-32602, "Select requires 1..256 options".into()));
        }
        let Some(host) = &self.host else {
            return Ok(json!(ApprovalDecision::Cancel));
        };
        let calls = self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        tokio::select! {
            biased;
            _ = calls.cancelled() => Err((-32800, "Turn interrupted".into())),
            result = host.ask(&self.plugin.name, prompt, cancel) => result.map(|v| json!(v)).map_err(|e| (-32000, e.0)),
        }
    }
}

pub fn validate_panel(panel: &PluginPanel) -> Result<(), ToolError> {
    fn nodes_valid(
        nodes: &[PanelNode],
        depth: usize,
        count: &mut usize,
        ids: &mut std::collections::BTreeSet<String>,
    ) -> bool {
        if depth > 16 {
            return false;
        }
        for node in nodes {
            *count += 1;
            match node {
                PanelNode::List { children } if !nodes_valid(children, depth + 1, count, ids) => {
                    return false;
                }
                PanelNode::Form { fields } => {
                    *count += fields.len();
                    for field in fields {
                        if field.id.is_empty() || !ids.insert(field.id.clone()) {
                            return false;
                        }
                    }
                }
                PanelNode::Progress { value, total, .. } if *total == 0 || value > total => {
                    return false;
                }
                _ => {}
            }
            if *count > 256 {
                return false;
            }
        }
        true
    }
    if !nodes_valid(&panel.nodes, 0, &mut 0, &mut Default::default()) {
        return Err(ToolError("Invalid panel tree".into()));
    }
    Ok(())
}

impl Module for Extension {
    fn id(&self) -> &str {
        &self.state.plugin.name
    }
    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        registry.register_tool_source(Arc::new(self.clone()));
        registry.register_runtime(Arc::new(self.clone()));
        registry.register_lifecycle_interceptor(Arc::new(self.clone()));
        for command in &self.state.declaration.contributions.commands {
            registry.register_command(Arc::new(ExtensionCommand(self.clone(), command.clone())));
        }
        for stage in &self.state.declaration.contributions.interceptors {
            registry.register_tool_interceptor(
                Arc::new(Interceptor(self.clone(), *stage)),
                Duration::from_secs(2),
            );
        }
        Ok(())
    }
}

impl RuntimeExtension for Extension {
    fn event(&self, event: &RuntimeEvent, _: &BranchView) -> Result<RuntimeOutput, ToolError> {
        match event {
            RuntimeEvent::Interrupt | RuntimeEvent::Settled(TurnStatus::Interrupted) => {
                let mut calls = self.state.calls.lock().unwrap_or_else(|e| e.into_inner());
                calls.cancel();
                *calls = self.state.cancel.child_token();
            }
            RuntimeEvent::Shutdown => self.state.cancel.cancel(),
            _ => {}
        }
        Ok(RuntimeOutput {
            events: std::mem::take(
                &mut *self.state.events.lock().unwrap_or_else(|e| e.into_inner()),
            ),
            ..Default::default()
        })
    }
}

#[async_trait]
impl LifecycleInterceptor for Extension {
    async fn on_event(
        &self,
        event: &LifecycleEvent,
        _: &LifecycleContext,
        cancel: CancellationToken,
    ) -> Result<LifecycleOutcome, ToolError> {
        if matches!(event, LifecycleEvent::SessionEnd) {
            self.state.cancel.cancel();
            tokio::select! {
                _ = cancel.cancelled() => {},
                _ = self.state.finished.cancelled() => {},
            }
        }
        Ok(LifecycleOutcome::default())
    }
}

fn name(plugin: &str, contribution: &str) -> String {
    format!("plugin_{plugin}_{contribution}")
}
impl ToolSource for Extension {
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        if self
            .state
            .peer
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        {
            return vec![];
        }
        self.state
            .declaration
            .contributions
            .tools
            .iter()
            .map(|tool| {
                Arc::new(ExtensionTool {
                    extension: self.clone(),
                    name: tool.name.clone(),
                    spec: ToolSpec {
                        name: name(&self.state.plugin.name, &tool.name),
                        description: tool.description.clone(),
                        input_schema: tool.input_schema.clone(),
                    },
                }) as Arc<dyn Tool>
            })
            .collect()
    }
}

struct ExtensionTool {
    extension: Extension,
    name: String,
    spec: ToolSpec,
}
fn output(value: Value) -> Result<ToolOutput, ToolError> {
    let value: PluginOutput = serde_json::from_value(value).map_err(rpc::error)?;
    Ok(ToolOutput {
        is_error: value.is_error,
        ..ToolOutput::text(value.text)
    })
}
#[async_trait]
impl Tool for ExtensionTool {
    fn deferrable(&self) -> bool {
        true
    }
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn source(&self) -> Option<String> {
        Some(format!("Plugin: {}", self.extension.id()))
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Shared
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let progress = Arc::new(move |text| {
            (ctx.updates)(ToolUpdate {
                call_id: call.id.clone(),
                epoch: ctx.epoch,
                output: ToolOutput::text(text),
            })
        });
        output(
            self.extension
                .state
                .call(
                    "tools/call",
                    json!({"name":self.name,"arguments":call.arguments}),
                    None,
                    ctx.cancel,
                    Some(progress),
                )
                .await?,
        )
    }
}

struct ExtensionCommand(Extension, PluginCommand);
#[async_trait]
impl Command for ExtensionCommand {
    fn interactive(&self) -> bool {
        true
    }
    async fn session_run(
        &self,
        arguments: &str,
        _: CommandSession,
        cancel: CancellationToken,
    ) -> Result<String, ToolError> {
        self.run(arguments, cancel).await
    }
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: name(self.0.id(), &self.1.name),
            description: self.1.description.clone(),
            argument_hint: None,
        }
    }
    async fn run(&self, arguments: &str, cancel: CancellationToken) -> Result<String, ToolError> {
        let result = self
            .0
            .state
            .call(
                "commands/run",
                json!({"name":self.1.name,"arguments":arguments}),
                None,
                cancel,
                None,
            )
            .await?;
        result
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| ToolError("Plugin command result must be text".into()))
    }
}

struct Interceptor(Extension, PluginStage);
#[async_trait]
impl ToolInterceptor for Interceptor {
    fn stage(&self) -> InterceptorStage {
        match self.1 {
            PluginStage::BeforePolicy => InterceptorStage::BeforePolicy,
            PluginStage::Policy => InterceptorStage::Policy,
            PluginStage::AfterPolicy => InterceptorStage::AfterPolicy,
        }
    }
    async fn before(&self, call: &ToolCall, ctx: ToolContext) -> Result<BeforeToolCall, ToolError> {
        if !self.0.state.authorized.load(Ordering::Acquire) {
            return Ok(BeforeToolCall::Allow);
        }
        let result = self
            .0
            .state
            .call(
                "tool.intercept",
                json!({"phase":"before","stage":self.1,"call":call}),
                Some(Duration::from_secs(2)),
                ctx.cancel,
                None,
            )
            .await?;
        Ok(
            match serde_json::from_value::<PluginVerdict>(result).map_err(rpc::error)? {
                PluginVerdict::Allow => BeforeToolCall::Allow,
                PluginVerdict::Deny { reason } => BeforeToolCall::Deny(reason),
                PluginVerdict::Rewrite { arguments } => BeforeToolCall::Rewrite(arguments),
            },
        )
    }
    async fn after(
        &self,
        call: &ToolCall,
        result: &ToolOutput,
        ctx: ToolContext,
    ) -> Result<Option<ToolOutput>, ToolError> {
        if !self.0.state.authorized.load(Ordering::Acquire) {
            return Ok(None);
        }
        let result = self
            .0
            .state
            .call(
                "tool.intercept",
                json!({"phase":"after","stage":self.1,"call":call,"output":result}),
                Some(Duration::from_secs(2)),
                ctx.cancel,
                None,
            )
            .await?;
        if result.is_null() {
            Ok(None)
        } else {
            output(result).map(Some)
        }
    }
}
