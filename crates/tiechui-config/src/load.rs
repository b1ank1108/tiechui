use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use toml::{Table, Value};

use crate::{
    Config, ConfigError, ConfigPaths, Diagnostic, IgnoredReason, LoadedConfig, ProviderConfig,
    StatusLineConfig, StatusLineItem, TuiConfig, Wire, validate_env_name,
};

#[derive(Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "ConfigLayer")]
struct Layer {
    /// Default model reference in `<provider>/<id>` form.
    model: Option<String>,
    thinking: Option<crate::ThinkingLevel>,
    modules: BTreeMap<String, bool>,
    retry: Option<crate::RetryConfig>,
    compaction: Option<crate::CompactionConfig>,
    permissions: Option<crate::PermissionsConfig>,
    mcp_servers: BTreeMap<String, crate::McpServerConfig>,
    tui: TuiLayer,
    network: Option<crate::NetworkConfig>,
    ask: Option<crate::AskConfig>,
    sessions: Option<crate::SessionsConfig>,
    skills: Option<crate::SkillsConfig>,
}

#[derive(Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
struct TuiLayer {
    theme: Option<String>,
    copy_on_select: Option<bool>,
    status_line: StatusLineLayer,
}

/// Each field replaces the earlier layer's value on its own.
#[derive(Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
struct StatusLineLayer {
    items: Option<Vec<StatusLineItem>>,
    use_theme_colors: Option<bool>,
    icons: Option<Vec<StatusLineItem>>,
}

/// Schema for one config file or CLI layer.
pub fn schema() -> schemars::schema::RootSchema {
    schemars::schema_for!(Layer)
}

/// Schema for `providers.toml`: each top-level table is one provider.
pub fn providers_schema() -> schemars::schema::RootSchema {
    schemars::schema_for!(BTreeMap<String, ProviderConfig>)
}

/// Merge defaults, user, trusted project, then CLI settings; module keys merge
/// individually. Providers are read from `providers.toml` alone.
pub fn load(
    paths: &ConfigPaths,
    project_trusted: bool,
    cli: &Table,
) -> Result<LoadedConfig, ConfigError> {
    load_with_mandatory_modules(paths, project_trusted, cli, &[])
}

/// Mandatory module identities come from the host's statically linked modules.
pub fn load_with_mandatory_modules(
    paths: &ConfigPaths,
    project_trusted: bool,
    cli: &Table,
    mandatory: &[&str],
) -> Result<LoadedConfig, ConfigError> {
    let mut merged = Layer::default();
    let mut mcp_origins = BTreeMap::new();
    let mut diagnostics = Vec::new();
    let mut project_rules = Vec::new();
    let mut project_invalid = false;
    if let Some(text) = read_optional(&paths.user)? {
        merge(
            &mut merged,
            parse_layer(&paths.user, parse_table(&paths.user, &text)?)?,
            &paths.user,
            &mut mcp_origins,
            &mut diagnostics,
        );
    }
    if let Some(path) = &paths.project {
        let project = read_optional(path)
            .and_then(|text| text.map(|text| parse_table(path, &text)).transpose());
        match project {
            Ok(Some(mut table)) if project_trusted => {
                if let Some(modules) = table.get_mut("modules").and_then(Value::as_table_mut) {
                    modules.retain(|id, value| {
                        if mandatory.contains(&id) && value.as_bool() == Some(false) {
                            diagnostics.push(Diagnostic {
                                file: path.to_owned(),
                                key: format!("modules.{id}"),
                                reason: IgnoredReason::MandatoryModule,
                            });
                            false
                        } else {
                            true
                        }
                    });
                }
                let mut project_layer = match parse_layer(path, table.clone()) {
                    Ok(layer) => layer,
                    Err(error) if error.key.starts_with("permissions") => {
                        project_invalid = true;
                        diagnostics.push(Diagnostic {
                            file: path.to_owned(),
                            key: error.key,
                            reason: IgnoredReason::ProjectPermissionsInvalid,
                        });
                        table.remove("permissions");
                        parse_layer(path, table)?
                    }
                    Err(error) => return Err(error),
                };
                let mut user_only = vec![];
                if let Some(permissions) = project_layer.permissions.take() {
                    if permissions.mode.is_some() {
                        user_only.push("permissions.mode");
                    }
                    // A project could otherwise send every reviewed action to a model it picks.
                    if permissions.classifier_model.is_some() {
                        user_only.push("permissions.classifier_model");
                    }
                    project_rules = permissions.rules.unwrap_or_default();
                }
                // A project could otherwise route requests, keys included, through its own proxy.
                if project_layer.network.take().is_some() {
                    user_only.push("network");
                }
                if project_layer.sessions.take().is_some() {
                    user_only.push("sessions");
                }
                if project_layer.skills.take().is_some() {
                    user_only.push("skills");
                }
                diagnostics.extend(user_only.into_iter().map(|key| Diagnostic {
                    file: path.to_owned(),
                    key: key.into(),
                    reason: IgnoredReason::UserOnly,
                }));
                merge(
                    &mut merged,
                    project_layer,
                    path,
                    &mut mcp_origins,
                    &mut diagnostics,
                );
            }
            Ok(Some(table)) => ignored_keys(&Value::Table(table), "", path, &mut diagnostics),
            Ok(None) | Err(_) if !project_trusted => diagnostics.push(Diagnostic {
                file: path.to_owned(),
                key: "*".into(),
                reason: IgnoredReason::UntrustedProject,
            }),
            Ok(None) => {}
            Err(error) => return Err(error),
        }
    }
    let cli_path = Path::new("<cli>");
    merge(
        &mut merged,
        parse_layer(cli_path, cli.clone())?,
        cli_path,
        &mut mcp_origins,
        &mut diagnostics,
    );
    if let Some(network) = &mut merged.network {
        network
            .normalize()
            .map_err(|message| ConfigError::new(&paths.user, "network", message))?;
    }
    let config = Config {
        model: merged.model,
        thinking: merged.thinking,
        modules: merged.modules,
        retry: merged.retry.unwrap_or_default(),
        compaction: merged.compaction.unwrap_or_default(),
        permissions: crate::PermissionsConfig {
            project_rules,
            project_invalid,
            ..merged.permissions.unwrap_or_default()
        },
        mcp_servers: merged.mcp_servers,
        mcp_origins,
        tui: tui(merged.tui),
        network: merged.network.unwrap_or_default(),
        ask: merged.ask.unwrap_or_default(),
        sessions: merged.sessions.unwrap_or_default(),
        skills: merged.skills.unwrap_or_default(),
        providers: load_providers(&paths.providers())?,
    };
    Ok(LoadedConfig {
        config,
        diagnostics,
    })
}

