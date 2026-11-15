//! `providers/*` and `config/*`: the user's `providers.toml` and `config.toml`, edited in place.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tiechui_config::{
    ModelConfig, NetworkConfig, ProviderConfig, ThinkingLevelValue, UnsupportedLevel,
};
use tiechui_protocol::ThinkingLevel;
use tiechui_protocol::app::*;
use tokio_util::sync::CancellationToken;

use super::{Reply, blocking, internal, invalid, lock, parse, value};

const LEVELS: [ThinkingLevel; 7] = [
    ThinkingLevel::Off,
    ThinkingLevel::Minimal,
    ThinkingLevel::Low,
    ThinkingLevel::Medium,
    ThinkingLevel::High,
    ThinkingLevel::Xhigh,
    ThinkingLevel::Max,
];

const QUERIES: [&str; 5] = [
    "providers/list",
    "providers/presets",
    "providers/reload",
    "providers/test",
    "providers/models",
];

const EDITS: [&str; 4] = [
    "providers/save",
    "providers/delete",
    "providers/save_model",
    "providers/delete_model",
];

pub(super) fn handles(method: &str) -> bool {
    edits(method) || QUERIES.contains(&method) || matches!(method, "config/get" | "config/update")
}

pub(super) fn edits(method: &str) -> bool {
    EDITS.contains(&method)
}

pub(super) async fn handle(home: &Path, method: &str, params: Value) -> Reply {
    if matches!(method, "config/get" | "config/update") {
        return handle_config(home, method, params).await;
    }
    if edits(method) {
        return handle_edit(home.join("providers.toml"), method, params).await;
    }
    handle_query(home, method, params).await
}

async fn handle_config(home: &Path, method: &str, params: Value) -> Reply {
    if method == "config/get" {
        let user = home.join("config.toml");
        return blocking(move |_| crate::setup::user_config(&user))
            .await
            .and_then(|config| value(values(config)));
    }
    let p: ConfigUpdateParams = parse(params)?;
    let home = home.to_owned();
    write(move || update(&home, p)).await
}

async fn handle_edit(file: PathBuf, method: &str, params: Value) -> Reply {
    match method {
        "providers/save" => {
            let p: ProviderSaveParams = parse(params)?;
            write(move || {
                tiechui_config::save_provider(
                    &file,
                    &p.name,
                    &p.wire,
                    p.base_url.as_deref(),
                    p.api_key.as_deref(),
                )
            })
            .await
        }
        "providers/delete" => {
            let p: ProviderParams = parse(params)?;
            write(move || tiechui_config::remove_provider(&file, &p.name)).await
        }
        "providers/save_model" => {
            let p: ModelSaveParams = parse(params)?;
            write(move || {
                tiechui_config::save_model(
                    &file,
                    &p.provider,
                    p.previous.as_deref(),
                    &config(p.model),
                )
            })
            .await
        }
        _ => {
            let p: ModelParams = parse(params)?;
            write(move || tiechui_config::remove_model(&file, &p.provider, &p.id)).await
        }
    }
}

async fn handle_query(home: &Path, method: &str, params: Value) -> Reply {
    let file = home.join("providers.toml");
    match method {
        "providers/list" => blocking(move |_| list(&file)).await.and_then(value),
        "providers/presets" => presets(params),
        "providers/reload" => write(move || tiechui_config::load_providers(&file).map(drop)).await,
        "providers/test" => test_model(home, params).await,
        _ => list_remote_models(home, params).await,
    }
}

async fn test_model(home: &Path, params: Value) -> Reply {
    let p: ModelParams = parse(params)?;
    let (provider, network) = declared(home, p.provider.clone()).await?;
    let model = provider
        .models
        .iter()
        .find(|model| model.id == p.id)
        .ok_or_else(|| invalid(format!("{}/{} is not declared", p.provider, p.id)))?;
    let start = Instant::now();
    let error = crate::setup::check(&p.provider, &provider, model, &network)
        .await
        .err();
    value(ProviderTest {
        error,
        millis: start.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
    })
}

async fn list_remote_models(home: &Path, params: Value) -> Reply {
    let p: ProviderParams = parse(params)?;
    let (provider, network) = declared(home, p.name).await?;
    tiechui_providers::list_models(&provider, &network, &CancellationToken::new())
        .await
        .map_err(internal)
        .and_then(value)
}

