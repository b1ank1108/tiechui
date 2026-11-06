#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use tiechui_config::{ConfigPaths, IgnoredReason, load_with_mandatory_modules};
use tiechui_plugin_api::{Module, ModuleConfig, Registry, StaticContext, ToolError};

struct TestModule {
    id: &'static str,
    default: bool,
    mandatory: bool,
}

impl Module for TestModule {
    fn id(&self) -> &'static str {
        self.id
    }
    fn default_enabled(&self) -> bool {
        self.default
    }
    fn mandatory(&self) -> bool {
        self.mandatory
    }
    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        registry.register_context_provider(Arc::new(StaticContext(vec![self.id.into()])));
        Ok(())
    }
}

#[tokio::test]
async fn defaults_and_explicit_overrides_control_registration() {
    let on = TestModule {
        id: "on",
        default: true,
        mandatory: false,
    };
    let off = TestModule {
        id: "off",
        default: false,
        mandatory: false,
    };
    let mut config = ModuleConfig::default();
    assert_eq!(
        Registry::from_modules(&[&on, &off], &config)
            .unwrap()
            .context_providers()
            .len(),
        1
    );
    config.modules.insert("on".into(), false);
    config.modules.insert("off".into(), true);
    let registry = Registry::from_modules(&[&on, &off], &config).unwrap();
    let input = tiechui_plugin_api::TurnInput {
        cwd: "/project".into(),
        content: vec![],
    };
    assert_eq!(
        registry.context_providers()[0]
            .fragments(&input, tiechui_plugin_api::CancellationToken::new())
            .await
            .unwrap(),
        vec!["off"]
    );
    assert_eq!(
        Registry::from_modules(&[&on, &off], &config)
            .unwrap()
            .context_providers()
            .len(),
        1
    );
    assert!(Registry::from_modules(&[&on, &on], &config).is_err());
}

#[test]
fn project_cannot_disable_a_mandatory_module_but_user_and_cli_can() {
    let root = tempfile::tempdir().unwrap();
    let user = root.path().join("user.toml");
    let project = root.path().join("project.toml");
    std::fs::write(&user, "").unwrap();
    std::fs::write(&project, "[modules]\nmandatory = false\noptional = false\n").unwrap();
    let paths = ConfigPaths {
        user: user.clone(),
        project: Some(project),
    };
    let module = TestModule {
        id: "mandatory",
        default: true,
        mandatory: true,
    };
    let optional = TestModule {
        id: "optional",
        default: true,
        mandatory: false,
    };
    let modules: [&dyn Module; 2] = [&module, &optional];
    let mandatory: Vec<_> = modules
        .iter()
        .filter(|module| module.mandatory())
        .map(|module| module.id())
        .collect();
    let loaded =
        load_with_mandatory_modules(&paths, true, &toml::Table::new(), &mandatory).unwrap();
    assert_eq!(loaded.diagnostics.len(), 1);
    assert_eq!(loaded.diagnostics[0].key, "modules.mandatory");
    assert_eq!(loaded.diagnostics[0].reason, IgnoredReason::MandatoryModule);
    assert_eq!(
        Registry::from_modules(&modules, &loaded.config)
            .unwrap()
            .context_providers()
            .len(),
        1
    );
    std::fs::write(user, "[modules]\nmandatory = false\n").unwrap();
    let loaded =
        load_with_mandatory_modules(&paths, true, &toml::Table::new(), &mandatory).unwrap();
    assert!(
        Registry::from_modules(&modules, &loaded.config)
            .unwrap()
            .context_providers()
            .is_empty()
    );
    let cli = toml::from_str("[modules]\nmandatory = true").unwrap();
    let loaded = load_with_mandatory_modules(&paths, true, &cli, &mandatory).unwrap();
    assert_eq!(
        Registry::from_modules(&modules, &loaded.config)
            .unwrap()
            .context_providers()
            .len(),
        1
    );
}