fn tui(layer: TuiLayer) -> TuiConfig {
    let defaults = StatusLineConfig::default();
    let line = layer.status_line;
    TuiConfig {
        theme: layer.theme,
        copy_on_select: layer
            .copy_on_select
            .unwrap_or(TuiConfig::default().copy_on_select),
        status_line: StatusLineConfig {
            items: line.items.unwrap_or(defaults.items),
            use_theme_colors: line.use_theme_colors.unwrap_or(defaults.use_theme_colors),
            icons: line.icons.unwrap_or(defaults.icons),
        },
    }
}

pub(crate) fn read_optional(path: &Path) -> Result<Option<String>, ConfigError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ConfigError::new(path, "<file>", error.to_string())),
    }
}

fn parse_table(path: &Path, text: &str) -> Result<Table, ConfigError> {
    toml::from_str(text)
        .map_err(|error: toml::de::Error| ConfigError::new(path, "<document>", error.message()))
}

/// Rejects a config file that would not load.
pub(crate) fn check_layer(path: &Path, text: &str) -> Result<(), ConfigError> {
    parse_layer(path, parse_table(path, text)?).map(drop)
}

/// Validates every provider without reading credential values.
pub fn load_providers(path: &Path) -> Result<BTreeMap<String, ProviderConfig>, ConfigError> {
    match read_optional(path)? {
        Some(text) => parse_providers(path, &text),
        None => Ok(BTreeMap::new()),
    }
}

pub(crate) fn parse_providers(
    path: &Path,
    text: &str,
) -> Result<BTreeMap<String, ProviderConfig>, ConfigError> {
    let providers: BTreeMap<String, ProviderConfig> = serde_path_to_error::deserialize(
        Value::Table(parse_table(path, text)?),
    )
    .map_err(|error| ConfigError::new(path, error.path().to_string(), error.inner().message()))?;
    for (name, provider) in &providers {
        if let Some(env) = &provider.api_key_env {
            validate_env_name(path, &format!("{name}.api_key_env"), env)?;
            crate::register_secret_env_var(env);
        }
        let invalid_key = match (&provider.api_key, &provider.api_key_env) {
            (Some(_), Some(_)) => Some("set api_key or api_key_env, not both"),
            (Some(key), None) if key.0.is_empty() => Some("must not be empty"),
            _ => None,
        };
        if let Some(message) = invalid_key {
            return Err(ConfigError::new(path, format!("{name}.api_key"), message));
        }
        for (index, model) in provider.models.iter().enumerate() {
            if model.prices.is_some_and(|prices| !prices.valid()) {
                return Err(ConfigError::new(
                    path,
                    format!("{name}.models[{index}].prices"),
                    "prices must be finite, non-negative USD per million tokens",
                ));
            }
            let mut seen = std::collections::BTreeSet::new();
            for fallback in &model.fallback_models {
                if provider.wire != Wire::AnthropicMessages
                    || fallback == &model.id
                    || !provider
                        .models
                        .iter()
                        .any(|candidate| &candidate.id == fallback)
                    || !seen.insert(fallback)
                {
                    return Err(ConfigError::new(
                        path,
                        format!("{name}.models[{index}].fallback_models"),
                        "fallbacks must be distinct other models configured in the same Anthropic provider",
                    ));
                }
            }
        }
    }
    Ok(providers)
}

