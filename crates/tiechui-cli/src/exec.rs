use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;

use clap::ArgMatches;
use tiechui_config::{ConfigPaths, IgnoredReason, TrustStore};
use tiechui_core::{BoundedRetry, Interactions, SessionActor, SessionConfig, SessionHandle};
use tiechui_model::{ModelId, ReasoningLevel};
use tiechui_module_context_files::ContextFilesModule;
use tiechui_module_mcp::DeclineElicitation;
use tiechui_module_permissions::PermissionsModule;
use tiechui_plugin_api::{Module, PermissionMode, StaticContext};
use tiechui_protocol::*;
use tiechui_providers::{ErrorKind, ModelProvider};
use tiechui_session::{EntryKind, ModelRef, SessionStore};

use crate::approvals::{Prompts, Requester};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) type Failure = (i32, String);

pub(crate) const BASE_SYSTEM_PROMPT: &str = "You are Tiechui, a coding assistant. Use the available tools to complete the user's task. Read existing files before editing them. Keep answers concise.";
const MCP_STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

fn config_error(error: impl ToString) -> Failure {
    (2, error.to_string())
}
fn runtime_error(error: impl ToString) -> Failure {
    (1, error.to_string())
}

pub(crate) async fn run(args: &ArgMatches) -> i32 {
    let (signals, mut interrupted) = mpsc::unbounded_channel();
    let listener = tokio::spawn(async move {
        let mut count = 0;
        loop {
            match tokio::signal::ctrl_c().await {
                Ok(()) => {
                    count += 1;
                    if count == 2 {
                        std::process::exit(130);
                    }
                    let _ = signals.send(Ok(()));
                }
                Err(error) => {
                    let _ = signals.send(Err(error));
                    break;
                }
            }
        }
    });
    let result = execute(args, &mut interrupted).await;
    listener.abort();
    match result {
        Ok(code) => code,
        Err((code, message)) => {
            eprintln!("chui: {message}");
            code
        }
    }
}

/// What the command line, or an app-server client, asks of the sessions it opens.
#[derive(Debug, Clone, Default)]
pub(crate) struct Options {
    pub cwd: Option<PathBuf>,
    pub resume: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub mode: Option<String>,
    pub approval: Option<String>,
}

impl Options {
    /// Arguments the subcommand does not define are absent.
    pub(crate) fn from_args(args: &ArgMatches) -> Self {
        let get = |id: &str| args.try_get_one::<String>(id).ok().flatten().cloned();
        let yolo = args.try_get_one::<bool>("yolo").ok().flatten().copied() == Some(true);
        Self {
            cwd: get("cwd").map(PathBuf::from),
            resume: get("resume"),
            model: get("model"),
            thinking: get("thinking"),
            mode: if yolo {
                Some("yolo".into())
            } else {
                get("mode")
            },
            approval: get("approval"),
        }
    }
}

/// The working directory as given, its canonical form, the config paths, and the Tiechui home.
pub(crate) fn locations(
    options: &Options,
) -> Result<(PathBuf, PathBuf, ConfigPaths, PathBuf), Failure> {
    locations_at(match &options.cwd {
        Some(path) => path.clone(),
        None => std::env::current_dir().map_err(config_error)?,
    })
}

pub(crate) fn locations_at(
    given: PathBuf,
) -> Result<(PathBuf, PathBuf, ConfigPaths, PathBuf), Failure> {
    let given = tiechui_session::resolve_cwd(&given).map_err(config_error)?;
    let cwd = given
        .canonicalize()
        .map_err(|error| config_error(format!("{}: {error}", given.display())))?;
    let paths = ConfigPaths::discover(&cwd).map_err(config_error)?;
    let home = paths
        .user
        .parent()
        .ok_or_else(|| config_error("missing config directory"))?
        .to_owned();
    Ok((given, cwd, paths, home))
}

/// Which session `prepare` opens.
pub(crate) enum Open {
    /// `--resume <ID>` when given, otherwise a new session, in `--cwd`.
    Args,
    /// A new session in this directory.
    New(PathBuf),
    /// This session file, in its own working directory.
    Path(PathBuf),
}

pub(crate) struct Prepared {
    pub handle: SessionHandle,
    /// The session's thread id.
    pub id: String,
    pub tui: tiechui_config::TuiConfig,
    /// The working directory as given.
    pub cwd: PathBuf,
    pub path: PathBuf,
}

