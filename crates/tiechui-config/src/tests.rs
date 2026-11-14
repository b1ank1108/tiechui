use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::*;

const USER: &str = r#"
model = "local/small"
[modules]
editing = true
future_module = false
"#;

const PROVIDERS: &str = r#"
[local]
wire = "openai-completions"
base_url = "https://user.invalid/v1"
api_key_env = "FIXTURE_API_KEY"
[[local.models]]
id = "small"
context_window = 32000
max_output = 4000
"#;

fn providers(text: &str) -> Result<BTreeMap<String, ProviderConfig>, ConfigError> {
    load_providers(&write(temp_dir().path(), "providers.toml", text))
}

#[test]
fn cache_retention_and_prices_have_schema_hints_and_validate() {
    for retention in ["none", "short", "long"] {
        let loaded = providers(&format!("{PROVIDERS}cache_retention='{retention}'\nprices={{ input=2.0, output=10.0, cache_read=0.2, cache_write=2.5 }}")).unwrap();
        assert!(loaded["local"].models[0].prices.is_some());
    }
    for value in ["-1.0", "nan", "inf"] {
        let error = providers(&format!(
            "{PROVIDERS}prices={{input={value}, output=1.0, cache_read=0.0, cache_write=0.0}}"
        ))
        .unwrap_err();
        assert_eq!(error.key, "local.models[0].prices");
    }
    let schema = serde_json::to_value(providers_schema()).unwrap();
    for field in ["prices", "cache_retention"] {
        assert!(
            schema["definitions"]["ModelConfig"]["properties"][field]["description"].is_string()
        );
    }
    assert!(serde_json::from_value::<ModelConfig>(serde_json::json!({"id":"bad", "context_window":1, "max_output":1, "cache_retention":"forever"})).is_err());
}

#[test]
fn retry_defaults_overrides_validation_and_fallbacks() {
    let temp = temp_dir();
    let paths = paths(
        temp.path(),
        "[retry]\nmax_retries=7\n",
        "[retry]\nenabled=false\n",
    );
    assert_eq!(
        load(&paths, false, &toml::Table::new())
            .unwrap()
            .config
            .retry
            .max_retries,
        7
    );
    let project = load(&paths, true, &toml::Table::new())
        .unwrap()
        .config
        .retry;
    assert!(!project.enabled);
    assert_eq!(project.max_retries, 3); // A retry table replaces the earlier table.
    for value in [
        "base_delay_ms=0",
        "base_delay_ms=10\nmax_delay_ms=9",
        "max_retries=-1",
        "enabled='yes'",
        "unknown=1",
    ] {
        let cli = toml::from_str(&format!("[retry]\n{value}")).unwrap();
        assert!(load(&paths, false, &cli).is_err(), "{value}");
    }
    for (wire, fallback, valid) in [
        ("anthropic-messages", "['backup']", true),
        ("openai-completions", "['backup']", false),
        ("anthropic-messages", "['missing']", false),
        ("anthropic-messages", "['main']", false),
        ("anthropic-messages", "['backup','backup']", false),
    ] {
        let text = format!(
            "[test]\nwire='{wire}'\nmodels=[{{id='main', context_window=10000, max_output=1000, fallback_models={fallback}}},{{id='backup', context_window=10000, max_output=1000}}]"
        );
        assert_eq!(providers(&text).is_ok(), valid);
    }
    let schema = serde_json::to_value(schema()).unwrap();
    assert!(schema["definitions"]["RetryConfig"]["properties"]["max_retries"].is_object());
    let schema = serde_json::to_value(providers_schema()).unwrap();
    assert!(schema["definitions"]["ModelConfig"]["properties"]["fallback_models"].is_object());
}

#[test]
fn compaction_defaults_layers_schema_and_safe_integer_validation() {
    let defaults = CompactionConfig::default();
    assert!(defaults.enabled);
    assert_eq!(
        (defaults.reserve_tokens, defaults.keep_recent_tokens),
        (16384, 20000)
    );
    let temp = temp_dir();
    let paths = paths(
        temp.path(),
        "[compaction]\nreserve_tokens=100\n",
        "[compaction]\nenabled=false\nkeep_recent_tokens=0\n",
    );
    let untrusted = load(&paths, false, &toml::Table::new())
        .unwrap()
        .config
        .compaction;
    assert_eq!(untrusted.reserve_tokens, 100);
    let trusted = load(&paths, true, &toml::Table::new())
        .unwrap()
        .config
        .compaction;
    assert!(!trusted.enabled);
    assert_eq!(trusted.keep_recent_tokens, 0);
    for invalid in [
        "reserve_tokens=-1",
        "keep_recent_tokens=1.5",
        "reserve_tokens=9007199254740992",
        "enabled='yes'",
        "unknown=1",
    ] {
        assert!(
            load(
                &paths,
                true,
                &toml::from_str(&format!("[compaction]\n{invalid}")).unwrap()
            )
            .is_err(),
            "{invalid}"
        );
    }
    let schema = serde_json::to_value(schema()).unwrap();
    for field in ["enabled", "reserve_tokens", "keep_recent_tokens"] {
        assert!(
            schema["definitions"]["CompactionConfig"]["properties"][field]["description"]
                .is_string()
        );
    }
    let overrides = toml::from_str("[compaction]\nreserve_tokens=100\n[compaction.model_overrides.'test/model']\nkeep_recent_tokens=0\n").unwrap();
    let config = load(&paths, true, &overrides).unwrap().config.compaction;
    let resolved = config.for_model("test/model");
    assert_eq!(
        (resolved.reserve_tokens, resolved.keep_recent_tokens),
        (100, 0)
    );
    assert_eq!(config.for_model("test/other").keep_recent_tokens, 20000);
    for invalid in [
        "reserve_tokens=-1",
        "keep_recent_tokens=9007199254740992",
        "enabled=false",
    ] {
        let overrides = toml::from_str(&format!(
            "[compaction.model_overrides.'test/model']\n{invalid}\n"
        ))
        .unwrap();
        assert!(load(&paths, true, &overrides).is_err());
    }
}

#[test]
fn permission_rules_are_ordered_strict_and_project_cannot_disable_the_module() {
    let temp = temp_dir();
    let user = "[[permissions.rules]]\nsurface='bash'\npattern='*'\naction='ask'\n[[permissions.rules]]\nsurface='bash'\npattern='git status'\naction='allow'\n";
    let paths = paths(
        temp.path(),
        user,
        "[modules]\npermissions=false\n[[permissions.rules]]\nsurface='write'\npattern='*'\naction='deny'\n",
    );
    let loaded =
        load_with_mandatory_modules(&paths, true, &toml::Table::new(), &["permissions"]).unwrap();
    assert_eq!(
        loaded.config.permissions.rules.as_ref().unwrap()[1].pattern,
        "git status"
    );
    assert_eq!(loaded.config.modules.get("permissions"), None);
    assert_eq!(loaded.diagnostics[0].reason, IgnoredReason::MandatoryModule);
    assert_eq!(loaded.config.permissions.project_rules.len(), 1);
    let untrusted =
        load_with_mandatory_modules(&paths, false, &toml::Table::new(), &["permissions"]).unwrap();
    assert!(untrusted.config.permissions.project_rules.is_empty());
    for text in [
        "surface='path_wrote'\npattern='*'\naction='allow'",
        "surface='read'\npattern='*'\naction='alow'",
        "surface='bash'\npattern='*'\naction='allow'\nreason='x'",
        "surface='*'\npattern='git *'\naction='allow'",
        "surface=' bash'\npattern='*'\naction='ask'",
        "surface='bash'\npattern=''\naction='ask'",
        "surface='bash'\npattern='*'\naction='deny'\nreason=''",
        "surface='bash'\npattern='*'\naction='ask'\nextra=true",
    ] {
        let cli = toml::from_str(&format!("[[permissions.rules]]\n{text}")).unwrap();
        assert!(load(&paths, false, &cli).is_err(), "{text}");
    }
    let cli = toml::from_str("[permissions]\nrules=[]").unwrap();
    assert_eq!(
        load(&paths, false, &cli).unwrap().config.permissions.rules,
        Some(vec![])
    );
    let cli = toml::from_str("[permissions]").unwrap();
    assert_eq!(
        load(&paths, false, &cli).unwrap().config.permissions.rules,
        None
    );
}