fn parse_layer(path: &Path, table: Table) -> Result<Layer, ConfigError> {
    if table.contains_key("providers") {
        return Err(ConfigError::new(
            path,
            "providers",
            "providers are defined in providers.toml beside the user config.toml",
        ));
    }
    let layer: Layer = serde_path_to_error::deserialize(Value::Table(table)).map_err(|error| {
        ConfigError::new(path, error.path().to_string(), error.inner().message())
    })?;
    if let Some(retry) = &layer.retry
        && (retry.base_delay_ms == 0 || retry.max_delay_ms < retry.base_delay_ms)
    {
        return Err(ConfigError::new(
            path,
            "retry",
            "delays must be positive and max_delay_ms must be at least base_delay_ms",
        ));
    }
    for (key, value) in [
        (
            "ask.auto_continue_after_sec",
            layer
                .ask
                .as_ref()
                .and_then(|ask| ask.auto_continue_after_sec),
        ),
        (
            "sessions.archive_after_days",
            layer.sessions.as_ref().and_then(|s| s.archive_after_days),
        ),
    ] {
        if value == Some(0) {
            return Err(ConfigError::new(path, key, "must be positive"));
        }
    }
    if let Some(compaction) = &layer.compaction {
        compaction
            .validate_range()
            .map_err(|message| ConfigError::new(path, "compaction", message))?;
    }
    if let Some(compaction) = &layer.compaction
        && (compaction.reserve_tokens > 9007199254740991
            || compaction.keep_recent_tokens > 9007199254740991
            || compaction.model_overrides.values().any(|value| {
                value
                    .reserve_tokens
                    .is_some_and(|tokens| tokens > 9007199254740991)
                    || value
                        .keep_recent_tokens
                        .is_some_and(|tokens| tokens > 9007199254740991)
            }))
    {
        return Err(ConfigError::new(
            path,
            "compaction",
            "token settings must be non-negative safe integers",
        ));
    }
    let model_ref = |model: &str| {
        model
            .split_once('/')
            .is_some_and(|(provider, id)| !provider.is_empty() && !id.is_empty())
    };
    if layer
        .model
        .as_deref()
        .is_some_and(|model| !model_ref(model))
    {
        return Err(ConfigError::new(path, "model", "expected <provider>/<id>"));
    }
    if let Some(permissions) = &layer.permissions {
        if permissions
            .classifier_model
            .as_deref()
            .is_some_and(|model| !model_ref(model))
        {
            return Err(ConfigError::new(
                path,
                "permissions.classifier_model",
                "expected <provider>/<id>",
            ));
        }
        for (index, rule) in permissions.rules.iter().flatten().enumerate() {
            rule.validate().map_err(|error| {
                ConfigError::new(path, format!("permissions.rules[{index}]"), error)
            })?;
        }
    }
    Ok(layer)
}

fn merge(
    base: &mut Layer,
    layer: Layer,
    source: &Path,
    mcp_origins: &mut BTreeMap<String, PathBuf>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (name, server) in layer.mcp_servers {
        for key in server.unknown.keys() {
            diagnostics.push(Diagnostic {
                file: source.to_owned(),
                key: format!("mcp_servers.{name}.{key}"),
                reason: IgnoredReason::UnknownMcpKey,
            });
        }
        mcp_origins.insert(name.clone(), source.to_owned());
        base.mcp_servers.insert(name, server);
    }
    if layer.model.is_some() {
        base.model = layer.model;
    }
    if layer.thinking.is_some() {
        base.thinking = layer.thinking;
    }
    base.modules.extend(layer.modules);
    if layer.retry.is_some() {
        base.retry = layer.retry;
    }
    if layer.compaction.is_some() {
        base.compaction = layer.compaction;
    }
    if layer.permissions.is_some() {
        base.permissions = layer.permissions;
    }
    if layer.network.is_some() {
        base.network = layer.network;
    }
    if layer.ask.is_some() {
        base.ask = layer.ask;
    }
    if layer.sessions.is_some() {
        base.sessions = layer.sessions;
    }
    if layer.skills.is_some() {
        base.skills = layer.skills;
    }
    base.tui.theme = layer.tui.theme.or(base.tui.theme.take());
    base.tui.copy_on_select = layer.tui.copy_on_select.or(base.tui.copy_on_select);
    let (line, next) = (&mut base.tui.status_line, layer.tui.status_line);
    line.items = next.items.or(line.items.take());
    line.use_theme_colors = next.use_theme_colors.or(line.use_theme_colors);
    line.icons = next.icons.or(line.icons.take());
}

fn ignored_keys(value: &Value, key: &str, file: &Path, diagnostics: &mut Vec<Diagnostic>) {
    if let Value::Table(table) = value
        && !table.is_empty()
    {
        for (name, value) in table {
            let key = if key.is_empty() {
                name.clone()
            } else {
                format!("{key}.{name}")
            };
            ignored_keys(value, &key, file, diagnostics);
        }
    } else {
        diagnostics.push(Diagnostic {
            file: file.to_owned(),
            key: if key.is_empty() {
                "*".into()
            } else {
                key.into()
            },
            reason: IgnoredReason::UntrustedProject,
        });
    }
}