/// The running session and the loaded `[tui]` settings. Problems that do not stop the
/// session are added to `warnings`, also when a later one does.
/// An interactive session asks the client through its interaction queue; others use the
/// headless approval policy and decline elicitations.
pub(crate) async fn prepare(
    options: &Options,
    cancel: CancellationToken,
    interactive: bool,
    open: Open,
    epoch: SessionEpoch,
    schedules: &tiechui_module_loop::Schedules,
    warnings: &mut Vec<String>,
) -> Result<Prepared, Failure> {
    let (interactions, inbox) = Interactions::new();
    let (context_files_cwd, cwd, paths, home) = match &open {
        Open::Args => locations(options)?,
        Open::New(cwd) => locations_at(cwd.clone())?,
        Open::Path(path) => {
            let (path, header_cancel) = (path.clone(), cancel.clone());
            let cwd = tokio::task::spawn_blocking(move || {
                SessionStore::open_read_only(&path, &header_cancel)
                    .map(|store| store.header().cwd.clone())
            })
            .await
            .map_err(runtime_error)?
            .map_err(runtime_error)?;
            locations_at(cwd)?
        }
    };
    let given = context_files_cwd.clone();
    let trust_home = home.clone();
    let trust_cwd = cwd.clone();
    let trust_cancel = cancel.clone();
    let project_config = paths.project.is_some();
    let (trusted, agent_trusted) = tokio::task::spawn_blocking(move || {
        let requires_trust =
            project_config || super::trust::has_project_resources(&trust_cwd, &trust_home);
        let allowed = TrustStore::new(&trust_home)
            .get(&trust_cwd, &trust_cancel)
            .map_err(config_error)?
            == Some(true);
        Ok::<_, Failure>((!requires_trust || allowed, allowed))
    })
    .await
    .map_err(runtime_error)??;
    if !trusted {
        warnings.push(format!(
            "trust: skipping untrusted project content in {}; use chui trust --cwd {} allow",
            cwd.display(),
            cwd.display()
        ));
    }
    let approval = if interactive {
        None
    } else {
        options.approval.as_deref()
    };
    let user_home = tiechui_config::user_home().unwrap_or_else(|| home.clone());
    let permissions = if interactive {
        PermissionsModule::with_requester(
            cwd.clone(),
            user_home,
            Arc::new(Requester(interactions.clone())),
        )
    } else {
        PermissionsModule::headless(
            cwd.clone(),
            user_home,
            if approval == Some("deny") {
                tiechui_module_permissions::HeadlessPolicy::Deny
            } else {
                tiechui_module_permissions::HeadlessPolicy::Unavailable
            },
        )
    };
    let permissions = permissions.with_sources(&paths.user, paths.project.as_deref(), trusted);
    let mandatory = if permissions.mandatory() {
        vec![permissions.id()]
    } else {
        vec![]
    };
    let mut cli = toml::Table::new();
    for (key, value) in [("model", &options.model), ("thinking", &options.thinking)] {
        if let Some(value) = value {
            cli.insert(key.into(), toml::Value::String(value.clone()));
        }
    }
    let loaded = tiechui_config::load_with_mandatory_modules(&paths, trusted, &cli, &mandatory)
        .map_err(config_error)?;
    permissions.mode().set(
        match options.mode.as_deref() {
            Some("auto") => Some(PermissionMode::Auto),
            Some("yolo") => Some(PermissionMode::Yolo),
            Some(_) => Some(PermissionMode::Rules),
            None => (approval == Some("yolo")).then_some(PermissionMode::Yolo),
        }
        .or(loaded.config.permissions.mode)
        .unwrap_or_default(),
    );
    for diagnostic in loaded.diagnostics {
        if diagnostic.reason == IgnoredReason::UntrustedProject {
            continue;
        }
        warnings.push(format!(
            "config: {}: {}: {:?}",
            diagnostic.file.display(),
            diagnostic.key,
            diagnostic.reason
        ));
    }
    let prompts = Arc::new(Prompts::new(interactions, permissions.mode()));
    let plugins =
        super::plugins::discovered(&cwd, &home, trusted, &loaded.config, &cancel, warnings)
            .map_err(config_error)?;
    let mut registry = plugins.registry(&loaded.config).map_err(config_error)?;
    for plugin in &plugins.plugins {
        if let Some(extension) = tiechui_module_plugins::Extension::start(
            plugin,
            &home,
            agent_trusted,
            interactive.then(|| prompts.clone() as Arc<dyn tiechui_module_plugins::PluginHost>),
            cancel.clone(),
        ) {
            extension
                .register(&mut registry, &loaded.config)
                .map_err(runtime_error)?;
            if !interactive {
                extension.wait_for_startup(cancel.clone()).await;
            }
        }
    }
    let hooks = tiechui_module_hooks::HooksModule::load_with_sources(
        &cwd,
        &home,
        trusted,
        permissions.mode(),
        registry.hook_sources(),
        &cancel,
    )
    .map_err(config_error)?;
    for diagnostic in &hooks.diagnostics {
        warnings.push(format!("hooks: {diagnostic}"));
    }
    let ask = tiechui_module_ask::AskModule(
        interactive.then(|| prompts.clone() as Arc<dyn tiechui_module_ask::QuestionRequester>),
    );
    let rewind = tiechui_module_rewind::RewindModule {
        directory: home.join("state/rewind"),
    };
    let compaction = tiechui_module_compaction::CompactionModule;
    let scheduled = tiechui_module_loop::LoopModule {
        schedule: interactive,
        root: trusted.then(|| {
            cwd.ancestors()
                .find(|dir| dir.join(".git").exists())
                .unwrap_or(&cwd)
                .to_owned()
        }),
        schedules: schedules.clone(),
    };
    let modules: [&dyn Module; 13] = [
        &compaction,
        &tiechui_module_btw::BtwModule,
        &tiechui_module_goal::GoalModule,
        &scheduled,
        &rewind,
        &tiechui_module_context::ContextModule,
        &tiechui_module_tool_search::ToolSearchModule,
        &tiechui_module_ptc::PtcModule,
        &ask,
        &tiechui_module_exec::ExecModule { background: true },
        &tiechui_module_editing::EditingModule,
        &hooks,
        &permissions,
    ];
    for module in modules {
        if loaded
            .config
            .modules
            .get(module.id())
            .copied()
            .unwrap_or_else(|| module.default_enabled())
        {
            module
                .register(&mut registry, &loaded.config)
                .map_err(runtime_error)?;
        }
    }
    let resume = match &open {
        Open::Args => options.resume.clone(),
        _ => None,
    };
    let session = match &open {
        Open::Path(path) => Some(path.clone()),
        _ => None,
    };
    let child_home = home.clone();
    let storage_cancel = cancel.clone();
    let context_home = home.clone();
    let context_cwd = cwd.clone();
    let session_cwd = given.clone();
    let store = tokio::task::spawn_blocking(move || {
        let cancel = storage_cancel;
        if let Some(path) = session {
            SessionStore::open(&path, &cancel).map_err(runtime_error)
        } else if let Some(id) = resume {
            let sessions = tiechui_session::list(&home, &cwd, &cancel).map_err(runtime_error)?;
            let session = sessions
                .sessions
                .into_iter()
                .find(|session| session.header.id == id)
                .ok_or_else(|| {
                    config_error(format!("unknown session {id} for {}", cwd.display()))
                })?;
            SessionStore::open(&session.path, &cancel).map_err(runtime_error)
        } else {
            SessionStore::create(&home, &session_cwd, &cancel).map_err(runtime_error)
        }
    })
    .await
    .map_err(runtime_error)??;
    let given = store.header().cwd.clone();
    for diagnostic in store.diagnostics() {
        warnings.push(format!(
            "session: line {}: {}",
            diagnostic.line, diagnostic.message
        ));
    }
    let saved = store
        .path_to_root(store.current_leaf().unwrap_or(&store.header().id))
        .map_err(runtime_error)?
        .into_iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            EntryKind::ModelChange { model } => Some(model.clone()),
            _ => None,
        });
    let configured = loaded
        .config
        .model
        .as_deref()
        .map(parse_model)
        .transpose()?;
    // The command line's model replaces a resumed session's own only at startup.
    let model = if matches!(open, Open::Args) && options.model.is_some() {
        configured.clone()
    } else {
        saved.or(configured)
    }
    .ok_or_else(|| {
        config_error("set model in config.toml, use --model <provider>/<id>, or run chui setup")
    })?;
    let provider_config = loaded
        .config
        .providers
        .get(&model.provider)
        .ok_or_else(|| config_error(format!("unknown provider {}", model.provider)))?;
    let provider =
        tiechui_providers::from_config(&model.provider, provider_config, &loaded.config.network)
            .map_err(config_error)?;
    if provider.capabilities(&model.model).context_window == 0 {
        return Err(config_error(format!("unknown model {}", model.model.0)));
    }
    let mcp = if loaded.config.modules.get("mcp") != Some(&false) {
        let mcp = tiechui_module_mcp::McpModule::from_config(
            &loaded.config,
            registry.mcp_sources(),
            &context_cwd,
            paths.project.as_deref(),
            trusted,
            cancel.clone(),
            if interactive {
                prompts.clone()
            } else {
                Arc::new(DeclineElicitation)
            },
            Some(&child_home),
        );
        mcp.register(&mut registry, &loaded.config)
            .map_err(runtime_error)?;
        if !interactive {
            // A prompt would otherwise reach the model before local servers finish their
            // handshake, so later requests carry a different tool set and miss the prefix cache.
            // The bound keeps a slow server from blocking the first turn.
            let _ =
                tokio::time::timeout(MCP_STARTUP_GRACE, mcp.wait_for_startup(cancel.clone())).await;
        }
        Some(mcp)
    } else {
        None
    };
    super::subagents::register(
        &mut registry,
        super::subagents::Children {
            home: child_home.clone(),
            trusted: agent_trusted,
            config: loaded.config.clone(),
            permissions,
            rewind: child_home.join("state/rewind"),
            mcp,
        },
        child_home,
        agent_trusted,
        interactive.then_some(prompts as Arc<dyn tiechui_module_subagents::ProjectConfirmation>),
    )
    .map_err(runtime_error)?;
    registry.register_context_provider(Arc::new(StaticContext(vec![BASE_SYSTEM_PROMPT.into()])));
    if loaded.config.modules.get("skills") != Some(&false) {
        let skills_cwd = context_cwd.clone();
        let skills_home = context_home.clone();
        let skills_cancel = cancel.clone();
        let (updated_registry, skills) = tokio::task::spawn_blocking(move || {
            let skills = super::skills::load(
                &skills_cwd,
                &skills_home,
                trusted,
                &registry,
                &skills_cancel,
            )
            .map_err(runtime_error)?;
            Ok::<_, Failure>((registry, skills))
        })
        .await
        .map_err(runtime_error)??;
        registry = updated_registry;
        for diagnostic in &skills.diagnostics {
            warnings.push(format!(
                "skills: {}: {}",
                diagnostic.path.display(),
                diagnostic.message
            ));
        }
        skills
            .register(&mut registry, &loaded.config)
            .map_err(runtime_error)?;
    }
    let prompts = if loaded.config.modules.get("prompts") != Some(&false) {
        let (cwd, home) = (context_cwd.clone(), context_home.clone());
        let prompts = tokio::task::spawn_blocking(move || {
            tiechui_module_prompts::PromptsModule::load(&cwd, &home, trusted)
        })
        .await
        .map_err(runtime_error)?;
        warnings.extend(prompts.warnings.iter().map(|w| format!("prompts: {w}")));
        Some(prompts)
    } else {
        None
    };
    let store = if loaded.config.modules.get("context-files") != Some(&false) {
        let context_cancel = cancel.clone();
        let (store, context, diagnostics) = tokio::task::spawn_blocking(move || {
            let mut store = store;
            let last_recorded = store
                .path_to_root(store.current_leaf().unwrap_or(&store.header().id))
                .map_err(runtime_error)?
                .into_iter()
                .rev()
                .find_map(|entry| match &entry.kind {
                    EntryKind::Custom { kind, data }
                        if kind == tiechui_module_context_files::ENTRY_KIND =>
                    {
                        Some(data.clone())
                    }
                    _ => None,
                });
            let (context, diagnostics) =
                ContextFilesModule::load(&context_files_cwd, &context_home, &context_cancel)
                    .map_err(runtime_error)?;
            let audit = context.entry_data();
            if last_recorded.as_ref() != Some(&audit) {
                store
                    .append(
                        EntryKind::Custom {
                            kind: tiechui_module_context_files::ENTRY_KIND.into(),
                            data: audit,
                        },
                        &context_cancel,
                    )
                    .map_err(runtime_error)?;
            }
            Ok::<_, Failure>((store, context, diagnostics))
        })
        .await
        .map_err(runtime_error)??;
        for diagnostic in diagnostics {
            warnings.push(format!(
                "context: {}: {}",
                diagnostic.path.display(),
                diagnostic.message
            ));
        }
        context
            .register(&mut registry, &loaded.config)
            .map_err(runtime_error)?;
        store
    } else {
        store
    };
    let mut providers: BTreeMap<String, Arc<dyn ModelProvider>> =
        BTreeMap::from([(model.provider.clone(), Arc::from(provider))]);
    for (name, config) in &loaded.config.providers {
        if providers.contains_key(name) {
            continue;
        }
        match tiechui_providers::from_config(name, config, &loaded.config.network) {
            Ok(provider) => {
                providers.insert(name.clone(), Arc::from(provider));
            }
            // Providers without a usable key stay out of the model list.
            Err(error) if error.kind == ErrorKind::Auth => {}
            Err(error) => warnings.push(format!("providers.toml: {name}: {error}")),
        }
    }
    let mut status = vec![
        format!("User config: {}", paths.user.display()),
        format!(
            "Project config: {}",
            paths
                .project
                .as_ref()
                .map_or_else(|| "none".into(), |p| p.display().to_string())
        ),
    ];
    for name in [
        "compaction",
        "btw",
        "goal",
        "loop",
        "rewind",
        "context",
        "ask",
        "exec",
        "editing",
        "hooks",
        "permissions",
        "skills",
        "prompts",
        "mcp",
        "subagents",
        "context-files",
    ] {
        let state = if loaded.config.modules.get(name) == Some(&false) {
            "disabled"
        } else {
            "active"
        };
        status.push(format!("{name}: {state}"));
    }
    status.extend(registry.module_states().iter().map(|(name, enabled)| {
        format!("{name}: {}", if *enabled { "active" } else { "disabled" })
    }));
    status.extend(warnings.iter().cloned());
    let sources = registry
        .commands()
        .iter()
        .filter(|c| c.info().name == "mcp")
        .cloned()
        .collect();
    registry.register_command(Arc::new(super::diagnostics::Status {
        text: status.join("\n"),
        sources,
    }));
    if let Some(prompts) = prompts {
        prompts
            .register(&mut registry, &loaded.config)
            .map_err(runtime_error)?;
    }
    let tui = loaded.config.tui.clone();
    let path = store.path().to_owned();
    let id = store.header().id.clone();
    let handle = SessionActor::spawn(
        store,
        SessionConfig {
            compaction: loaded.config.compaction.clone(),
            model,
            thinking: loaded.config.thinking.and_then(reasoning),
            providers,
            registry,
            retry: Arc::new(BoundedRetry {
                max_retries: if loaded.config.retry.enabled {
                    loaded.config.retry.max_retries
                } else {
                    0
                },
                initial_delay: std::time::Duration::from_millis(loaded.config.retry.base_delay_ms),
                max_delay: std::time::Duration::from_millis(loaded.config.retry.max_delay_ms),
            }),
            interactions: interactive.then_some(inbox),
            epoch,
            short_cache: false,
        },
        cancel,
    );
    Ok(Prepared {
        handle,
        id,
        tui,
        cwd: given,
        path,
    })
}