#[test]
fn saving_preferences_preserves_permission_rule_order_and_comments() {
    let temp = temp_dir();
    let text = "# Keep policy\n[[permissions.rules]]\nsurface='bash'\npattern='*'\naction='ask'\n[[permissions.rules]]\nsurface='bash'\npattern='git status'\naction='allow'\n";
    let file = write(temp.path(), "config.toml", text);
    save_preference(&file, "model", "local/model".into()).unwrap();
    let saved = fs::read_to_string(&file).unwrap();
    assert!(saved.contains(text));
}

#[test]
fn invalid_project_permission_rules_are_diagnosed_and_clamp_inherited_allows() {
    let temp = temp_dir();
    let paths = paths(
        temp.path(),
        "",
        "[[permissions.rules]]\nsurface='path_wrote'\npattern='*'\naction='allow'\n",
    );
    let loaded =
        load_with_mandatory_modules(&paths, true, &toml::Table::new(), &["permissions"]).unwrap();
    assert!(loaded.config.permissions.project_invalid);
    assert!(loaded.config.permissions.project_rules.is_empty());
    assert_eq!(
        loaded.diagnostics[0].reason,
        IgnoredReason::ProjectPermissionsInvalid
    );
    assert!(
        !load(&paths, false, &toml::Table::new())
            .unwrap()
            .config
            .permissions
            .project_invalid
    );
}

#[test]
fn permission_mode_is_user_only() {
    let temp = temp_dir();
    let paths = paths(
        temp.path(),
        "[permissions]\nmode='auto'\n",
        "[permissions]\nmode='yolo'\n",
    );
    let loaded = load(&paths, true, &toml::Table::new()).unwrap();
    assert_eq!(loaded.config.permissions.mode, Some(PermissionMode::Auto));
    assert_eq!(loaded.diagnostics[0].key, "permissions.mode");
    assert_eq!(loaded.diagnostics[0].reason, IgnoredReason::UserOnly);
}

#[test]
fn classifier_model_is_user_only_and_names_a_provider_model() {
    let temp = temp_dir();
    let valid = paths(
        temp.path(),
        "[permissions]\nclassifier_model='fast/reviewer'\n",
        "[permissions]\nclassifier_model='project/model'\n",
    );
    let loaded = load(&valid, true, &toml::Table::new()).unwrap();
    assert_eq!(
        loaded.config.permissions.classifier_model.as_deref(),
        Some("fast/reviewer")
    );
    assert_eq!(loaded.diagnostics[0].key, "permissions.classifier_model");
    assert_eq!(loaded.diagnostics[0].reason, IgnoredReason::UserOnly);
    let temp = temp_dir();
    let invalid = paths(
        temp.path(),
        "[permissions]\nclassifier_model='reviewer'\n",
        "",
    );
    let error = load(&invalid, true, &toml::Table::new()).unwrap_err();
    assert_eq!(error.key, "permissions.classifier_model");
}

#[test]
fn network_sessions_and_skills_are_user_only_and_validated() {
    let temp = temp_dir();
    let paths = paths(
        temp.path(),
        "[network]\nproxy='http://127.0.0.1:8080'\nno_proxy='localhost'\n[ask]\nauto_continue_after_sec=300\n[sessions]\narchive_after_days=7\n[skills]\ndisabled=['draft']\n",
        "[network]\nproxy='http://project.invalid'\n[ask]\nauto_continue_after_sec=60\n[sessions]\narchive_after_days=1\n[skills]\ndisabled=[]\n",
    );
    let loaded = load(&paths, true, &toml::Table::new()).unwrap();
    assert_eq!(
        loaded.config.network.proxy.as_deref(),
        Some("http://127.0.0.1:8080")
    );
    assert_eq!(loaded.config.sessions.archive_after_days, Some(7));
    assert_eq!(loaded.config.ask.auto_continue_after_sec, Some(60));
    assert_eq!(loaded.config.skills.disabled, ["draft"]);
    let ignored: Vec<_> = loaded
        .diagnostics
        .iter()
        .filter(|d| d.reason == IgnoredReason::UserOnly)
        .map(|d| d.key.as_str())
        .collect();
    assert_eq!(ignored, ["network", "sessions", "skills"]);

    for (text, key) in [
        (
            "[ask]\nauto_continue_after_sec=0\n",
            "ask.auto_continue_after_sec",
        ),
        (
            "[sessions]\narchive_after_days=0\n",
            "sessions.archive_after_days",
        ),
        ("[network]\nproxy='not a url'\n", "network"),
        ("[network]\nproxy='ftp://127.0.0.1:21'\n", "network"),
    ] {
        fs::write(&paths.user, text).unwrap();
        assert_eq!(
            load(&paths, false, &toml::Table::new()).unwrap_err().key,
            key
        );
    }
}

fn temp_dir() -> TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/config-tests");
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

#[test]
fn the_default_thinking_level_layers_like_the_model() {
    let temp = temp_dir();
    let paths = paths(temp.path(), "thinking = 'low'\n", "thinking = 'max'\n");
    let level = |trusted, cli: &str| {
        load(&paths, trusted, &toml::from_str(cli).unwrap())
            .unwrap()
            .config
            .thinking
    };
    assert_eq!(level(false, ""), Some(ThinkingLevel::Low));
    assert_eq!(level(true, ""), Some(ThinkingLevel::Max));
    assert_eq!(level(true, "thinking = 'off'"), Some(ThinkingLevel::Off));
    assert!(load(&paths, true, &toml::from_str("thinking = 'huge'").unwrap()).is_err());
}

#[test]
fn skills_section_takes_only_disabled_names() {
    let temp = temp_dir();
    let paths = paths(temp.path(), "[skills]\npaths = []\n", "");
    assert!(
        load(&paths, false, &toml::Table::new())
            .unwrap_err()
            .key
            .starts_with("skills")
    );
    let schema = serde_json::to_value(schema()).unwrap();
    assert!(schema["properties"]["skills"].is_object());
}

#[test]
fn thinking_level_maps_are_optional_strict_and_round_trip() {
    for (id, levels) in [
        (
            "claude-opus-5-5",
            "off = false\nminimal = false\nlow = 'low'\nmedium = 'medium'\nhigh = 'high'\nxhigh = 'xhigh'\nmax = 'max'",
        ),
        (
            "deepseek-v4.1-flash",
            "off = false\nminimal = false\nlow = 'low'\nmedium = false\nhigh = 'high'\nxhigh = false\nmax = 'max'",
        ),
    ] {
        let text = format!(
            "id = '{id}'\ncontext_window = 200000\nmax_output = 32000\n[thinking_levels]\n{levels}"
        );
        let model: ModelConfig = toml::from_str(&text).unwrap();
        assert_eq!(
            toml::from_str::<ModelConfig>(&toml::to_string(&model).unwrap()).unwrap(),
            model
        );
        assert!(toml::from_str::<ModelConfig>(&text.replace("off = false", "off = true")).is_err());
        assert!(
            toml::from_str::<ModelConfig>(&text.replace("off = false", "unknown = false")).is_err()
        );
        assert!(toml::from_str::<ModelConfig>(&text.replace("off = false", "off = 1")).is_err());
    }
}

