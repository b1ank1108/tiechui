//! `providers/*` and `config/*`: the user's `providers.toml` and `config.toml`, edited in place.

use std::path::Path;
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

pub(super) fn handles(method: &str) -> bool {
    edits(method)
        || matches!(
            method,
            "providers/list"
                | "providers/test"
                | "providers/models"
                | "config/get"
                | "config/update"
        )
}

/// Whether the method changes the providers, which open sessions then reload.
pub(super) fn edits(method: &str) -> bool {
    matches!(
        method,
        "providers/save" | "providers/delete" | "providers/save_model" | "providers/delete_model"
    )
}

pub(super) async fn handle(home: &Path, method: &str, params: Value) -> Reply {
    let file = home.join("providers.toml");
    match method {
        "providers/list" => blocking(move |_| list(&file)).await.and_then(value),
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
        "providers/delete_model" => {
            let p: ModelParams = parse(params)?;
            write(move || tiechui_config::remove_model(&file, &p.provider, &p.id)).await
        }
        "providers/test" => {
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
        "providers/models" => {
            let p: ProviderParams = parse(params)?;
            let (provider, network) = declared(home, p.name).await?;
            tiechui_providers::list_models(&provider, &network, &CancellationToken::new())
                .await
                .map_err(internal)
                .and_then(value)
        }
        "config/get" => {
            let user = home.join("config.toml");
            blocking(move |_| crate::setup::user_config(&user))
                .await
                .and_then(|config| value(values(config)))
        }
        _ => {
            let p: ConfigUpdateParams = parse(params)?;
            let home = home.to_owned();
            write(move || update(&home, p)).await
        }
    }
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
const OPTIONAL: [&str; 5] = [
    "network.proxy",
    "network.no_proxy",
    "network.ca_cert",
    "ask.auto_continue_after_sec",
    "sessions.archive_after_days",
];

/// `config/update`: one key of the user's `config.toml`, kept only if the configuration still
/// loads.
fn update(home: &Path, params: ConfigUpdateParams) -> Result<(), String> {
    let path = home.join("config.toml");
    let original = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        read => Some(read.map_err(|e| e.to_string())?),
    };
    let key = params.key.as_str();
    let save = |value| tiechui_config::save_preference(&path, key, value);
    match (key, params.value) {
        ("model", value) => {
            let model = value.as_str().ok_or("model is a string")?;
            let (provider, id) = model.split_once('/').ok_or("model is <provider>/<id>")?;
            let providers = tiechui_config::load_providers(&home.join("providers.toml"))
                .map_err(|e| e.to_string())?;
            if !providers
                .get(provider)
                .is_some_and(|declared| declared.models.iter().any(|m| m.id == id))
            {
                return Err(format!("{model} is not declared"));
            }
            save(model.into())
        }
        ("thinking", value) => {
            let level: ThinkingLevel = serde_json::from_value(value).map_err(|e| e.to_string())?;
            save(convert::<String>(level).into())
        }
        (key, Value::Null) if OPTIONAL.contains(&key) => {
            tiechui_config::remove_preference(&path, key)
        }
        ("network.proxy" | "network.no_proxy" | "network.ca_cert", Value::String(text)) => {
            save(text.as_str().into())
        }
        ("ask.auto_continue_after_sec" | "sessions.archive_after_days", Value::Number(number)) => {
            save(number.as_i64().ok_or("expected a whole number")?.into())
        }
        (key, _) if OPTIONAL.contains(&key) => return Err(format!("{key} has the wrong type")),
        (key, _) => return Err(format!("{key} cannot be set here")),
    }
    .map_err(|e| e.to_string())?;
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
