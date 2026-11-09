//! `chui setup`, which `chui` also runs while no provider is configured: one provider and model,
//! saved to the user profile and selected.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use futures::StreamExt;
use tiechui_config::{
    ApiKey, Config, ConfigPaths, ModelConfig, NetworkConfig, ProviderConfig, Wire,
};
use tiechui_model::{Content, Message, ModelId, ModelRequest, ReasoningLevel, Role};
use tiechui_tui::consent::{Answer, Input, Question, ask};
use tokio_util::sync::CancellationToken;

const FORMATS: [(&str, Wire); 3] = [
    ("Anthropic Messages", Wire::AnthropicMessages),
    ("OpenAI Chat Completions", Wire::OpenaiCompletions),
    ("OpenAI Responses", Wire::OpenaiResponses),
];

/// `chui setup`.
pub(crate) async fn command() -> i32 {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("chui: setup asks questions and needs a terminal");
        return 2;
    }
    let paths = match std::env::current_dir()
        .map_err(|error| error.to_string())
        .and_then(|cwd| ConfigPaths::discover(&cwd).map_err(|error| error.to_string()))
    {
        Ok(paths) => paths,
        Err(message) => {
            eprintln!("chui: {message}");
            return 2;
        }
    };
    match run(&paths).await {
        Ok(Some(_)) => 0,
        Ok(None) => 1,
        Err(message) => {
            eprintln!("chui: {message}");
            2
        }
    }
}

/// Whether `chui` needs setup first: no provider is declared, so no session could open.
pub(crate) fn needed(paths: &ConfigPaths) -> bool {
    tiechui_config::load_providers(&paths.providers()).is_ok_and(|providers| providers.is_empty())
}

/// The user configuration alone: projects set neither providers nor the network.
pub(crate) fn user_config(user: &Path) -> Result<Config, String> {
    let paths = ConfigPaths {
        user: user.to_owned(),
        project: None,
    };
    tiechui_config::load(&paths, false, &toml::Table::new())
        .map(|loaded| loaded.config)
        .map_err(|e| e.to_string())
}