#[test]
fn completions_compat_defaults_and_strict_values_survive_loading() {
    let defaults = providers(PROVIDERS).unwrap()["local"].compat.clone();
    assert_eq!(defaults, CompletionsCompat::default());
    assert!(
        defaults.supports_store
            && defaults.supports_developer_role
            && defaults.supports_reasoning_effort
            && defaults.supports_usage_in_streaming
    );
    assert!(
        !defaults.supports_strict_mode
            && !defaults.requires_reasoning_content_on_assistant_messages
    );
    assert_eq!(
        defaults.max_tokens_field,
        MaxTokensField::MaxCompletionTokens
    );
    assert_eq!(defaults.thinking_format, ThinkingFormat::Openai);
    let compat = "[local.compat]\nsupports_store = false\nsupports_developer_role = false\nsupports_strict_mode = false\nsupports_reasoning_effort = false\nsupports_usage_in_streaming = false\nmax_tokens_field = 'max_tokens'\nthinking_format = 'deepseek'\nrequires_reasoning_content_on_assistant_messages = true\n";
    let temp = temp_dir();
    let paths = paths(temp.path(), USER, "");
    write(
        temp.path(),
        "user/providers.toml",
        &format!("{PROVIDERS}\n{compat}"),
    );
    let loaded = load(&paths, false, &toml::Table::new()).unwrap();
    let actual = &loaded.config.providers["local"].compat;
    assert!(
        !actual.supports_store
            && !actual.supports_developer_role
            && !actual.supports_reasoning_effort
            && !actual.supports_usage_in_streaming
    );
    assert!(
        !actual.supports_strict_mode && actual.requires_reasoning_content_on_assistant_messages
    );
    assert_eq!(actual.max_tokens_field, MaxTokensField::MaxTokens);
    assert_eq!(actual.thinking_format, ThinkingFormat::Deepseek);
    let round_trip: Config = toml::from_str(&toml::to_string(&loaded.config).unwrap()).unwrap();
    assert_eq!(round_trip, loaded.config);
    for invalid in [
        "supportsStore = true",
        "supports_store = 'false'",
        "max_tokens_field = 'limit'",
        "thinking_format = 'zai'",
        "supports_mid_convo_system_messages = true",
        "supports_mid_convo_tool_additions = true",
    ] {
        let error = providers(&format!("{PROVIDERS}\n[local.compat]\n{invalid}")).unwrap_err();
        assert!(error.key.starts_with("local.compat"), "{error}");
    }
}

#[test]
fn model_compat_overrides_single_provider_fields_and_round_trips() {
    let source = r#"
[gateway]
wire = "openai-completions"
[gateway.compat]
thinking_format = "deepseek"
supports_developer_role = false
supports_store = false
[[gateway.models]]
id = "deepseek-v4.1-flash"
context_window = 200000
max_output = 32000
reasoning = true
[[gateway.models]]
id = "gemini-3.8-flash"
context_window = 200000
max_output = 32000
reasoning = true
[gateway.models.compat]
thinking_format = "openai"
supports_developer_role = true
"#;
    let loaded = providers(source).unwrap();
    let provider = &loaded["gateway"];
    assert!(provider.models[0].compat.is_none());
    assert_eq!(provider.compat.with(None), provider.compat);
    let compat = provider.compat.with(provider.models[1].compat.as_ref());
    assert_eq!(compat.thinking_format, ThinkingFormat::Openai);
    assert!(compat.supports_developer_role && !compat.supports_store);
    let round_trip: BTreeMap<String, ProviderConfig> =
        toml::from_str(&toml::to_string(&loaded).unwrap()).unwrap();
    assert_eq!(round_trip, loaded);
    for invalid in [
        "supportsStore = true",
        "supports_store = 'false'",
        "thinking_format = 'invalid'",
    ] {
        let error =
            providers(&source.replace("thinking_format = \"openai\"", invalid)).unwrap_err();
        assert!(error.key.starts_with("gateway.models[1].compat"), "{error}");
    }
}

#[test]
fn providers_are_only_read_from_the_user_providers_file() {
    let temp = temp_dir();
    let section = "[providers.local]\nwire = 'openai-completions'\n";
    let paths = paths(temp.path(), section, "");
    let error = load(&paths, false, &toml::Table::new()).unwrap_err();
    assert_eq!(
        (&error.file, error.key.as_str()),
        (&paths.user, "providers")
    );
    assert!(error.message.contains("providers.toml"));
    let paths = self::paths(temp.path(), "", section);
    let error = load(&paths, true, &toml::Table::new()).unwrap_err();
    assert_eq!(error.file, paths.project.clone().unwrap());
    let untrusted = load(&paths, false, &toml::Table::new()).unwrap();
    assert!(untrusted.config.providers.is_empty());
    assert!(
        untrusted
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.reason == IgnoredReason::UntrustedProject)
    );
    let cli = toml::from_str(section).unwrap();
    assert_eq!(
        load(&paths, false, &cli).unwrap_err().file,
        Path::new("<cli>")
    );
    let paths = self::paths(temp.path(), "", "");
    write(temp.path(), "project/.agents/providers.toml", PROVIDERS);
    assert!(
        load(&paths, true, &toml::Table::new())
            .unwrap()
            .config
            .providers
            .is_empty()
    );
    write(temp.path(), "user/providers.toml", PROVIDERS);
    assert_eq!(paths.providers(), temp.path().join("user/providers.toml"));
    let loaded = load(&paths, true, &toml::Table::new()).unwrap();
    assert_eq!(
        loaded.config.providers["local"].api_key_env.as_deref(),
        Some("FIXTURE_API_KEY")
    );
    // Internal helper processes drop declared provider keys along with the well-known ones.
    let scrubbed = scrubbed_secret_env_vars();
    assert!(scrubbed.iter().any(|name| name == "FIXTURE_API_KEY"));
    assert!(scrubbed.iter().any(|name| name == "OPENAI_API_KEY"));
}

fn write(root: &Path, name: &str, contents: &str) -> PathBuf {
    let path = root.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, contents).unwrap();
    path
}

fn paths(root: &Path, user: &str, project: &str) -> ConfigPaths {
    ConfigPaths {
        user: write(root, "user/config.toml", user),
        project: Some(write(root, "project/.agents/config.toml", project)),
    }
}

#[test]
fn merges_defaults_user_project_and_cli_without_interpreting_modules() {
    let temp = temp_dir();
    let paths = paths(
        temp.path(),
        USER,
        "model = \"local/project\"\n[modules]\nediting = false\nproject_only = true\n",
    );
    write(temp.path(), "user/providers.toml", PROVIDERS);
    let user = load(
        &ConfigPaths {
            project: None,
            ..paths.clone()
        },
        true,
        &toml::Table::new(),
    )
    .unwrap();
    assert_eq!(user.config.model.as_deref(), Some("local/small"));
    let model = &user.config.providers["local"].models[0];
    assert!(!model.reasoning && !model.images);
    assert_eq!(model.thinking, Thinking::Budget);
    let project = load(&paths, true, &toml::Table::new()).unwrap();
    assert_eq!(project.config.model.as_deref(), Some("local/project"));
    assert!(!project.config.modules["editing"]);
    assert!(project.config.modules["project_only"]);
    assert!(!project.config.modules["future_module"]);
    assert_eq!(project.config.providers, user.config.providers);
    let cli = toml::from_str("model = \"local/cli\"\n[modules]\nediting = true\n").unwrap();
    let final_config = load(&paths, true, &cli).unwrap().config;
    assert_eq!(final_config.model.as_deref(), Some("local/cli"));
    assert!(final_config.modules["editing"]);
    assert!(final_config.modules["project_only"]);
}

#[test]
fn missing_files_use_empty_built_in_defaults() {
    let temp = temp_dir();
    let paths = ConfigPaths {
        user: temp.path().join("missing.toml"),
        project: None,
    };
    let loaded = load(&paths, false, &toml::Table::new()).unwrap();
    assert_eq!(loaded.config, Config::default());
    assert!(loaded.diagnostics.is_empty());
}

#[test]
fn providers_schema_requires_a_wire_and_documents_both_key_sources() {
    let schema = providers_schema();
    let schemars::schema::Schema::Object(provider) = &schema.definitions["ProviderConfig"] else {
        panic!("provider must be an object schema");
    };
    let object = provider.object.as_ref().unwrap();
    assert!(object.required.contains("wire"));
    assert_eq!(object.additional_properties, Some(Box::new(false.into())));
    assert!(object.properties.contains_key("api_key_env"));
    assert!(object.properties.contains_key("api_key"));
    let config = serde_json::to_value(self::schema()).unwrap();
    assert!(config["properties"].get("providers").is_none());
}