/// Edits read, change and replace a whole file, so they run one at a time to keep each other's
/// changes.
pub(super) async fn write<E: ToString>(
    edit: impl FnOnce() -> Result<(), E> + Send + 'static,
) -> Reply {
    static WRITES: Mutex<()> = Mutex::new(());
    blocking(move |_| {
        let _write = lock(&WRITES);
        edit().map_err(|e| e.to_string())
    })
    .await
    .and_then(value)
}

fn list(file: &Path) -> Result<Vec<ProviderEntry>, String> {
    let providers = tiechui_config::load_providers(file).map_err(|e| e.to_string())?;
    Ok(providers
        .into_iter()
        .map(|(name, provider)| ProviderEntry {
            name,
            wire: convert(provider.wire),
            base_url: provider.base_url,
            api_key: provider.api_key.is_some(),
            api_key_env: provider.api_key_env,
            models: provider.models.iter().map(entry).collect(),
        })
        .collect())
}

fn presets(params: Value) -> Reply {
    let p: ModelPresetsParams = parse(params)?;
    let matched: Vec<ModelEntry> = p
        .ids
        .into_iter()
        .filter_map(|id| {
            let preset = tiechui_providers::model_preset(&id)?;
            Some(entry(&preset.to_model_config(id)))
        })
        .collect();
    value(matched)
}

fn entry(model: &ModelConfig) -> ModelEntry {
    ModelEntry {
        id: model.id.clone(),
        context_window: model.context_window,
        max_output: model.max_output,
        levels: tiechui_providers::offered(model)
            .into_iter()
            .map(|level| level.map_or(ThinkingLevel::Off, convert))
            .collect(),
        mapping: model
            .thinking_levels
            .iter()
            .flatten()
            .filter_map(|(level, value)| match value {
                ThinkingLevelValue::Wire(wire) => Some((convert(level), wire.clone())),
                ThinkingLevelValue::Unsupported(_) => None,
            })
            .collect(),
        images: model.images,
    }
}

/// The configuration `entry` reads back: reasoning when a level besides off is offered, mapped
/// values, and other levels marked unsupported, except xhigh and max, which are off unless mapped.
fn config(model: ModelEntry) -> ModelConfig {
    let reasoning = model
        .levels
        .iter()
        .any(|level| *level != ThinkingLevel::Off);
    let levels: std::collections::BTreeMap<_, _> = LEVELS
        .into_iter()
        .filter(|_| reasoning)
        .filter_map(|level| {
            let offered = model.levels.contains(&level);
            let optional = matches!(level, ThinkingLevel::Xhigh | ThinkingLevel::Max);
            let value = match model.mapping.get(&level).filter(|wire| !wire.is_empty()) {
                Some(wire) if offered => ThinkingLevelValue::Wire(wire.clone()),
                None if offered && optional => ThinkingLevelValue::Wire(convert(level)),
                _ if !offered && !optional => ThinkingLevelValue::Unsupported(UnsupportedLevel),
                _ => return None,
            };
            Some((convert(level), value))
        })
        .collect();
    ModelConfig {
        id: model.id,
        cache_retention: Default::default(),
        prices: None,
        fallback_models: Vec::new(),
        context_window: model.context_window,
        max_output: model.max_output,
        reasoning,
        thinking: Default::default(),
        thinking_levels: (!levels.is_empty()).then_some(levels),
        compat: None,
        images: model.images,
    }
}

fn values(config: tiechui_config::Config) -> ConfigValues {
    ConfigValues {
        model: config.model,
        lightweight_model: config.lightweight_model,
        classifier_model: config.permissions.classifier_model,
        thinking: config.thinking.map(convert),
        proxy: config.network.proxy,
        no_proxy: config.network.no_proxy,
        ca_cert: config
            .network
            .ca_cert
            .map(|path| path.display().to_string()),
        auto_continue_after_sec: config.ask.auto_continue_after_sec,
        archive_after_days: config.sessions.archive_after_days,
    }
}