/// Asks, checks and saves; the selected `<provider>/<model>`, or `None` when the user quit.
pub(crate) async fn run(paths: &ConfigPaths) -> Result<Option<String>, String> {
    let taken = tiechui_config::load_providers(&paths.providers()).map_err(|e| e.to_string())?;
    let network = user_config(&paths.user)?.network;
    loop {
        let Some(format) = choose(
            "Welcome to Tiechui",
            vec!["Choose the API format. Run chui setup to add another provider later.".into()],
            FORMATS
                .iter()
                .map(|(label, _)| (*label).to_owned())
                .collect(),
        )?
        else {
            return Ok(None);
        };
        let Some(url) = text(
            "API address",
            vec!["/v1 is added unless the address has a version, as in /api/paas/v4.".into()],
            "",
            false,
            |url| {
                (url.starts_with("https://") || url.starts_with("http://"))
                    .then(|| url.trim_end_matches('/').to_owned())
                    .ok_or("Start with https:// or http://.")
            },
        )?
        else {
            return Ok(None);
        };
        let Some((api_key, api_key_env)) = key()? else {
            return Ok(None);
        };
        let name = unique(host_name(&url), &taken);
        let mut config = ProviderConfig {
            wire: FORMATS[format].1,
            base_url: Some(url.clone()),
            api_key,
            api_key_env,
            compat: Default::default(),
            models: Vec::new(),
        };
        let cancel = CancellationToken::new();
        let listed = pending(
            "Fetching models…",
            tiechui_providers::list_models(&config, &network, &cancel),
        )
        .await;
        let Some(ids) = (match listed {
            Ok(ids) if !ids.is_empty() => {
                let body = vec![format!(
                    "{} models at {url}. Space checks several; the first is selected. Type to \
                     search, or enter an id that is not listed.",
                    ids.len()
                )];
                search("Models", body, ids)?
            }
            listed => {
                let reason = match listed {
                    Err(error) => format!("The models could not be listed: {error}"),
                    Ok(_) => "The service lists no models.".into(),
                };
                text("Model id", vec![reason], "", false, |id| {
                    Ok::<_, &str>(vec![id.to_owned()])
                })?
            }
        }) else {
            return Ok(None);
        };
        let subject = match ids.as_slice() {
            [id] => id.clone(),
            ids => format!("each of the {} models", ids.len()),
        };
        let Some(context_window) = text(
            "Context window",
            vec![format!(
                "The most tokens {subject} reads at once, such as 200000 or 1m."
            )],
            "200000",
            false,
            tokens,
        )?
        else {
            return Ok(None);
        };
        let Some(max_output) = text(
            "Max output",
            vec![format!("The most tokens {subject} writes in one reply.")],
            "32000",
            false,
            tokens,
        )?
        else {
            return Ok(None);
        };
        let Some(reasoning) = choose(
            "Reasoning",
            vec![format!(
                "Does {subject} think before answering? Thinking levels need it."
            )],
            vec!["Yes".into(), "No".into()],
        )?
        else {
            return Ok(None);
        };
        let reasoning = reasoning == 0;
        config.models = ids
            .iter()
            .map(|id| tiechui_config::ModelConfig {
                id: id.clone(),
                cache_retention: Default::default(),
                prices: None,
                fallback_models: Vec::new(),
                context_window,
                max_output,
                reasoning,
                thinking: Default::default(),
                thinking_levels: None,
                compat: None,
                images: false,
            })
            .collect();
        let model = format!("{name}/{}", ids[0]);
        if let Err(error) = pending(
            &format!("Checking {model}…"),
            check(&name, &config, &config.models[0], &network),
        )
        .await
        {
            match choose(
                "The check failed",
                vec![
                    error,
                    String::new(),
                    "providers.toml can be edited later.".into(),
                ],
                vec!["Save anyway".into(), "Start over".into(), "Quit".into()],
            )? {
                Some(0) => {}
                Some(1) => continue,
                _ => return Ok(None),
            }
        }
        save(paths, &name, &config, &model)?;
        let _ = writeln!(
            std::io::stdout(),
            "Saved {} to {} and selected {model} in {}.",
            match ids.len() {
                1 => "1 model".to_owned(),
                count => format!("{count} models"),
            },
            paths.providers().display(),
            paths.user.display()
        );
        return Ok(Some(model));
    }
}

/// `api_key` and `api_key_env`: the inline key, or the variable holding it.
type Key = (Option<ApiKey>, Option<String>);

fn key() -> Result<Option<Key>, String> {
    let choices = vec![
        "Paste a key, saved in providers.toml for you alone".to_owned(),
        "Read it from an environment variable".to_owned(),
    ];
    let Some(choice) = choose("API key", Vec::new(), choices)? else {
        return Ok(None);
    };
    Ok(match choice {
        0 => text("API key", Vec::new(), "", true, |key| {
            Ok::<_, &str>(key.to_owned())
        })?
        .map(|key| (Some(ApiKey(key)), None)),
        _ => text(
            "Environment variable",
            vec!["Export it in the shell that starts chui.".into()],
            "",
            false,
            |name| {
                is_set(name)
                    .then(|| name.to_owned())
                    .ok_or_else(|| format!("{name} is not set in this shell."))
            },
        )?
        .map(|name| (None, Some(name))),
    })
}