#[test]
fn untrusted_project_keys_never_enter_the_merge() {
    let temp = temp_dir();
    let paths = paths(
        temp.path(),
        USER,
        r#"
model = "local/untrusted"
[modules]
editing = false
[providers.local]
api_key_env = "UNTRUSTED_KEY"
base_url = "https://untrusted.invalid"
"#,
    );
    write(temp.path(), "user/providers.toml", PROVIDERS);
    let loaded = load(&paths, false, &toml::Table::new()).unwrap();
    assert_eq!(loaded.config.model.as_deref(), Some("local/small"));
    assert!(loaded.config.modules["editing"]);
    assert_eq!(
        loaded.config.providers["local"].api_key_env.as_deref(),
        Some("FIXTURE_API_KEY")
    );
    assert_eq!(loaded.diagnostics.len(), 4);
    assert!(
        loaded
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.reason == IgnoredReason::UntrustedProject)
    );
    assert!(
        loaded
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.key == "modules.editing")
    );
}

#[test]
fn untrusted_project_read_and_parse_errors_are_diagnostics() {
    for failure in ["syntax", "encoding", "directory", "missing"] {
        let temp = temp_dir();
        let paths = paths(temp.path(), USER, "");
        let project = paths.project.as_ref().unwrap();
        match failure {
            "syntax" => fs::write(project, "model = 'sanitized-content").unwrap(),
            "encoding" => fs::write(project, b"sanitized-content\xff").unwrap(),
            _ => {
                fs::remove_file(project).unwrap();
                if failure == "directory" {
                    fs::create_dir(project).unwrap();
                }
            }
        }
        let cli = toml::from_str("[modules]\nediting = false").unwrap();
        let expected = load(
            &ConfigPaths {
                project: None,
                ..paths.clone()
            },
            false,
            &cli,
        )
        .unwrap();
        let loaded = load(&paths, false, &cli).unwrap();
        assert_eq!(loaded.config, expected.config);
        assert_eq!(
            loaded.diagnostics,
            vec![Diagnostic {
                file: project.clone(),
                key: "*".into(),
                reason: IgnoredReason::UntrustedProject,
            }]
        );
        assert!(!format!("{:?}", loaded.diagnostics).contains("sanitized-content"));
        if failure == "missing" {
            assert!(load(&paths, true, &cli).is_ok());
        } else {
            assert_eq!(load(&paths, true, &cli).unwrap_err().file, *project);
        }
    }
}

#[test]
fn unknown_provider_fields_and_wrong_types_report_the_file_and_key_path() {
    let temp = temp_dir();
    for (text, key) in [
        (
            "[local]\nwire = 'openai-completions'\nheaders = { custom = 'sanitized-header' }",
            "local.headers",
        ),
        (
            "[local]\nwire = 'openai-completions'\nbase_url = 42",
            "local.base_url",
        ),
        ("[local]\nwire = 'unsupported'", "local.wire"),
        (
            "[local]\nwire = 'openai-completions'\n[[local.models]]\nid = 'x'\ncontext_window = 'large'\nmax_output = 5",
            "local.models[0].context_window",
        ),
        (
            "[local]\nwire = 'openai-completions'\n[[local.models]]\nid = 'x'\ncontext_window = 20\nmax_output = 5\ntypo = true",
            "local.models[0].typo",
        ),
    ] {
        let path = write(temp.path(), "providers.toml", text);
        let error = load_providers(&path).unwrap_err();
        assert_eq!(error.file, path);
        assert_eq!(error.key, key, "{error}");
        assert!(!error.to_string().contains("sanitized"));
    }
}

#[test]
fn unknown_keys_and_wrong_types_report_the_file_and_full_key_path() {
    let temp = temp_dir();
    for (text, key) in [
        ("unknown = true", "unknown"),
        ("[modules]\nediting = 'yes'", "modules.editing"),
        ("[tui]\ntheme = 1", "tui.theme"),
    ] {
        let paths = paths(temp.path(), text, "");
        let error = load(&paths, true, &toml::Table::new()).unwrap_err();
        assert_eq!(error.file, paths.user);
        assert_eq!(error.key, key, "{error}");
        assert!(error.to_string().contains(paths.user.to_str().unwrap()));
        assert!(error.to_string().contains(key));
        assert!(!error.to_string().contains("sanitized-secret"));
    }
}

#[test]
fn invalid_lower_layers_cannot_be_hidden_by_overrides() {
    let temp = temp_dir();
    let paths = paths(temp.path(), "[modules]\nediting = 'yes'", "");
    let cli = toml::from_str("[modules]\nediting = true").unwrap();
    let error = load(&paths, true, &cli).unwrap_err();
    assert_eq!(error.file, paths.user);
    assert_eq!(error.key, "modules.editing");
    let paths = self::paths(temp.path(), "", "[modules]\nediting = 1");
    assert_eq!(
        load(&paths, true, &cli).unwrap_err().file,
        paths.project.unwrap()
    );
}

#[test]
fn cli_errors_and_missing_provider_wire_have_origins() {
    let temp = temp_dir();
    let paths = paths(temp.path(), "", "");
    let file = write(temp.path(), "user/providers.toml", "[local]\nmodels = []");
    let error = load(&paths, true, &toml::Table::new()).unwrap_err();
    assert_eq!((error.file, error.key.as_str()), (file, "local"));
    assert!(error.message.contains("wire"), "{}", error.message);
    let cli = toml::from_str("unexpected = true").unwrap();
    let error = load(
        &ConfigPaths {
            project: None,
            ..paths
        },
        false,
        &cli,
    )
    .unwrap_err();
    assert_eq!(error.file, Path::new("<cli>"));
    assert_eq!(error.key, "unexpected");
}

#[test]
fn model_reference_and_environment_name_are_validated() {
    let temp = temp_dir();
    for text in [
        "model = 'no-provider'",
        "model = '/id'",
        "model = 'provider/'",
    ] {
        let paths = paths(temp.path(), text, "");
        assert_eq!(
            load(&paths, false, &toml::Table::new()).unwrap_err().key,
            "model"
        );
    }
    assert_eq!(
        providers("[local]\nwire = 'anthropic-messages'\napi_key_env = '$KEY'")
            .unwrap_err()
            .key,
        "local.api_key_env"
    );
}

#[test]
fn credentials_resolve_from_the_inline_key_or_only_the_named_environment_variable() {
    let provider = ProviderConfig {
        wire: Wire::AnthropicMessages,
        base_url: None,
        api_key: None,
        api_key_env: Some("FIXTURE_API_KEY".into()),
        compat: Default::default(),
        models: vec![],
    };
    assert_eq!(
        provider
            .api_key_with(|name| {
                assert_eq!(name, "FIXTURE_API_KEY");
                Ok("sanitized-value".into())
            })
            .unwrap()
            .as_deref(),
        Some("sanitized-value")
    );
    assert!(
        provider
            .api_key_with(|_| Err(VarError::NotPresent))
            .unwrap_err()
            .to_string()
            .contains("FIXTURE_API_KEY")
    );
    let anonymous = ProviderConfig {
        api_key_env: None,
        ..provider
    };
    assert_eq!(
        anonymous
            .api_key_with(|_| panic!("must not inspect other environment variables"))
            .unwrap(),
        None
    );
    let inline = ProviderConfig {
        api_key: Some(ApiKey("sanitized-inline".into())),
        ..anonymous
    };
    assert_eq!(
        inline
            .api_key_with(|_| panic!("an inline key must not read the environment"))
            .unwrap()
            .as_deref(),
        Some("sanitized-inline")
    );
}

#[test]
fn inline_keys_are_exclusive_non_empty_and_redacted() {
    let base = "[local]\nwire = 'openai-completions'\n";
    for (text, message) in [
        (
            "api_key = 'sanitized-inline'\napi_key_env = 'FIXTURE_API_KEY'",
            "not both",
        ),
        ("api_key = ''", "must not be empty"),
    ] {
        let error = providers(&format!("{base}{text}")).unwrap_err();
        assert_eq!(error.key, "local.api_key");
        assert!(error.message.contains(message), "{error}");
        assert!(!error.to_string().contains("sanitized"));
    }
    let temp = temp_dir();
    let paths = paths(temp.path(), USER, "");
    write(
        temp.path(),
        "user/providers.toml",
        &PROVIDERS.replace(
            "api_key_env = \"FIXTURE_API_KEY\"",
            "api_key = \"sanitized-inline\"",
        ),
    );
    let loaded = load(&paths, false, &toml::Table::new()).unwrap();
    assert_eq!(
        loaded.config.providers["local"]
            .api_key()
            .unwrap()
            .as_deref(),
        Some("sanitized-inline")
    );
    assert!(!format!("{loaded:?}").contains("sanitized-inline"));
}

