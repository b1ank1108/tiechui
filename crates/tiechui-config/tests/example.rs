#![allow(clippy::unwrap_used)]

use std::path::PathBuf;

use serde_json::Value;
use tiechui_config::{Config, ConfigPaths, load, load_providers, providers_schema, schema};

const EXAMPLE: &str = include_str!("../../../examples/config.toml");
const PROVIDERS: &str = include_str!("../../../examples/providers.toml");

fn temp_dir() -> tempfile::TempDir {
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/config-tests");
    std::fs::create_dir_all(&target).unwrap();
    tempfile::tempdir_in(target).unwrap()
}

fn resolve(table: &toml::Table) -> Config {
    let directory = temp_dir();
    load(
        &ConfigPaths {
            user: directory.path().join("missing.toml"),
            project: None,
        },
        false,
        table,
    )
    .unwrap()
    .config
}

#[test]
fn active_example_settings_preserve_default_behavior() {
    let table = toml::from_str(EXAMPLE).unwrap();
    let mut config = resolve(&table);
    assert!(!config.modules.is_empty());
    assert!(config.modules.values().all(|enabled| *enabled));
    config.modules.clear();
    assert_eq!(config, Config::default());
}

fn expand(example: &str) -> String {
    example
        .lines()
        .map(|line| {
            line.strip_prefix("# ")
                .filter(|content| {
                    content.starts_with('[')
                        || content.split_once(" = ").is_some_and(|(key, _)| {
                            !key.is_empty() && !key.contains(char::is_whitespace)
                        })
                })
                .unwrap_or(line)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn optional_examples_parse_and_document_every_configuration_field() {
    let table: toml::Table = toml::from_str(&expand(EXAMPLE)).unwrap();
    resolve(&table);
    let value = serde_json::to_value(table).unwrap();
    let schema = serde_json::to_value(schema()).unwrap();
    assert_documented(&schema, &schema, &value, &PathBuf::new());
}

#[test]
fn provider_example_is_inactive_and_documents_every_provider_field() {
    let directory = temp_dir();
    let path = directory.path().join("providers.toml");
    std::fs::write(&path, PROVIDERS).unwrap();
    assert!(load_providers(&path).unwrap().is_empty());
    let expanded = expand(PROVIDERS);
    std::fs::write(&path, &expanded).unwrap();
    assert_eq!(load_providers(&path).unwrap().len(), 2);
    let value = serde_json::to_value(toml::from_str::<toml::Table>(&expanded).unwrap()).unwrap();
    let schema = serde_json::to_value(providers_schema()).unwrap();
    assert_documented(&schema, &schema, &value, &PathBuf::new());
}

fn assert_documented(root: &Value, schema: &Value, value: &Value, path: &std::path::Path) {
    if let Some(reference) = schema["$ref"].as_str() {
        assert_documented(root, root.pointer(&reference[1..]).unwrap(), value, path);
    }
    if let Some(properties) = schema["properties"].as_object() {
        for (key, child) in properties {
            let path = path.join(key);
            let value = value.get(key).unwrap_or_else(|| {
                panic!("example is missing configuration field {}", path.display())
            });
            assert_documented(root, child, value, &path);
        }
    }
    if let Some(variants) = schema["anyOf"].as_array() {
        for variant in variants {
            assert_documented(root, variant, value, path);
        }
    }
    if let Some(items) = schema.get("items") {
        for entry in value.as_array().unwrap() {
            assert_documented(root, items, entry, path);
        }
    }
    if let Some(additional) = schema["additionalProperties"].as_object() {
        // Alternative MCP transports document different members of the same server type.
        let combined: serde_json::Map<_, _> = value
            .as_object()
            .unwrap()
            .values()
            .filter_map(Value::as_object)
            .flat_map(|entry| entry.clone())
            .collect();
        if !combined.is_empty() {
            assert_documented(
                root,
                &Value::Object(additional.clone()),
                &Value::Object(combined),
                &path.join("*"),
            );
        }
    }
}