/// One request to the model with its declared limits, ended at its first event.
pub(crate) async fn check(
    name: &str,
    config: &ProviderConfig,
    model: &ModelConfig,
    network: &NetworkConfig,
) -> Result<(), String> {
    let provider =
        tiechui_providers::from_config(name, config, network).map_err(|e| e.to_string())?;
    let cancel = CancellationToken::new();
    let request = ModelRequest {
        disable_cache: true,
        model: ModelId(format!("{name}/{}", model.id)),
        messages: vec![Message {
            role: Role::User,
            content: vec![Content::Text {
                text: "Reply with OK.".into(),
                opaque: None,
            }],
            opaque: None,
        }],
        input_tokens: None,
        tools: Vec::new(),
        max_output: None,
        reasoning: Some(ReasoningLevel::Low)
            .filter(|low| tiechui_providers::offered(model).contains(&Some(*low))),
    };
    let first = match provider.stream(request, cancel.clone()).await {
        Ok(mut stream) => stream.next().await,
        Err(error) => return Err(error.to_string()),
    };
    cancel.cancel();
    match first {
        Some(Err(error)) => Err(error.to_string()),
        _ => Ok(()),
    }
}

fn save(
    paths: &ConfigPaths,
    name: &str,
    config: &ProviderConfig,
    model: &str,
) -> Result<(), String> {
    let quote = |text: &str| toml::Value::from(text).to_string();
    let wire = toml::Value::try_from(config.wire).map_err(|e| e.to_string())?;
    let mut table = format!("[{name}]\nwire = {wire}\n");
    for (key, value) in [
        ("base_url", config.base_url.as_deref()),
        ("api_key", config.api_key.as_ref().map(|key| key.0.as_str())),
        ("api_key_env", config.api_key_env.as_deref()),
    ] {
        if let Some(value) = value {
            table += &format!("{key} = {}\n", quote(value));
        }
    }
    for declared in &config.models {
        table += &format!(
            "\n[[{name}.models]]\nid = {}\ncontext_window = {}\nmax_output = {}\nreasoning = {}\nimages = {}\n",
            quote(&declared.id),
            declared.context_window,
            declared.max_output,
            declared.reasoning,
            declared.images
        );
    }
    let file = paths.providers();
    tiechui_config::add_provider(&file, name, &table).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    if config.api_key.is_some() {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{}: {e}", file.display()))?;
    }
    tiechui_config::save_preference(&paths.user, "model", model.into()).map_err(|e| e.to_string())
}

/// A gateway's provider name: the host's main label, such as `deepseek` for `api.deepseek.com`.
fn host_name(url: &str) -> String {
    let host = url
        .split_once("://")
        .map_or(url, |(_, rest)| rest)
        .split(['/', ':'])
        .next()
        .unwrap_or_default()
        .to_lowercase();
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return "local".into();
    }
    let labels: Vec<&str> = host.split('.').filter(|label| *label != "api").collect();
    let label = labels
        .get(labels.len().saturating_sub(2))
        .copied()
        .unwrap_or_default();
    if !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        label.to_owned()
    } else {
        "gateway".into()
    }
}

/// `name`, or `name-2`, `name-3`… when it is taken.
fn unique(name: String, taken: &BTreeMap<String, ProviderConfig>) -> String {
    (1..)
        .map(|n| match n {
            1 => name.clone(),
            n => format!("{name}-{n}"),
        })
        .find(|candidate| !taken.contains_key(candidate))
        .unwrap_or(name)
}

/// A positive token count; `k` and `m` suffixes scale it.
fn tokens(text: &str) -> Result<u64, &'static str> {
    let text = text.to_lowercase().replace(['_', ','], "");
    let (digits, scale) = match text.as_bytes().last() {
        Some(b'k') => (&text[..text.len() - 1], 1_000),
        Some(b'm') => (&text[..text.len() - 1], 1_000_000),
        _ => (text.as_str(), 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|count| count.checked_mul(scale))
        .filter(|count| *count > 0)
        .ok_or("Enter a positive number of tokens.")
}

fn is_set(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| !value.is_empty())
}