#[test]
fn home_override_and_git_root_discovery_use_injected_environment() {
    let temp = temp_dir();
    let root = temp.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("repo/.git")).unwrap();
    fs::create_dir_all(root.join("repo/a/b")).unwrap();
    let project = write(&root, "repo/.agents/config.toml", "");
    write(&root, ".tiechui/config.toml", "");
    let cwd = root.join("repo/a/b");
    let discovered = ConfigPaths::discover_with_env(&cwd, |name| match name {
        "TIECHUI_HOME" => Some(root.join("override").into_os_string()),
        "HOME" => panic!("override must take precedence over HOME"),
        _ => None,
    })
    .unwrap();
    assert_eq!(discovered.user, root.join("override/config.toml"));
    assert_eq!(discovered.project, Some(project));
    let nearer = write(&root, "repo/a/.agents/config.toml", "");
    let discovered = ConfigPaths::discover_with_env(&cwd, |name| {
        (name == "HOME").then(|| root.join("home").into_os_string())
    })
    .unwrap();
    assert_eq!(discovered.user, root.join("home/.tiechui/config.toml"));
    assert_eq!(discovered.project, Some(nearer));
    fs::remove_dir_all(root.join("repo/.agents")).unwrap();
    fs::remove_dir_all(root.join("repo/a/.agents")).unwrap();
    assert!(
        ConfigPaths::discover_with_env(&cwd, |_| Some(root.join("override").into_os_string()))
            .unwrap()
            .project
            .is_none()
    );
}

#[test]
fn git_worktree_markers_and_relative_home_overrides_are_supported() {
    let temp = temp_dir();
    let root = temp.path().canonicalize().unwrap();
    write(&root, ".tiechui/config.toml", "");
    let home = |_: &str| Some(OsString::from("relative-home"));
    let project = write(&root, "worktree/.agents/config.toml", "");
    write(&root, "worktree/.git", "gitdir: ../fixture-only\n");
    fs::create_dir(root.join("worktree/sub")).unwrap();
    let paths = ConfigPaths::discover_with_env(&root.join("worktree/sub"), home).unwrap();
    assert_eq!(paths.project, Some(project));
    assert_eq!(
        paths.user,
        root.join("worktree/sub/relative-home/config.toml")
    );
}

#[test]
fn discovery_excludes_user_config_at_the_home_git_root() {
    let temp = temp_dir();
    let root = temp.path().canonicalize().unwrap();
    let home = root.join("home");
    fs::create_dir_all(home.join("child")).unwrap();
    fs::create_dir(home.join(".git")).unwrap();
    let user = write(&home, ".tiechui/config.toml", USER);
    write(&root, ".tiechui/config.toml", "[modules]\nediting = false");
    for cwd in [&home, &home.join("child")] {
        let paths = ConfigPaths::discover_with_env(cwd, |key| {
            (key == "HOME").then(|| temp.path().join("home/child/..").into_os_string())
        })
        .unwrap();
        assert_ne!(paths.user, user);
        assert_eq!(paths.user.canonicalize().unwrap(), user);
        assert!(paths.project.is_none());
        assert!(
            load(&paths, false, &toml::Table::new())
                .unwrap()
                .diagnostics
                .is_empty()
        );
    }
}

#[test]
fn discovery_skips_user_config_alias_and_keeps_walking() {
    let temp = temp_dir();
    let root = temp.path().canonicalize().unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("child/alias")).unwrap();
    let project = write(&root, ".agents/config.toml", "[modules]\nediting = false");
    let user = write(&root, "child/.agents/config.toml", USER);
    let paths = ConfigPaths::discover_with_env(&root.join("child"), |key| {
        (key == "TIECHUI_HOME").then(|| temp.path().join("child/alias/../.agents").into_os_string())
    })
    .unwrap();
    assert_ne!(paths.user, user);
    assert_eq!(paths.user.canonicalize().unwrap(), user);
    assert_eq!(paths.project, Some(project));
}

#[test]
fn project_config_prefers_agents_accepts_claude_and_ignores_legacy_directory() {
    let temp = temp_dir();
    let root = temp.path().canonicalize().unwrap();
    fs::create_dir_all(root.join(".git")).unwrap();
    fs::create_dir_all(root.join("child")).unwrap();
    write(&root, "child/.tiechui/config.toml", "invalid legacy config");
    let discover = || {
        ConfigPaths::discover_with_env(&root.join("child"), |_| {
            Some(root.join("profile").into_os_string())
        })
        .unwrap()
    };
    assert!(discover().project.is_none());
    let parent = write(&root, ".agents/config.toml", "");
    assert_eq!(discover().project, Some(parent));
    let claude = write(&root, "child/.claude/config.toml", "");
    assert_eq!(discover().project, Some(claude));
    let agents = write(&root, "child/.agents/config.toml", "");
    assert_eq!(discover().project, Some(agents));
}

#[test]
fn saves_preserve_comments_spacing_and_unrelated_values() {
    let temp = temp_dir();
    let text = "# keep header\nmodel   = 'local/old' # default model\n\n[modules] # keep table\nediting   = true # preference\n\n[unrelated]\nitems = [\n  'one', # first\n  'two',\n]\n";
    let path = write(temp.path(), "config.toml", text);
    save_preference(&path, "modules.editing", false.into()).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        text.replace("editing   = true", "editing   = false")
    );
    save_preference(&path, "model", "local/new".into()).unwrap();
    let updated = fs::read_to_string(&path).unwrap();
    assert!(updated.contains("# default model"));
    assert!(updated.contains("model   = \"local/new\""));
    assert!(updated.contains("[unrelated]\nitems = [\n  'one', # first\n  'two',\n]\n"));
}

#[test]
fn saves_handle_inline_and_quoted_dotted_keys_and_new_files() {
    let temp = temp_dir();
    let text = "# keep\nmodules = { 'future.module' = true, editing=false } # inline\n";
    let path = write(temp.path(), "inline.toml", text);
    save_preference(&path, "modules.'future.module'", false.into()).unwrap();
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        text.replace("'future.module' = true", "'future.module' = false")
    );
    let fresh = temp.path().join("new/config.toml");
    save_preference(&fresh, "modules.editing", true.into()).unwrap();
    let value: toml::Value = toml::from_str(&fs::read_to_string(fresh).unwrap()).unwrap();
    assert_eq!(value["modules"]["editing"].as_bool(), Some(true));
}

#[test]
fn removed_preferences_keep_the_rest_of_the_file() {
    let temp = temp_dir();
    let text =
        "# keep\nmodel = 'local/old'\n\n[network] # proxy\nproxy = 'http://a'\nno_proxy = 'b'\n";
    let path = write(temp.path(), "config.toml", text);
    remove_preference(&path, "network.proxy").unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        text.replace("proxy = 'http://a'\n", "")
    );
    remove_preference(&path, "network.ca_cert").unwrap();
    remove_preference(&path, "sessions.archive_after_days").unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        text.replace("proxy = 'http://a'\n", "")
    );
}

