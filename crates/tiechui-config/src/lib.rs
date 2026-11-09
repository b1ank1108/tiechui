//! Typed configuration with explicit project trust and environment-only credentials.

mod load;
mod range;
pub use range::*;
mod mcp;
mod paths;
mod permissions;
pub mod private;
mod save;
mod trust;

use std::collections::BTreeMap;
use std::env::VarError;
use std::fmt;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use load::{load, load_providers, load_with_mandatory_modules, providers_schema, schema};
pub use mcp::McpServerConfig;
pub use paths::ConfigPaths;
pub use permissions::{PermissionAction, PermissionMode, PermissionRule, PermissionsConfig};
pub use save::{
    add_provider, remove_mcp_server, remove_model, remove_preference, remove_provider,
    save_mcp_server, save_model, save_preference, save_provider, save_status_line,
};
pub use trust::{TrustEntry, TrustStore};

/// `HOME`, falling back to `USERPROFILE` where Windows leaves `HOME` unset.
pub fn user_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(std::path::PathBuf::from)
}

/// Built-in defaults leave model selection and providers unset. Providers come from
/// `providers.toml` beside the user config, never from a project or the command line.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Default model reference in `<provider>/<id>` form; the id may contain slashes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Default thinking level, clamped to the levels each model accepts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Overrides each registered module's default activation.
    pub modules: BTreeMap<String, bool>,
    pub retry: RetryConfig,
    pub compaction: CompactionConfig,
    pub permissions: PermissionsConfig,
    pub mcp_servers: BTreeMap<String, McpServerConfig>,
    pub tui: TuiConfig,
    /// User configuration only.
    pub network: NetworkConfig,
    pub ask: AskConfig,
    /// User configuration only.
    pub sessions: SessionsConfig,
    /// User configuration only.
    pub skills: SkillsConfig,
    #[serde(skip)]
    #[schemars(skip)]
    pub mcp_origins: BTreeMap<String, PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionConfig {
    #[schemars(range(min = 1, max = 9007199254740991u64))]
    pub context_window_cap: u64,
    #[schemars(range(min = 1, max = 9007199254740991u64))]
    pub fallback_context_window: u64,
    #[schemars(range(max = 9007199254740991u64))]
    pub preserve_recent_messages: usize,
    #[schemars(range(max = 9007199254740991u64))]
    pub preserve_recent_tokens: u64,
    pub protected_tools: Vec<String>,
    pub nudge: RangeNudge,
    pub tiers: RangeTiers,
    pub compress: RangeCompress,
    pub search: RangeSearch,
    pub output: RangeOutput,
    /// Ignored since read_history replaced decompress; kept so older configurations still load.
    pub decompress: RangeDecompress,

    /// Automatically compact at the context threshold and recover overflow (default: true).
    pub enabled: bool,
    /// Space reserved below the model window (default: 16384; zero is allowed).
    #[schemars(range(max = 9007199254740991u64))]
    pub reserve_tokens: u64,
    /// Recent history to retain at a safe message boundary (default: 20000).
    #[schemars(range(max = 9007199254740991u64))]
    pub keep_recent_tokens: u64,
    /// Per-model token overrides keyed by `<provider>/<model>`; unspecified fields inherit.
    pub model_overrides: BTreeMap<String, CompactionTokenOverride>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CompactionTokenOverride {
    #[schemars(range(max = 9007199254740991u64))]
    pub reserve_tokens: Option<u64>,
    #[schemars(range(max = 9007199254740991u64))]
    pub keep_recent_tokens: Option<u64>,
}

impl CompactionConfig {
    pub fn for_model(&self, key: &str) -> Self {
        let mut resolved = self.clone();
        if let Some(overrides) = self.model_overrides.get(key) {
            resolved.reserve_tokens = overrides.reserve_tokens.unwrap_or(self.reserve_tokens);
            resolved.keep_recent_tokens = overrides
                .keep_recent_tokens
                .unwrap_or(self.keep_recent_tokens);
        }
        resolved
    }
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16384,
            keep_recent_tokens: 20000,
            model_overrides: BTreeMap::new(),
            context_window_cap: 300000,
            fallback_context_window: 32768,
            preserve_recent_messages: 5,
            preserve_recent_tokens: 5000,
            protected_tools: vec![],
            nudge: RangeNudge::default(),
            tiers: RangeTiers::default(),
            compress: RangeCompress::default(),
            search: RangeSearch::default(),
            output: RangeOutput::default(),
            decompress: RangeDecompress::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RetryConfig {
    /// Enable automatic transient-error retries (default: true).
    pub enabled: bool,
    /// Retries after the initial request, independently for each tool step.
    pub max_retries: u32,
    /// Initial exponential backoff in milliseconds (default: 2000, must be positive).
    pub base_delay_ms: u64,
    /// Caps backoff; a longer server-requested delay fails immediately.
    pub max_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2000,
            max_delay_ms: 60000,
        }
    }
}