/// Shows `label` until `work` finishes.
async fn pending<T>(label: &str, work: impl Future<Output = T>) -> T {
    let mut out = std::io::stdout();
    let _ = write!(out, "\n  {label}");
    let _ = out.flush();
    let result = work.await;
    let _ = write!(out, "\r\x1b[2K\x1b[1A");
    let _ = out.flush();
    result
}

fn prompt(question: Question) -> Result<Option<Answer>, String> {
    match ask(&question).map_err(|e| e.to_string())? {
        Answer::Dismissed | Answer::Interrupted => Ok(None),
        answer => Ok(Some(answer)),
    }
}

fn choose(title: &str, body: Vec<String>, choices: Vec<String>) -> Result<Option<usize>, String> {
    Ok(prompt(Question {
        title: title.into(),
        body,
        choices,
        dismiss: "quit",
        input: None,
        multiple: false,
    })?
    .and_then(|answer| match answer {
        Answer::Choice(index) => Some(index),
        _ => None,
    }))
}

/// The checked choices, or the typed text when none matches it.
fn search(
    title: &str,
    body: Vec<String>,
    choices: Vec<String>,
) -> Result<Option<Vec<String>>, String> {
    let question = Question {
        title: title.into(),
        body,
        choices,
        dismiss: "quit",
        input: Some(Input {
            text: String::new(),
            secret: false,
        }),
        multiple: true,
    };
    Ok(match ask(&question).map_err(|e| e.to_string())? {
        Answer::Choices(indices) => Some(
            indices
                .into_iter()
                .map(|index| question.choices[index].clone())
                .collect(),
        ),
        Answer::Text(text) => Some(vec![text]),
        _ => None,
    })
}

/// Typed text that `parse` accepts; its error is shown and the question asked again.
fn text<T, E: std::fmt::Display>(
    title: &str,
    body: Vec<String>,
    initial: &str,
    secret: bool,
    parse: impl Fn(&str) -> Result<T, E>,
) -> Result<Option<T>, String> {
    let mut problem = None;
    let mut initial = initial.to_owned();
    loop {
        let mut lines = body.clone();
        if let Some(problem) = problem.take() {
            lines.extend([String::new(), problem]);
        }
        let Some(Answer::Text(typed)) = prompt(Question {
            title: title.into(),
            body: lines,
            choices: Vec::new(),
            dismiss: "quit",
            input: Some(Input {
                text: initial.clone(),
                secret,
            }),
            multiple: false,
        })?
        else {
            return Ok(None);
        };
        match parse(&typed) {
            Ok(value) => return Ok(Some(value)),
            Err(error) => {
                problem = Some(error.to_string());
                initial = typed;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateways_are_named_after_their_host() {
        for (url, name) in [
            ("https://api.deepseek.com", "deepseek"),
            ("https://api.anthropic.com", "anthropic"),
            ("https://openrouter.ai/api/v1", "openrouter"),
            ("https://open.bigmodel.cn/api/paas/v4", "bigmodel"),
            ("http://localhost:11434", "localhost"),
            ("http://127.0.0.1:8000/v1", "local"),
        ] {
            assert_eq!(host_name(url), name, "{url}");
        }
        let taken = BTreeMap::from_iter(["deepseek", "deepseek-2"].map(|name| {
            (
                name.to_owned(),
                ProviderConfig {
                    wire: Wire::OpenaiCompletions,
                    base_url: None,
                    api_key: None,
                    api_key_env: None,
                    compat: Default::default(),
                    models: Vec::new(),
                },
            )
        }));
        assert_eq!(unique("deepseek".into(), &taken), "deepseek-3");
    }

    #[test]
    fn token_counts_accept_suffixes() {
        assert_eq!(tokens("200000"), Ok(200_000));
        assert_eq!(tokens("200k"), Ok(200_000));
        assert_eq!(tokens("1M"), Ok(1_000_000));
        assert_eq!(tokens("128_000"), Ok(128_000));
        assert!(tokens("0").is_err() && tokens("lots").is_err() && tokens("").is_err());
    }
}