#[test]
fn mcp_server_saves_change_the_edited_keys_and_keep_the_rest() {
    let temp = temp_dir();
    let text = "model = 'local/small'\n\n# github\n[mcp_servers.github]\nurl = 'https://a.invalid/mcp' # remote\nbearer_token_env_var = 'GH_TOKEN'\nenabled = false\n";
    let path = write(temp.path(), "config.toml", text);
    let http = McpServerConfig {
        url: Some("https://b.invalid/mcp".into()),
        http_headers: BTreeMap::from([("X-Team".into(), "a b".into())]),
        tool_timeout_sec: Some(30.0),
        ..Default::default()
    };
    save_mcp_server(&path, Some("github"), "gh", &http).unwrap();
    let stdio = McpServerConfig {
        command: Some("npx".into()),
        args: vec!["-y".into(), "a b".into()],
        env: BTreeMap::from([("KEY".into(), "v".into())]),
        enabled: Some(false),
        ..Default::default()
    };
    save_mcp_server(&path, None, "local", &stdio).unwrap();
    assert!(save_mcp_server(&path, None, "gh", &stdio).is_err());
    assert!(save_mcp_server(&path, Some("missing"), "x", &stdio).is_err());
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "model = 'local/small'\n\n# github\n[mcp_servers.gh]\nurl = \"https://b.invalid/mcp\" # remote\nbearer_token_env_var = 'GH_TOKEN'\nhttp_headers = { X-Team = \"a b\" }\ntool_timeout_sec = 30.0\n\n[mcp_servers.local]\ncommand = \"npx\"\nargs = [\"-y\", \"a b\"]\nenv = { KEY = \"v\" }\nenabled = false\n"
    );
    let inline = write(
        temp.path(),
        "inline.toml",
        "mcp_servers = { a = { command = 'a' } }\n",
    );
    save_mcp_server(&inline, None, "b", &stdio).unwrap();
    let inline = ConfigPaths {
        user: inline,
        project: None,
    };
    let servers = load(&inline, false, &toml::Table::new())
        .unwrap()
        .config
        .mcp_servers;
    assert_eq!((servers.len(), &servers["b"]), (2, &stdio));
    let paths = ConfigPaths {
        user: path.clone(),
        project: None,
    };
    let servers = load(&paths, false, &toml::Table::new())
        .unwrap()
        .config
        .mcp_servers;
    assert_eq!(
        servers["gh"],
        McpServerConfig {
            bearer_token_env_var: Some("GH_TOKEN".into()),
            ..http
        }
    );
    assert_eq!(servers["local"], stdio);
    remove_mcp_server(&path, "gh").unwrap();
    assert!(remove_mcp_server(&path, "gh").is_err());
    assert_eq!(
        load(&paths, false, &toml::Table::new())
            .unwrap()
            .config
            .mcp_servers
            .keys()
            .collect::<Vec<_>>(),
        ["local"]
    );
}

#[test]
fn added_providers_append_to_the_file_without_replacing_one() {
    let temp = temp_dir();
    let path = temp.path().join("providers.toml");
    let table = "[other]\nwire = \"anthropic-messages\"\n";
    add_provider(&path, "local", PROVIDERS.trim_start()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    add_provider(&path, "other", table).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{}\n{table}", PROVIDERS.trim_start())
    );
    assert!(add_provider(&path, "local", PROVIDERS).is_err());
    assert_eq!(load_providers(&path).unwrap().len(), 2);
}

#[test]
fn malformed_saves_and_scalar_path_conflicts_do_not_change_files() {
    let temp = temp_dir();
    for text in ["broken = [", "modules = false\n"] {
        let path = write(temp.path(), "config.toml", text);
        assert!(save_preference(&path, "modules.editing", true.into()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), text);
    }
}

#[test]
fn saves_preserve_crlf_and_mixed_line_endings() {
    let temp = temp_dir();
    for text in [
        "# header\r\n[modules]\r\nediting = true # keep\r\nother = false\r\n",
        "model = 'local/世界'\n[modules]\r\nediting = true # keep\nother = false\r\n",
    ] {
        let path = write(temp.path(), "config.toml", text);
        save_preference(&path, "modules.editing", false.into()).unwrap();
        let expected = text.replace("editing = true", "editing = false");
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
        save_preference(&path, "modules.added", true.into()).unwrap();
        let updated = fs::read_to_string(path).unwrap();
        assert!(updated.starts_with(&expected));
        assert!(updated.ends_with("added = true\r\n"));
    }
}

#[cfg(unix)]
#[test]
fn saves_preserve_symlinks_and_target_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = temp_dir();
    let target = write(temp.path(), "actual.toml", "model = 'local/old'\n");
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    let link = temp.path().join("config.toml");
    symlink(&target, &link).unwrap();
    save_preference(&link, "model", "local/new".into()).unwrap();
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o640
    );
    assert!(fs::read_to_string(target).unwrap().contains("local/new"));
}
#[test]
fn mcp_blocks_keep_origins_accept_unknown_keys_and_require_project_trust() {
    let root = tempfile::tempdir().unwrap();
    let paths = ConfigPaths {
        user: root.path().join("user.toml"),
        project: Some(root.path().join("project.toml")),
    };
    std::fs::write(
        &paths.user,
        r#"
[mcp_servers.docs]
command = "fixture"
args = ["--stdio"]
env = { EXPLICIT = "fixture" }
env_vars = ["FORWARD"]
cwd = "."
startup_timeout_sec = 0.5
tool_timeout_sec = 2
enabled = false
enabled_tools = ["one", "two"]
disabled_tools = ["two"]
future_option = { anything = true }
[mcp_servers.http]
url = "http://127.0.0.1/mcp"
http_headers = { X-Fixture = "fixture" }
bearer_token_env_var = "FIXTURE_TOKEN"
"#,
    )
    .unwrap();
    std::fs::write(
        paths.project.as_ref().unwrap(),
        "[mcp_servers.project]\ncommand = 'project-fixture'\n",
    )
    .unwrap();
    let untrusted = load(&paths, false, &toml::Table::new()).unwrap();
    assert_eq!(untrusted.config.mcp_servers.len(), 2);
    assert_eq!(untrusted.config.mcp_origins["docs"], paths.user);
    assert_eq!(
        untrusted.config.mcp_servers["docs"].startup_timeout_sec,
        Some(0.5)
    );
    assert!(
        untrusted
            .diagnostics
            .iter()
            .any(|d| d.key == "mcp_servers.docs.future_option"
                && d.reason == IgnoredReason::UnknownMcpKey)
    );
    let trusted = load(&paths, true, &toml::Table::new()).unwrap();
    assert_eq!(trusted.config.mcp_servers.len(), 3);
    assert_eq!(
        &trusted.config.mcp_origins["project"],
        paths.project.as_ref().unwrap()
    );
}

#[test]
fn copy_on_select_defaults_merges_and_reloads() {
    assert!(TuiConfig::default().copy_on_select);
    let temp = temp_dir();
    let paths = paths(temp.path(), "", "[tui]\ncopy_on_select = false\n");
    for (source, expected) in [
        ("", true),
        ("[tui]\ncopy_on_select = true\n", true),
        ("[tui]\ncopy_on_select = false\n", false),
    ] {
        fs::write(&paths.user, source).unwrap();
        assert_eq!(
            load(&paths, false, &toml::Table::new())
                .unwrap()
                .config
                .tui
                .copy_on_select,
            expected
        );
        assert!(
            !load(&paths, true, &toml::Table::new())
                .unwrap()
                .config
                .tui
                .copy_on_select
        );
        let cli = toml::from_str("[tui]\ncopy_on_select = true\n").unwrap();
        assert!(load(&paths, true, &cli).unwrap().config.tui.copy_on_select);
        let direct: Config = toml::from_str(source).unwrap();
        assert_eq!(direct.tui.copy_on_select, expected);
    }
}

#[test]
fn status_line_fields_merge_individually_over_defaults() {
    let temp = temp_dir();
    let paths = paths(
        temp.path(),
        "[tui.status_line]\nitems = ['cache-hit-rate', 'model']\nicons = ['model']\n",
        "[tui.status_line]\nuse_theme_colors = false\n",
    );
    let line = load(&paths, true, &toml::Table::new())
        .unwrap()
        .config
        .tui
        .status_line;
    assert_eq!(
        line,
        StatusLineConfig {
            items: vec![StatusLineItem::CacheHitRate, StatusLineItem::Model],
            use_theme_colors: false,
            icons: vec![StatusLineItem::Model],
        }
    );
    assert_eq!(
        StatusLineConfig::default().items,
        [
            StatusLineItem::ModelWithReasoning,
            StatusLineItem::CurrentDir,
            StatusLineItem::GitBranch,
            StatusLineItem::PermissionMode,
            StatusLineItem::ContextRemaining,
            StatusLineItem::ContextWindowSize,
        ]
    );
    for item in StatusLineItem::ALL {
        assert_eq!(
            toml::Value::try_from(item).unwrap().as_str(),
            Some(item.id())
        );
    }
    fs::write(&paths.user, "[tui.status_line]\nitems = ['nope']\n").unwrap();
    assert!(load(&paths, false, &toml::Table::new()).is_err());
}