fn reasoning(level: tiechui_config::ThinkingLevel) -> Option<ReasoningLevel> {
    use tiechui_config::ThinkingLevel as L;
    match level {
        L::Off => None,
        L::Minimal => Some(ReasoningLevel::Minimal),
        L::Low => Some(ReasoningLevel::Low),
        L::Medium => Some(ReasoningLevel::Medium),
        L::High => Some(ReasoningLevel::High),
        L::Xhigh => Some(ReasoningLevel::Xhigh),
        L::Max => Some(ReasoningLevel::Max),
    }
}

/// What the command line asks of a resumed session, sent once its thread starts.
pub(crate) fn startup(args: &ArgMatches) -> Result<Vec<Op>, Failure> {
    let mut ops = Vec::new();
    if let Some(model) = args.get_one::<String>("model") {
        let model = parse_model(model)?;
        ops.push(Op::SetModel {
            provider: model.provider,
            model: model.model.0,
        });
    }
    if let Some(level) = args.get_one::<String>("thinking") {
        ops.push(Op::SetThinking {
            level: serde_json::from_value(serde_json::Value::String(level.clone()))
                .map_err(config_error)?,
        });
    }
    Ok(ops)
}

pub(crate) fn parse_model(value: &str) -> Result<ModelRef, Failure> {
    let (provider, _) = value
        .split_once('/')
        .ok_or_else(|| config_error("expected <provider>/<id>"))?;
    Ok(ModelRef {
        provider: provider.into(),
        model: ModelId(value.into()),
    })
}