/// HTTP settings for provider and MCP requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkConfig {
    /// Proxy URL for every request; unset leaves the `*_PROXY` environment variables in effect.
    pub proxy: Option<String>,
    /// Comma-separated hosts, domains and IP ranges reached without `proxy`.
    pub no_proxy: Option<String>,
    /// PEM file of root certificates trusted besides the system's.
    pub ca_cert: Option<PathBuf>,
}

impl NetworkConfig {
    /// `builder` with the proxy and the extra root certificates, read now.
    pub fn apply(
        &self,
        mut builder: reqwest::ClientBuilder,
    ) -> Result<reqwest::ClientBuilder, String> {
        if let Some(proxy) = self.proxy()? {
            builder = builder.proxy(proxy);
        }
        if let Some(path) = &self.ca_cert {
            let error = |e: &dyn fmt::Display| format!("network.ca_cert {}: {e}", path.display());
            let pem = std::fs::read(path).map_err(|e| error(&e))?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| error(&e))?;
            if certs.is_empty() {
                return Err(error(&"no certificates"));
            }
            builder = certs
                .into_iter()
                .fold(builder, reqwest::ClientBuilder::add_root_certificate);
        }
        Ok(builder)
    }

    pub(crate) fn proxy(&self) -> Result<Option<reqwest::Proxy>, String> {
        let Some(url) = self.proxy.as_deref() else {
            return Ok(None);
        };
        // reqwest takes these, then fails each request.
        if let Some((scheme, _)) = url.split_once("://")
            && !["http", "https"].contains(&scheme.to_ascii_lowercase().as_str())
        {
            return Err(format!("network.proxy: {scheme} proxies are not supported"));
        }
        let no_proxy = self
            .no_proxy
            .as_deref()
            .and_then(reqwest::NoProxy::from_string);
        match reqwest::Proxy::all(url) {
            Ok(proxy) => Ok(Some(proxy.no_proxy(no_proxy))),
            Err(e) => {
                // reqwest wraps what is wrong with the URL in "builder error"s.
                let mut cause: &dyn std::error::Error = &e;
                while let Some(source) = cause.source() {
                    cause = source;
                }
                Err(format!("network.proxy: {cause}"))
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AskConfig {
    /// Seconds to wait for answers before the agent continues on its own judgment; unset waits.
    #[schemars(range(min = 1, max = 9007199254740991u64))]
    pub auto_continue_after_sec: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SessionsConfig {
    /// Days without activity after which `chui app-server` archives a session; unset keeps them.
    #[schemars(range(min = 1, max = 9007199254740991u64))]
    pub archive_after_days: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SkillsConfig {
    /// Names of discovered skills that sessions leave out.
    pub disabled: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TuiConfig {
    /// Syntax theme name; `automatic` follows the terminal's background.
    pub theme: Option<String>,
    /// Copy non-blank text selections to the terminal on mouse release.
    pub copy_on_select: bool,
    pub status_line: StatusLineConfig,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            theme: None,
            copy_on_select: true,
            status_line: StatusLineConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct StatusLineConfig {
    /// Items in display order; an empty list keeps only the shortcut and context hint.
    pub items: Vec<StatusLineItem>,
    /// False shows every item in the dim text color.
    pub use_theme_colors: bool,
    /// Items drawn as Nerd Font v3 glyphs; entries for hidden items are kept.
    pub icons: Vec<StatusLineItem>,
}

impl Default for StatusLineConfig {
    fn default() -> Self {
        Self {
            items: vec![
                StatusLineItem::ModelWithReasoning,
                StatusLineItem::CurrentDir,
                StatusLineItem::GitBranch,
                StatusLineItem::PermissionMode,
                StatusLineItem::ContextRemaining,
                StatusLineItem::ContextWindowSize,
            ],
            use_theme_colors: true,
            icons: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum StatusLineItem {
    Model,
    ModelWithReasoning,
    Reasoning,
    CurrentDir,
    ProjectName,
    Hostname,
    GitBranch,
    BranchChanges,
    RunState,
    PermissionMode,
    ContextRemaining,
    ContextUsed,
    Version,
    ContextWindowSize,
    UsedTokens,
    TotalInputTokens,
    TotalOutputTokens,
    CacheHitRate,
    EstimatedThreadCost,
    ThreadId,
    ThreadTitle,
}

impl StatusLineItem {
    pub const ALL: [Self; 21] = [
        Self::Model,
        Self::ModelWithReasoning,
        Self::Reasoning,
        Self::CurrentDir,
        Self::ProjectName,
        Self::Hostname,
        Self::GitBranch,
        Self::BranchChanges,
        Self::RunState,
        Self::PermissionMode,
        Self::ContextRemaining,
        Self::ContextUsed,
        Self::Version,
        Self::ContextWindowSize,
        Self::UsedTokens,
        Self::TotalInputTokens,
        Self::TotalOutputTokens,
        Self::CacheHitRate,
        Self::EstimatedThreadCost,
        Self::ThreadId,
        Self::ThreadTitle,
    ];

    /// The configuration spelling.
    pub fn id(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::ModelWithReasoning => "model-with-reasoning",
            Self::Reasoning => "reasoning",
            Self::CurrentDir => "current-dir",
            Self::ProjectName => "project-name",
            Self::Hostname => "hostname",
            Self::GitBranch => "git-branch",
            Self::BranchChanges => "branch-changes",
            Self::RunState => "run-state",
            Self::PermissionMode => "permission-mode",
            Self::ContextRemaining => "context-remaining",
            Self::ContextUsed => "context-used",
            Self::Version => "version",
            Self::ContextWindowSize => "context-window-size",
            Self::UsedTokens => "used-tokens",
            Self::TotalInputTokens => "total-input-tokens",
            Self::TotalOutputTokens => "total-output-tokens",
            Self::CacheHitRate => "cache-hit-rate",
            Self::EstimatedThreadCost => "estimated-thread-cost",
            Self::ThreadId => "thread-id",
            Self::ThreadTitle => "thread-title",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Wire {
    AnthropicMessages,
    OpenaiCompletions,
    OpenaiResponses,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    pub wire: Wire,
    /// API root; `/v1` is added unless a path segment is already a version. When absent,
    /// endpoint selection belongs to the provider implementation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Inline key; set this or `api_key_env`, not both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<ApiKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub compat: CompletionsCompat,
    #[serde(default)]
    pub models: Vec<ModelConfig>,
}

/// Keeps inline keys out of `Debug` output and logs.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct ApiKey(pub String);

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ApiKey(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelConfig {
    pub id: String,
    /// Anthropic cache retention: none, short (default), or long (one hour).
    #[serde(default)]
    pub cache_retention: CacheRetention,
    /// USD per million tokens. Omit when prices are unknown; no cost is then reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prices: Option<ModelPrices>,
    /// Server-side alternatives, using model IDs from this Anthropic provider.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_models: Vec<String>,
    pub context_window: u64,
    pub max_output: u64,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub thinking: Thinking,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "thinking_levels_schema")]
    pub thinking_levels: Option<BTreeMap<ThinkingLevel, ThinkingLevelValue>>,
    /// Each supplied member replaces the provider's value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compat: Option<CompatOverride>,
    #[serde(default)]
    pub images: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelPrices {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl ModelPrices {
    pub fn valid(&self) -> bool {
        [self.input, self.output, self.cache_read, self.cache_write]
            .into_iter()
            .all(|rate| rate.is_finite() && rate >= 0.0)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Thinking {
    #[default]
    Budget,
    Adaptive,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CompletionsCompat {
    pub supports_store: bool,
    pub supports_developer_role: bool,
    pub supports_strict_mode: bool,
    pub supports_reasoning_effort: bool,
    pub supports_usage_in_streaming: bool,
    pub max_tokens_field: MaxTokensField,
    pub thinking_format: ThinkingFormat,
    pub requires_reasoning_content_on_assistant_messages: bool,
}

impl Default for CompletionsCompat {
    fn default() -> Self {
        Self {
            supports_store: true,
            supports_developer_role: true,
            supports_strict_mode: false,
            supports_reasoning_effort: true,
            supports_usage_in_streaming: true,
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            thinking_format: ThinkingFormat::Openai,
            requires_reasoning_content_on_assistant_messages: false,
        }
    }
}

impl CompletionsCompat {
    pub fn with(&self, model: Option<&CompatOverride>) -> Self {
        let Some(model) = model else {
            return self.clone();
        };
        Self {
            supports_store: model.supports_store.unwrap_or(self.supports_store),
            supports_developer_role: model
                .supports_developer_role
                .unwrap_or(self.supports_developer_role),
            supports_strict_mode: model
                .supports_strict_mode
                .unwrap_or(self.supports_strict_mode),
            supports_reasoning_effort: model
                .supports_reasoning_effort
                .unwrap_or(self.supports_reasoning_effort),
            supports_usage_in_streaming: model
                .supports_usage_in_streaming
                .unwrap_or(self.supports_usage_in_streaming),
            max_tokens_field: model.max_tokens_field.unwrap_or(self.max_tokens_field),
            thinking_format: model.thinking_format.unwrap_or(self.thinking_format),
            requires_reasoning_content_on_assistant_messages: model
                .requires_reasoning_content_on_assistant_messages
                .unwrap_or(self.requires_reasoning_content_on_assistant_messages),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CompatOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_developer_role: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_strict_mode: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_reasoning_effort: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_usage_in_streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens_field: Option<MaxTokensField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_format: Option<ThinkingFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_reasoning_content_on_assistant_messages: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingFormat {
    Deepseek,
    Openai,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ThinkingLevelValue {
    Wire(String),
    Unsupported(UnsupportedLevel),
}

fn thinking_levels_schema(
    generator: &mut schemars::r#gen::SchemaGenerator,
) -> schemars::schema::Schema {
    let mut schema = generator
        .subschema_for::<Option<BTreeMap<ThinkingLevel, ThinkingLevelValue>>>()
        .into_object();
    schema
        .object
        .get_or_insert_with(Default::default)
        .property_names = Some(Box::new(generator.subschema_for::<ThinkingLevel>()));
    schema.into()
}

/// The only boolean accepted in a level map is false.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "bool", into = "bool")]
pub struct UnsupportedLevel;

impl TryFrom<bool> for UnsupportedLevel {
    type Error = &'static str;

    fn try_from(value: bool) -> Result<Self, Self::Error> {
        if value {
            Err("expected false")
        } else {
            Ok(Self)
        }
    }
}

impl From<UnsupportedLevel> for bool {
    fn from(_: UnsupportedLevel) -> Self {
        false
    }
}

impl JsonSchema for UnsupportedLevel {
    fn schema_name() -> String {
        "UnsupportedLevel".into()
    }

    fn json_schema(_: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        schemars::schema::SchemaObject {
            instance_type: Some(schemars::schema::InstanceType::Boolean.into()),
            enum_values: Some(vec![false.into()]),
            ..Default::default()
        }
        .into()
    }
}

impl ProviderConfig {
    /// Resolve credentials only on request; loading config never reads environment values.
    pub fn api_key(&self) -> Result<Option<String>, ConfigError> {
        self.api_key_with(|name| std::env::var(name))
    }

    fn api_key_with(
        &self,
        lookup: impl FnOnce(&str) -> Result<String, VarError>,
    ) -> Result<Option<String>, ConfigError> {
        if let Some(ApiKey(key)) = &self.api_key {
            return Ok(Some(key.clone()));
        }
        let Some(name) = &self.api_key_env else {
            return Ok(None);
        };
        validate_env_name(Path::new("<environment>"), name, name)?;
        match lookup(name) {
            Ok(value) if !value.is_empty() => Ok(Some(value)),
            Ok(_) => Err(ConfigError::new("<environment>", name, "variable is empty")),
            Err(VarError::NotPresent) => Err(ConfigError::new(
                "<environment>",
                name,
                "variable is not set",
            )),
            Err(VarError::NotUnicode(_)) => Err(ConfigError::new(
                "<environment>",
                name,
                "variable is not valid Unicode",
            )),
        }
    }
}

fn validate_env_name(file: &Path, key: &str, name: &str) -> Result<(), ConfigError> {
    let mut bytes = name.bytes();
    let valid = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    if !valid {
        return Err(ConfigError::new(
            file,
            key,
            "expected an environment variable name without a '$' prefix",
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub file: PathBuf,
    pub key: String,
    pub message: String,
}

impl ConfigError {
    pub(crate) fn new(
        file: impl Into<PathBuf>,
        key: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            file: file.into(),
            key: key.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            output,
            "{}: {}: {}",
            self.file.display(),
            self.key,
            self.message
        )
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IgnoredReason {
    UnknownMcpKey,
    MandatoryModule,
    UntrustedProject,
    ProjectPermissionsInvalid,
    UserOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub file: PathBuf,
    pub key: String,
    pub reason: IgnoredReason,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoadedConfig {
    pub config: Config,
    pub diagnostics: Vec<Diagnostic>,
}

#[cfg(test)]
mod tests;