#[test]
fn syntax_theme_saves_with_comments_and_reloads_through_layers() {
    let temp = temp_dir();
    let paths = paths(
        temp.path(),
        "# keep this comment\n[tui]\ntheme = 'automatic' # preference\n",
        "[tui]\ntheme = 'project'\n",
    );
    save_preference(&paths.user, "tui.theme", "Catppuccin Latte".into()).unwrap();
    let saved = fs::read_to_string(&paths.user).unwrap();
    assert!(saved.starts_with("# keep this comment\n"));
    assert!(saved.contains("# preference"));
    assert_eq!(
        load(&paths, false, &toml::Table::new())
            .unwrap()
            .config
            .tui
            .theme
            .as_deref(),
        Some("Catppuccin Latte")
    );
    assert_eq!(
        load(&paths, true, &toml::Table::new())
            .unwrap()
            .config
            .tui
            .theme
            .as_deref(),
        Some("project")
    );
}

#[test]
fn range_compaction_defaults_validation_and_reload() {
    let temp = temp_dir();
    let paths = paths(temp.path(), "", "");
    let partial=toml::from_str("[compaction]\npreserve_recent_messages=0\npreserve_recent_tokens=0\nprotected_tools=['read*']\n[compaction.output]\nenabled=false\nmin_chars=25000\n[compaction.search]\ndefault_limit=2\n").unwrap();
    let config = load(&paths, false, &partial).unwrap().config.compaction;
    assert_eq!(config.preserve_recent_messages, 0);
    assert_eq!(config.preserve_recent_tokens, 0);
    assert_eq!(config.output.min_chars, 25000);
    assert!(!config.output.enabled);
    assert_eq!(config.search.default_limit, 2);
    assert_eq!(config.search.max_limit, 30);
    assert_eq!(config.nudge.growth_ratio, 1.0 / 3.0);
    assert!(config.enabled);
    assert_eq!(config.reserve_tokens, 16384);
    for invalid in [
        "[compaction]\nunknown=1",
        "[compaction]\ncontext_window_cap=0",
        "[compaction]\npreserve_recent_tokens=-1",
        "[compaction]\nprotected_tools=['re*ad']",
        "[compaction.nudge]\ngrowth_ratio=0.0",
        "[compaction.nudge]\nmax_context_limit_pct=nan",
        "[compaction.nudge]\nemergency_threshold_pct=1.1",
        "[compaction.nudge]\nmin_context_limit_pct=0.9\nmax_context_limit_pct=0.8",
        "[compaction.nudge]\nmin_pressure_benefit_tokens='other'",
        "[compaction.output]\nmin_chars=1999",
        "[compaction.compress]\nmin_summary_length=100\nmax_summary_length=50",
        "[compaction.search]\ndefault_limit=31",
        "[compaction.tiers]\ntier2_trigger=0",
        "[compaction.decompress]\nfull=1",
        "[compaction]\ncontext_window_cap=9007199254740992",
    ] {
        let table = toml::from_str(&format!("[modules]\ncompaction=false\n{invalid}\n")).unwrap();
        assert!(load(&paths, false, &table).is_err(), "{invalid}");
    }
    fs::write(&paths.user,"# keep\n[compaction]\npreserve_recent_messages=2\n[compaction.compress]\nmin_net_savings_tokens=123\n").unwrap();
    let first = load(&paths, false, &toml::Table::new()).unwrap();
    assert_eq!(first.config.compaction.compress.min_net_savings_tokens, 123);
    save_preference(&paths.user, "compaction.preserve_recent_messages", 0.into()).unwrap();
    assert!(
        fs::read_to_string(&paths.user)
            .unwrap()
            .starts_with("# keep")
    );
    assert_eq!(
        load(&paths, false, &toml::Table::new())
            .unwrap()
            .config
            .compaction
            .preserve_recent_messages,
        0
    );
}

#[test]
fn compaction_keys_are_only_snake_case() {
    fn check(table: &toml::Table) {
        for (key, value) in table {
            assert!(key.chars().all(|c| !c.is_uppercase()), "{key}");
            if let Some(nested) = value.as_table() {
                check(nested);
            }
        }
    }
    let defaults = toml::Value::try_from(CompactionConfig::default()).unwrap();
    check(defaults.as_table().unwrap());
    for section in [
        None,
        Some("nudge"),
        Some("tiers"),
        Some("compress"),
        Some("search"),
        Some("output"),
    ] {
        let table = match section {
            Some(section) => defaults[section].as_table().unwrap(),
            None => defaults.as_table().unwrap(),
        };
        for (key, value) in table.iter().filter(|(key, _)| key.contains('_')) {
            let mut parts = key.split('_');
            let mut legacy_key = parts.next().unwrap().to_owned();
            for part in parts {
                let mut chars = part.chars();
                legacy_key.extend(chars.next().unwrap().to_uppercase());
                legacy_key.extend(chars);
            }
            let mut invalid = defaults.clone();
            let target = match section {
                Some(section) => invalid[section].as_table_mut().unwrap(),
                None => invalid.as_table_mut().unwrap(),
            };
            target.remove(key);
            target.insert(legacy_key.clone(), value.clone());
            assert!(
                invalid.try_into::<CompactionConfig>().is_err(),
                "{legacy_key}"
            );
        }
    }
}