async fn execute(
    args: &ArgMatches,
    signals: &mut mpsc::UnboundedReceiver<io::Result<()>>,
) -> Result<i32, Failure> {
    let shutdown = CancellationToken::new();
    let mut warnings = Vec::new();
    let schedules = Default::default();
    let options = Options::from_args(args);
    let mut handle = tokio::select! {
        biased;
        signal = signals.recv() => {
            shutdown.cancel();
            signal.ok_or_else(|| runtime_error("signal listener stopped"))?.map_err(runtime_error)?;
            return Ok(130);
        }
        result = prepare(&options, shutdown.clone(), false, Open::Args, SessionEpoch(0), &schedules, &mut warnings) => {
            for warning in &warnings {
                eprintln!("{warning}");
            }
            result?.handle
        }
    };
    let result = drive(args, signals, &mut handle).await;
    shutdown.cancel();
    drop(handle.submissions);
    let settled = handle
        .task
        .await
        .map_err(runtime_error)?
        .map_err(runtime_error);
    let code = result?;
    settled?;
    Ok(code)
}

async fn drive(
    args: &ArgMatches,
    signals: &mut mpsc::UnboundedReceiver<io::Result<()>>,
    handle: &mut SessionHandle,
) -> Result<i32, Failure> {
    let mut output = Output {
        json: args.get_flag("json"),
        assistants: HashMap::new(),
        line_open: false,
    };
    let mut thread: Option<(ThreadId, SessionEpoch)> = None;
    let mut turn: Option<TurnId> = None;
    let mut was_interrupted = false;
    let mut status = None;
    let result = loop {
        tokio::select! {
            biased;
            signal = signals.recv() => {
                signal.ok_or_else(|| runtime_error("signal listener stopped"))?.map_err(runtime_error)?;
                was_interrupted = true;
                eprintln!("interrupting; press Ctrl-C again to force quit");
                if let (Some((id, epoch)), Some(turn_id)) = (&thread, &turn) {
                    submit(handle, id, *epoch, Op::Interrupt { turn_id: turn_id.clone() }).await?;
                }
            }
            event = handle.events.recv() => {
                let event = match event { Ok(event) => event, Err(error) => break Err(runtime_error(error)) };
                if let Err(error) = output.event(&event) { break Err(runtime_error(error)); }
                match event.event {
                    EventKind::ThreadStarted { .. } => {
                        thread = Some((event.thread_id.clone(), event.epoch));
                        for op in startup(args)? {
                            submit(handle, &event.thread_id, event.epoch, op).await?;
                        }
                        submit(handle, &event.thread_id, event.epoch, Op::UserInput {
                            content: vec![InputContent::Text { text: args.get_one::<String>("prompt").cloned().unwrap_or_default() }],
                            delivery: InputDelivery::FollowUp,
                        }).await?;
                    }
                    EventKind::TurnStarted { turn: started } => {
                        if was_interrupted {
                            submit(handle, &event.thread_id, event.epoch, Op::Interrupt { turn_id: started.id.clone() }).await?;
                        }
                        turn = Some(started.id);
                    }
                    EventKind::TurnCompleted { turn } => { status = Some(turn.status); break Ok(()); }
                    EventKind::Error { .. } => { status = Some(TurnStatus::Failed); }
                    _ => {}
                }
            }
        }
    };
    result?;
    Ok(
        if was_interrupted || status == Some(TurnStatus::Interrupted) {
            130
        } else if status == Some(TurnStatus::Completed) {
            0
        } else {
            1
        },
    )
}

