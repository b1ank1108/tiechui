#![allow(clippy::unwrap_used)]
#![cfg(feature = "test-server")]

use std::fs;
use std::path::{Path, PathBuf};

use tiechui_module_hooks::{Event, HooksModule, trust_with_sources};
use tiechui_module_mcp::{DeclineElicitation, McpModule, ServerState};
use tiechui_module_plugins::{add, content_hash, discover, remove};
use tiechui_module_skills::SkillsModule;
use tiechui_plugin_api::{CancellationToken, Module, ModuleConfig, serde_json::json};

fn temp() -> tempfile::TempDir {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-tests");
    fs::create_dir_all(&directory).unwrap();
    tempfile::tempdir_in(directory).unwrap()
}

fn fixture(layout: &str, destination: &Path) -> PathBuf {
    fn copy(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &target);
            } else {
                let text = fs::read_to_string(entry.path()).unwrap().replace(
                    "\"FIXTURE_SERVER\"",
                    &json!(env!("CARGO_BIN_EXE_plugin-test-server")).to_string(),
                );
                fs::write(target, text).unwrap();
            }
        }
    }
    let root = destination.join(layout);
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(layout),
        &root,
    );
    if layout == "agent" {
        fs::copy(
            env!("CARGO_BIN_EXE_plugin-test-server"),
            root.join(format!("server{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
    }
    root
}

#[tokio::test]
async fn all_layouts_contribute_skills_live_mcp_and_approved_hooks() {
    for layout in ["tiechui", "codex", "agent"] {
        let temp = temp();
        let home = temp.path().join("home");
        let work = temp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let source = fixture(layout, temp.path());
        let cancel = CancellationToken::new();
        let name = add(&source, &home, &cancel).unwrap();
        assert!(add(&source, &home, &cancel).is_err());
        let config = ModuleConfig::default();
        let plugins = discover(&work, &home, false, &config, &cancel).unwrap();
        assert!(plugins.diagnostics.is_empty(), "{:?}", plugins.diagnostics);
        let plugin = &plugins.plugins[0];
        assert_eq!(plugin.name, name);
        assert!(plugin.enabled);
        assert_eq!(
            plugin.installation.as_ref().unwrap().source,
            source.canonicalize().unwrap()
        );
        assert_eq!(
            plugin.installation.as_ref().unwrap().content_hash,
            content_hash(&plugin.root, &cancel).unwrap()
        );
        assert_eq!(plugin.diagnostics.len(), usize::from(layout == "tiechui"));
        let mut registry = plugins.registry(&config).unwrap();
        let directories = &registry.plugin_directories()[0];
        assert_eq!(directories.prompts.len(), 1);
        assert_eq!(directories.agents.len(), 1);
        let skills =
            SkillsModule::load(&work, &home, &home, false, registry.skill_roots(), &cancel)
                .unwrap();
        assert_eq!(skills.skills.len(), 1);
        assert_eq!(skills.skills[0].root.source, "plugin");
        let mcp = McpModule::from_config(
            &config,
            registry.mcp_sources(),
            &work,
            None,
            false,
            cancel.clone(),
            std::sync::Arc::new(DeclineElicitation),
            None,
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            mcp.wait_for_startup(cancel.clone()),
        )
        .await
        .unwrap();
        let statuses = mcp.statuses();
        assert_eq!(statuses[0].state, ServerState::Ready, "{statuses:?}");
        assert!(statuses[0].origin.starts_with("plugin:"));
        assert!(
            statuses[0]
                .tools
                .iter()
                .any(|name| name == "mcp__fixture__echo")
        );
        mcp.register(&mut registry, &config).unwrap();
        skills.register(&mut registry, &config).unwrap();
        let resources = registry.resources();
        let plugin_resource = resources.iter().find(|r| r.name == name).unwrap();
        assert!(plugin_resource.reference.contains("mcp__fixture__echo"));
        let skill = resources
            .iter()
            .find(|r| r.kind == tiechui_plugin_api::protocol::ResourceKind::Skill)
            .unwrap();
        assert_eq!(skill.path.as_deref(), skills.skills[0].path.to_str());
        assert_eq!(skill.owner, plugin_resource.owner);
        assert!(plugin_resource.reference.contains(&skill.name));
        registry.restrict_tools(vec![]);
        assert!(
            !registry
                .resources()
                .iter()
                .any(|r| r.reference.contains("mcp__fixture__echo"))
        );
        let load_hooks = || {
            HooksModule::load_with_sources(
                &work,
                &home,
                false,
                Default::default(),
                registry.hook_sources(),
                &cancel,
            )
            .unwrap()
        };
        let unapproved = load_hooks();
        assert_eq!(unapproved.hooks()[0].trust, "unapproved");
        assert!(!unapproved.hooks()[0].enabled);
        assert_eq!(
            trust_with_sources(&work, &home, false, registry.hook_sources(), &cancel).unwrap(),
            1
        );
        let approved = load_hooks();
        assert!(approved.hooks()[0].enabled);
        let output = approved
            .dispatch(
                Event::UserPromptSubmit,
                json!({}),
                None,
                &work,
                cancel.clone(),
            )
            .await
            .unwrap();
        assert_eq!(output.context, ["plugin context"]);
        fs::write(plugin.root.join("hook.sh"), "exit 99\n").unwrap();
        let output = approved
            .dispatch(
                Event::UserPromptSubmit,
                json!({}),
                None,
                &work,
                cancel.clone(),
            )
            .await
            .unwrap();
        assert!(output.context.is_empty());
        assert!(
            output
                .diagnostics
                .iter()
                .any(|d| d.contains("Plugin contents changed"))
        );
        assert_eq!(load_hooks().hooks()[0].trust, "changed");
        let mut disabled = config.clone();
        disabled.modules.insert(name.clone(), false);
        let plugins = discover(&work, &home, false, &disabled, &cancel).unwrap();
        let registry = plugins.registry(&disabled).unwrap();
        assert!(registry.skill_roots().is_empty());
        assert!(registry.mcp_sources().is_empty());
        assert!(registry.hook_sources().is_empty());
        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while mcp
                .statuses()
                .iter()
                .any(|s| matches!(s.state, ServerState::Starting | ServerState::Ready))
            {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        remove(&name, &home, &CancellationToken::new()).unwrap();
        assert!(!home.join("plugins").join(name).exists());
    }
}

#[test]
fn project_plugins_require_trust_even_with_an_explicit_module_override() {
    let temp = temp();
    let home = temp.path().join("home");
    let work = temp.path().join("work");
    let directory = work.join(".agents/plugins");
    fixture("codex", &directory);
    let cancel = CancellationToken::new();
    let mut config = ModuleConfig::default();
    config.modules.insert("codex-fixture".into(), true);
    let plugins = discover(&work, &home, false, &config, &cancel).unwrap();
    assert!(!plugins.plugins[0].enabled);
    assert!(plugins.registry(&config).unwrap().skill_roots().is_empty());
    let plugins = discover(&work, &home, true, &config, &cancel).unwrap();
    assert_eq!(plugins.registry(&config).unwrap().skill_roots().len(), 1);
    assert!(plugins.plugins[0].project);
}

#[test]
fn rejects_escapes_symlinks_invalid_schemas_duplicates_and_cancelled_operations() {
    let temp = temp();
    let source = fixture("codex", temp.path());
    let home = temp.path().join("home");
    let cancel = CancellationToken::new();
    assert!(remove("../outside", &home, &cancel).is_err());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc", source.join("escape")).unwrap();
        assert!(add(&source, &home, &cancel).is_err());
        fs::remove_file(source.join("escape")).unwrap();
    }
    let manifest = source.join(".codex-plugin/plugin.json");
    fs::write(&manifest, r#"{"name":"paths","skills":["./../escape","/absolute"],"mcpServers":42,"hooks":"./../hooks"}"#).unwrap();
    add(&source, &home, &cancel).unwrap();
    let plugins = discover(temp.path(), &home, true, &ModuleConfig::default(), &cancel).unwrap();
    assert!(plugins.plugins[0].diagnostics.len() >= 4);
    fs::create_dir_all(home.join("plugins/duplicate/.codex-plugin")).unwrap();
    fs::copy(
        &manifest,
        home.join("plugins/duplicate/.codex-plugin/plugin.json"),
    )
    .unwrap();
    let plugins = discover(temp.path(), &home, true, &ModuleConfig::default(), &cancel).unwrap();
    assert_eq!(plugins.plugins.len(), 1);
    assert!(
        plugins
            .diagnostics
            .iter()
            .any(|d| d.contains("duplicate plugin"))
    );
    fs::write(source.join("plugin.json"), r#"{"$schema":"https://agent-plugins.org/schemas/9/plugin.schema.json","name":"unsupported"}"#).unwrap();
    assert!(add(&source, &home, &cancel).is_err());
    cancel.cancel();
    assert!(discover(temp.path(), &home, true, &ModuleConfig::default(), &cancel).is_err());
    assert!(content_hash(&source, &cancel).is_err());
    assert!(add(&source, &home, &cancel).is_err());
    assert!(remove("paths", &home, &cancel).is_err());
}

#[test]
fn legacy_manifest_uses_directory_name_and_ignores_unrelated_root_manifest() {
    let temp = temp();
    let source = fixture("codex", temp.path());
    fs::write(source.join(".codex-plugin/plugin.json"), "{}").unwrap();
    fs::write(source.join("plugin.json"), "unrelated invalid json").unwrap();
    let home = temp.path().join("home");
    let cancel = CancellationToken::new();
    assert_eq!(add(&source, &home, &cancel).unwrap(), "codex");
    let plugins = discover(temp.path(), &home, false, &ModuleConfig::default(), &cancel).unwrap();
    assert_eq!(plugins.plugins[0].name, "codex");
    let initial = content_hash(&plugins.plugins[0].root, &cancel).unwrap();
    fs::create_dir(plugins.plugins[0].root.join("__pycache__")).unwrap();
    fs::write(
        plugins.plugins[0].root.join("__pycache__/hook.pyc"),
        "generated bytecode",
    )
    .unwrap();
    assert_eq!(
        content_hash(&plugins.plugins[0].root, &cancel).unwrap(),
        initial
    );
}