#[test]
fn provider_and_model_edits_keep_other_keys_and_reject_invalid_results() {
    let temp = temp_dir();
    let text = "# providers\n[local]\nwire = \"openai-completions\" # wire\napi_key_env = \"FIXTURE_API_KEY\"\nmodels = [{ id = \"small\", context_window = 32000, max_output = 4000, cache_retention = \"long\" }]\n";
    let path = write(temp.path(), "providers.toml", text);
    let model = |id: &str| ModelConfig {
        id: id.into(),
        cache_retention: Default::default(),
        prices: None,
        fallback_models: Vec::new(),
        context_window: 64000,
        max_output: 8000,
        reasoning: true,
        thinking: Default::default(),
        thinking_levels: Some(BTreeMap::from([
            (
                ThinkingLevel::Minimal,
                ThinkingLevelValue::Unsupported(UnsupportedLevel),
            ),
            (
                ThinkingLevel::High,
                ThinkingLevelValue::Wire("max-effort".into()),
            ),
        ])),
        compat: None,
        images: true,
    };
    save_model(&path, "local", Some("small"), &model("small")).unwrap();
    save_model(&path, "local", None, &model("large")).unwrap();
    assert!(save_model(&path, "local", None, &model("large")).is_err());
    assert!(save_model(&path, "missing", None, &model("x")).is_err());
    let saved = fs::read_to_string(&path).unwrap();
    assert!(saved.starts_with("# providers\n[local]\nwire = \"openai-completions\" # wire\n"));
    assert!(saved.contains("[[local.models]]\nid = \"small\""));
    assert!(saved.contains("thinking_levels = { minimal = false, high = \"max-effort\" }"));
    let local = &load_providers(&path).unwrap()["local"];
    assert_eq!(local.models[0].cache_retention, CacheRetention::Long);
    assert_eq!(local.models[1].context_window, 64000);

    save_provider(
        &path,
        "local",
        "anthropic-messages",
        Some("https://a.invalid"),
        Some("key"),
    )
    .unwrap();
    let local = &load_providers(&path).unwrap()["local"];
    assert_eq!(
        (local.wire, local.api_key_env.as_deref()),
        (Wire::AnthropicMessages, None)
    );
    assert!(save_provider(&path, "local", "unknown", None, None).is_err());
    assert!(save_provider(&path, "a/b", "anthropic-messages", None, None).is_err());
    save_provider(&path, "other", "openai-completions", None, None).unwrap();
    remove_model(&path, "local", "small").unwrap();
    remove_provider(&path, "other").unwrap();
    let providers = load_providers(&path).unwrap();
    assert_eq!(providers.keys().collect::<Vec<_>>(), ["local"]);
    assert_eq!(providers["local"].models.len(), 1);
    assert!(remove_provider(&path, "other").is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn proxy_schemes_normalize_and_reject_invalid_urls() {
    let temp = temp_dir();
    let paths = ConfigPaths {
        user: temp.path().join("config.toml"),
        project: None,
    };

    for (input, expected) in [
        ("http://127.0.0.1:8080", "http://127.0.0.1:8080"),
        (
            "https://u:p@proxy.example.com:8443",
            "https://u:p@proxy.example.com:8443",
        ),
        ("socks5://127.0.0.1:1080", "socks5://127.0.0.1:1080"),
        (
            "socks5://u:s3cr3t@127.0.0.1:1080",
            "socks5://u:s3cr3t@127.0.0.1:1080",
        ),
        ("127.0.0.1:7890", "http://127.0.0.1:7890"),
        ("proxy.example.com:8080", "http://proxy.example.com:8080"),
        ("[::1]:1080", "http://[::1]:1080"),
    ] {
        fs::write(&paths.user, format!("[network]\nproxy = '{input}'\n")).unwrap();
        let loaded = load(&paths, false, &toml::Table::new()).unwrap();
        assert_eq!(
            loaded.config.network.proxy.as_deref(),
            Some(expected),
            "{input}"
        );
    }

    for invalid in [
        "ftp://127.0.0.1:21",
        "socks4://127.0.0.1:1080",
        "http://:8080",
        ":8080",
        "127.0.0.1:0",
        "http://127.0.0.1:0",
        "127.0.0.1:99999",
        "localhost",
        "u:leak-me-not@:99999",
    ] {
        fs::write(&paths.user, format!("[network]\nproxy = '{invalid}'\n")).unwrap();
        let err = load(&paths, false, &toml::Table::new()).unwrap_err();
        assert_eq!(err.key, "network", "{invalid}");
        assert!(
            !err.to_string().contains("leak-me-not"),
            "error leaked password: {err}"
        );
    }
}

#[test]
fn proxy_credentials_redact_debug_and_secure_permissions() {
    let temp = temp_dir();
    let paths = ConfigPaths {
        user: temp.path().join("config.toml"),
        project: None,
    };
    fs::write(
        &paths.user,
        "[network]\nproxy = 'socks5://alice:super-secret-pass@127.0.0.1:1080'\n",
    )
    .unwrap();
    let loaded = load(&paths, false, &toml::Table::new()).unwrap();
    let debug_text = format!("{:?}", loaded.config.network);
    assert!(
        !debug_text.contains("super-secret-pass"),
        "Debug leaked inline proxy password: {debug_text}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(&paths.user, "model = 'local/small'\n").unwrap();
        fs::set_permissions(&paths.user, fs::Permissions::from_mode(0o644)).unwrap();
        save_preference(&paths.user, "network.no_proxy", "localhost".into()).unwrap();
        assert_eq!(
            fs::metadata(&paths.user).unwrap().permissions().mode() & 0o777,
            0o644
        );
        save_preference(
            &paths.user,
            "network.proxy",
            "socks5://u:s3cr3t@127.0.0.1:1080".into(),
        )
        .unwrap();
        assert_eq!(
            fs::metadata(&paths.user).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn redact_replaces_percent_encoded_and_decoded_passwords() {
    let network = NetworkConfig {
        proxy: Some("http://u:p%40ss%3A1@127.0.0.1:8080".into()),
        no_proxy: None,
        ca_cert: None,
    };
    assert_eq!(
        network.redact(
            "proxy auth failed for p@ss:1 (encoded p%40ss%3A1) via http://u:p%40ss%3A1@127.0.0.1:8080"
        ),
        "proxy auth failed for <redacted> (encoded <redacted>) via http://u:<redacted>@127.0.0.1:8080"
    );

    let utf8_network = NetworkConfig {
        proxy: Some("socks5://u:p%C3%A4ss@127.0.0.1:1080".into()),
        no_proxy: None,
        ca_cert: None,
    };
    assert_eq!(
        utf8_network.redact("rejected päss and p%C3%A4ss"),
        "rejected <redacted> and <redacted>"
    );

    let nested = NetworkConfig {
        proxy: Some("http://u:%2525@127.0.0.1:8080".into()),
        no_proxy: None,
        ca_cert: None,
    };
    assert_eq!(
        nested.redact("raw %2525 decoded %25"),
        "raw <redacted> decoded <redacted>"
    );
}

use crate::test_support::TEST_CA_PEM;

fn assert_tilde_ca_cert_cases(home: &Path) {
    let ca_only = |p: &str| NetworkConfig {
        ca_cert: Some(PathBuf::from(p)),
        ..Default::default()
    };
    for (configured, expected) in [
        ("~/certs/corp-ca.pem", home.join("certs/corp-ca.pem")),
        ("~\\root-ca.pem", home.join("root-ca.pem")),
    ] {
        let network = ca_only(configured);
        let env = network.subprocess_env();
        let expected_str = expected.display().to_string();
        for key in [
            "SSL_CERT_FILE",
            "NODE_EXTRA_CA_CERTS",
            "CURL_CA_BUNDLE",
            "REQUESTS_CA_BUNDLE",
            "GIT_SSL_CAINFO",
        ] {
            assert_eq!(env.get(key), Some(&expected_str), "{configured} {key}");
        }
        network
            .apply(reqwest::Client::builder())
            .unwrap()
            .build()
            .unwrap();
    }

    let bare_tilde = ca_only("~");
    let home_str = home.display().to_string();
    assert_eq!(
        bare_tilde.subprocess_env().get("SSL_CERT_FILE"),
        Some(&home_str)
    );
    let err = bare_tilde.apply(reqwest::Client::builder()).unwrap_err();
    assert!(err.contains(&home_str), "{err}");
    assert_eq!(
        ca_only("~other/ca.pem")
            .subprocess_env()
            .get("SSL_CERT_FILE")
            .map(String::as_str),
        Some("~other/ca.pem")
    );
}

#[test]
fn ca_cert_expands_leading_tilde_in_apply_and_subprocess_env() {
    const CHILD_MARKER: &str = "TIECHUI_TEST_TILDE_HOME";
    if std::env::var_os(CHILD_MARKER).is_none() {
        let fake_home = temp_dir();
        write(fake_home.path(), "certs/corp-ca.pem", TEST_CA_PEM);
        write(fake_home.path(), "root-ca.pem", TEST_CA_PEM);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::ca_cert_expands_leading_tilde_in_apply_and_subprocess_env",
                "--nocapture",
            ])
            .env(CHILD_MARKER, "1")
            .env("HOME", fake_home.path())
            .env("USERPROFILE", fake_home.path())
            .output()
            .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    assert_tilde_ca_cert_cases(&user_home().unwrap());
}

#[test]
fn no_proxy_normalizes_semicolons_and_wildcard_domains_in_proxy_and_subprocess_env() {
    let raw_rules = "localhost; *.corp.internal, *.api.local:8443  10.0.0.0/8";
    let expected_joined = "localhost,.corp.internal,.api.local:8443,10.0.0.0/8";
    let network = NetworkConfig {
        proxy: Some("http://127.0.0.1:8080".into()),
        no_proxy: Some(raw_rules.into()),
        ca_cert: None,
    };
    let env = network.subprocess_env();
    for key in ["NO_PROXY", "no_proxy"] {
        assert_eq!(env.get(key).map(String::as_str), Some(expected_joined));
    }

    let proxy_with = |rules: Option<&str>| {
        reqwest::Proxy::all("http://127.0.0.1:8080")
            .unwrap()
            .no_proxy(rules.and_then(reqwest::NoProxy::from_string))
    };
    let actual_proxy = network.proxy().unwrap().unwrap();
    let expected_proxy = proxy_with(Some(expected_joined));
    assert_ne!(
        format!("{expected_proxy:?}"),
        format!("{:?}", proxy_with(Some(raw_rules)))
    );
    assert_eq!(format!("{actual_proxy:?}"), format!("{expected_proxy:?}"));

    let blank_delims = NetworkConfig {
        proxy: Some("http://127.0.0.1:8080".into()),
        no_proxy: Some(" ; ,  ".into()),
        ca_cert: None,
    };
    assert_eq!(
        blank_delims
            .subprocess_env()
            .get("NO_PROXY")
            .map(String::as_str),
        Some("")
    );
    assert_eq!(
        format!("{:?}", blank_delims.proxy().unwrap().unwrap()),
        format!("{:?}", proxy_with(None))
    );
}