async fn submit(
    handle: &SessionHandle,
    thread: &ThreadId,
    epoch: SessionEpoch,
    op: Op,
) -> Result<(), Failure> {
    handle
        .submissions
        .send(Submission {
            id: SubmissionId("exec".into()),
            thread_id: thread.clone(),
            epoch,
            op,
        })
        .await
        .map_err(runtime_error)
}

struct Output {
    json: bool,
    assistants: HashMap<ItemId, usize>,
    line_open: bool,
}

impl Output {
    fn event(&mut self, event: &Event) -> io::Result<()> {
        let mut stdout = io::stdout().lock();
        if self.json {
            serde_json::to_writer(&mut stdout, event)?;
            writeln!(stdout)?;
        } else {
            match &event.event {
                EventKind::ItemStarted {
                    item: Item::Known(KnownItem::AssistantMessage { id, .. }),
                    ..
                } => {
                    self.assistants.insert(id.clone(), 0);
                }
                EventKind::ItemDelta { item_id, delta, .. } => {
                    if let Some(bytes) = self.assistants.get_mut(item_id) {
                        write!(stdout, "{delta}")?;
                        if !delta.is_empty() {
                            self.line_open = !delta.ends_with('\n');
                        }
                        *bytes += delta.len();
                    }
                }
                EventKind::ItemCompleted {
                    item: Item::Known(KnownItem::AssistantMessage { id, text }),
                    ..
                } => {
                    let bytes = self.assistants.remove(id).unwrap_or(0);
                    if let Some(suffix) = text.get(bytes..) {
                        write!(stdout, "{suffix}")?;
                        if !suffix.is_empty() {
                            self.line_open = !suffix.ends_with('\n');
                        }
                    }
                    self.end_line(&mut stdout)?;
                }
                _ => {}
            }
        }
        // Usage can arrive before ItemCompleted while streamed text is still open.
        if matches!(
            &event.event,
            EventKind::ThreadStarted { .. }
                | EventKind::ItemCompleted {
                    item: Item::Known(KnownItem::ToolCall { .. }),
                    ..
                }
                | EventKind::TokenUsageUpdated { .. }
                | EventKind::Error { .. }
                | EventKind::CompactionStarted { .. }
                | EventKind::CompactionProgress { .. }
                | EventKind::CompactionEnded { .. }
                | EventKind::ItemCompleted {
                    item: Item::Known(KnownItem::Compaction { degraded: true, .. }),
                    ..
                }
                | EventKind::RequestRetry { .. }
                | EventKind::TurnCompleted { .. }
        ) || matches!(&event.event, EventKind::Extension { name, data }
            if name == tiechui_protocol::PLUGIN_NOTICE_EVENT && data.is_string())
        {
            self.end_line(&mut stdout)?;
        }
        stdout.flush()?;
        match &event.event {
            EventKind::ThreadStarted { .. } => eprintln!("session: {}", event.thread_id.0),
            EventKind::ItemCompleted {
                item: Item::Known(KnownItem::ToolCall { name, .. }),
                ..
            } => eprintln!("tool: {name}"),
            EventKind::TokenUsageUpdated { last, .. } => eprintln!(
                "usage: input={} output={} cache_read={} cache_write={}",
                last.input, last.output, last.cache_read, last.cache_write
            ),
            EventKind::Error { message } => eprintln!("error: {message}"),
            EventKind::Extension { name, data }
                if name == tiechui_protocol::PLUGIN_NOTICE_EVENT =>
            {
                if let Some(text) = data.as_str() {
                    eprintln!("{text}");
                }
            }
            EventKind::CompactionStarted { reason } => eprintln!("compaction: {reason:?}"),
            EventKind::CompactionProgress {
                phase,
                retries,
                shrinks,
                ..
            } => eprintln!("compaction: {phase:?} (retries={retries} input_reductions={shrinks})"),
            EventKind::ItemCompleted {
                item: Item::Known(KnownItem::Compaction { degraded: true, .. }),
                ..
            } => eprintln!("compaction: degraded; earlier messages were omitted from the summary"),
            EventKind::CompactionEnded {
                aborted,
                error,
                paused,
                ..
            } => {
                if *aborted {
                    eprintln!("Compaction cancelled");
                } else if let Some(error) = error {
                    eprintln!("{error}");
                }
                if let Some(paused) = paused {
                    eprintln!("{paused}");
                }
            }
            EventKind::RequestRetry {
                attempt,
                max_retries,
                delay_ms,
                message,
                ..
            } => {
                let limit = max_retries.map(|max| format!("/{max}")).unwrap_or_default();
                let state = delay_ms
                    .map(|ms| format!("in {ms}ms"))
                    .unwrap_or_else(|| "starting".into());
                eprintln!(
                    "retry: {attempt}{limit} {state}: {}",
                    message.replace(['\n', '\r'], " ")
                );
            }
            EventKind::TurnCompleted { turn } => eprintln!("turn: {:?}", turn.status),
            _ => {}
        }
        Ok(())
    }

    fn end_line(&mut self, stdout: &mut impl Write) -> io::Result<()> {
        if self.line_open {
            writeln!(stdout)?;
            self.line_open = false;
        }
        Ok(())
    }
}