/// Keys besides `model` and `thinking`, which null removes.
const OPTIONAL: [&str; 7] = [
    "lightweight_model",
    "permissions.classifier_model",
    "network.proxy",
    "network.no_proxy",
    "network.ca_cert",
    "ask.auto_continue_after_sec",
    "sessions.archive_after_days",
];

const MODEL_KEYS: [&str; 3] = ["model", "lightweight_model", "permissions.classifier_model"];

const NETWORK_KEYS: [&str; 3] = ["network.proxy", "network.no_proxy", "network.ca_cert"];

const NUMBER_KEYS: [&str; 2] = ["ask.auto_continue_after_sec", "sessions.archive_after_days"];

/// `config/update`: one key of the user's `config.toml`, kept only if the configuration still
/// loads.
fn update(home: &Path, params: ConfigUpdateParams) -> Result<(), String> {
    let path = home.join("config.toml");
    let original = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        read => Some(read.map_err(|e| e.to_string())?),
    };
    apply_preference(home, &path, params.key.as_str(), params.value)?;
    let config = crate::setup::user_config(&path);
    // Certificates are read only as clients are made.
    let network = config.and_then(|config| config.network.apply(reqwest::Client::builder()));
    network.map(drop).or_else(|error| {
        match original {
            Some(text) => std::fs::write(&path, text),
            None => std::fs::remove_file(&path),
        }
        .map_err(|e| e.to_string())?;
        Err(error)
    })
}

fn declared_model(home: &Path, key: &str, model: &str) -> Result<(), String> {
    let (provider, id) = model
        .split_once('/')
        .filter(|(provider, id)| !provider.is_empty() && !id.is_empty())
        .ok_or_else(|| format!("{key} is <provider>/<id>"))?;
    let providers =
        tiechui_config::load_providers(&home.join("providers.toml")).map_err(|e| e.to_string())?;
    let exists = providers
        .get(provider)
        .is_some_and(|declared| declared.models.iter().any(|m| m.id == id));
    exists
        .then_some(())
        .ok_or_else(|| format!("{model} is not declared"))
}

fn apply_preference(home: &Path, path: &Path, key: &str, value: Value) -> Result<(), String> {
    if value.is_null() && OPTIONAL.contains(&key) {
        return tiechui_config::remove_preference(path, key).map_err(|e| e.to_string());
    }
    if save_string_preference(home, path, key, &value)? {
        return Ok(());
    }
    let save = |val| tiechui_config::save_preference(path, key, val).map_err(|e| e.to_string());
    if key == "thinking" {
        let level: ThinkingLevel = serde_json::from_value(value).map_err(|e| e.to_string())?;
        return save(convert::<String>(level).into());
    }
    if NUMBER_KEYS.contains(&key) {
        let number = value
            .as_i64()
            .ok_or_else(|| format!("{key} has the wrong type"))?;
        return save(number.into());
    }
    Err(format!("{key} cannot be set here"))
}

fn save_string_preference(
    home: &Path,
    path: &Path,
    key: &str,
    value: &Value,
) -> Result<bool, String> {
    if !MODEL_KEYS.contains(&key) && !NETWORK_KEYS.contains(&key) {
        return Ok(false);
    }
    let text = value
        .as_str()
        .ok_or_else(|| format!("{key} has the wrong type"))?;
    if MODEL_KEYS.contains(&key) {
        declared_model(home, key, text)?;
    }
    tiechui_config::save_preference(path, key, text.into()).map_err(|e| e.to_string())?;
    Ok(true)
}

/// The provider with the user's network settings.
async fn declared(
    home: &Path,
    name: String,
) -> Result<(ProviderConfig, NetworkConfig), super::rpc::Error> {
    let user = home.join("config.toml");
    blocking(move |_| {
        let mut config = crate::setup::user_config(&user)?;
        let provider = config
            .providers
            .remove(&name)
            .ok_or_else(|| format!("provider {name} is not declared"))?;
        Ok((provider, config.network))
    })
    .await
}

/// Levels, wires and MCP server states have the same names in the protocol as at their source.
pub(super) fn convert<T: DeserializeOwned>(from: impl Serialize) -> T {
    serde_json::to_value(from)
        .and_then(serde_json::from_value)
        .expect("the same name on both sides")
}
