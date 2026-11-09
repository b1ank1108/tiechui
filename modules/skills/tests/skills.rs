#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tiechui_module_skills::SkillsModule;
use tiechui_plugin_api::model::{Content, Message, Role, ToolCall, ToolResult};
use tiechui_plugin_api::{
    CancellationToken, Compactor, ContextProvider, LifecycleContext, Module, ModuleConfig,
    Registry, RestoreContext, RestoredContext, SkillRoot, TurnInput, serde_json,
};

struct Extra(Vec<PathBuf>);
impl SkillRoot for Extra {
    fn paths(&self) -> Vec<PathBuf> {
        self.0.clone()
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    cwd: PathBuf,
    user: PathBuf,
    home: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/skills-tests");
        fs::create_dir_all(&target).unwrap();
        let temp = tempfile::tempdir_in(target).unwrap();
        let cwd = temp.path().join("project");
        let user = temp.path().join("user");
        let home = user.join(".tiechui");
        for dir in [&cwd, &user, &home] {
            fs::create_dir_all(dir).unwrap();
        }
        Self {
            temp,
            cwd,
            user,
            home,
        }
    }

    fn load(&self, trusted: bool, extra: Vec<PathBuf>) -> SkillsModule {
        let mut registry = Registry::default();
        registry.register_skill_root(Arc::new(Extra(extra)));
        SkillsModule::load(
            &self.cwd,
            &self.user,
            &self.home,
            trusted,
            registry.skill_roots(),
            &CancellationToken::new(),
        )
        .unwrap()
    }
}

fn write(path: &Path, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn skill(path: &Path, name: &str) {
    write(
        path,
        &format!("---\nname: {name}\ndescription: Fixture skill\n---\nBODY NEVER IN CATALOG\n"),
    );
}

#[test]
fn catalog_and_validation_diagnostics_match_golden() {
    let fixture = Fixture::new();
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let loaded = fixture.load(false, vec![path.clone()]);
    let normalized = loaded
        .catalog()
        .replace(path.to_str().unwrap(), "/skills-fixtures");
    assert_eq!(normalized, include_str!("catalog.golden"));
    let actual = serde_json::json!({
        "skills": loaded.skills.iter().map(|skill| serde_json::json!({
            "name": skill.name, "description": skill.description,
            "path": skill.path.to_str().unwrap().replace(path.to_str().unwrap(), "/skills-fixtures"),
            "disable_model_invocation": skill.disable_model_invocation,
        })).collect::<Vec<_>>(),
        "diagnostics": loaded.diagnostics.iter().map(|diag| serde_json::json!({
            "type": diag.kind, "message": diag.message,
            "path": diag.path.to_str().unwrap().replace(path.to_str().unwrap(), "/skills-fixtures"),
        })).collect::<Vec<_>>(),
    });
    assert_eq!(
        actual,
        serde_json::from_str::<serde_json::Value>(include_str!("metadata.golden.json")).unwrap()
    );
}

#[test]
fn default_roots_are_ordered() {
    let fixture = Fixture::new();
    let plugin = fixture.temp.path().join("plugin");
    let roots = [
        fixture.user.join(".agents/skills"),
        fixture.user.join(".claude/skills"),
        fixture.home.join("skills"),
        fixture.cwd.join(".agents/skills"),
        fixture.cwd.join(".claude/skills"),
        plugin.clone(),
    ];
    for (index, root) in roots.iter().enumerate() {
        skill(&root.join("a/SKILL.md"), "duplicate");
        skill(&root.join("b/SKILL.md"), &format!("unique-{index}"));
    }
    skill(
        &fixture.cwd.join(".tiechui/skills/legacy/SKILL.md"),
        "legacy-only",
    );
    let loaded = fixture.load(true, vec![plugin]);
    assert_eq!(
        loaded
            .skills
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        [
            "duplicate",
            "unique-0",
            "unique-1",
            "unique-2",
            "unique-3",
            "unique-4",
            "unique-5"
        ]
    );
    assert_eq!(loaded.skills[0].root.path, roots[0]);
    assert_eq!(loaded.skills[0].root.source, "user");
    let catalog = loaded.catalog();
    let locations: Vec<_> = loaded
        .skills
        .iter()
        .map(|skill| {
            catalog
                .find(&format!("<location>{}</location>", skill.path.display()))
                .unwrap()
        })
        .collect();
    assert!(locations.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(loaded.diagnostics.len(), roots.len() - 1);
    for (diagnostic, loser) in loaded.diagnostics.iter().zip(&roots[1..]) {
        assert_eq!(diagnostic.kind, "collision");
        assert_eq!(diagnostic.message, "name \"duplicate\" collision");
        assert_eq!(diagnostic.path, loser.join("a/SKILL.md"));
        assert_eq!(diagnostic.winner_path, Some(roots[0].join("a/SKILL.md")));
    }
}

#[test]
fn untrusted_project_is_skipped_before_parsing_and_cannot_shadow_user() {
    let fixture = Fixture::new();
    skill(&fixture.cwd.join(".agents/skills/a/SKILL.md"), "shared");
    write(
        &fixture.cwd.join(".tiechui/skills/a/SKILL.md"),
        "---\ndescription: \"unterminated\n---\n",
    );
    skill(&fixture.user.join(".agents/skills/a/SKILL.md"), "shared");
    write(
        &fixture.cwd.join(".claude/skills/bad/SKILL.md"),
        "---\ndescription: \"unterminated\n---\n",
    );
    let loaded = fixture.load(false, vec![]);
    assert_eq!(loaded.skills.len(), 1);
    assert_eq!(loaded.skills[0].root.source, "user");
    assert_eq!(loaded.diagnostics.len(), 2);
    assert!(
        loaded
            .diagnostics
            .iter()
            .all(|d| d.message == "skipping untrusted project skills")
    );
    let loaded = fixture.load(true, vec![]);
    assert_eq!(loaded.skills[0].root.source, "user");
    let collision = loaded
        .diagnostics
        .iter()
        .find(|d| d.kind == "collision")
        .unwrap();
    assert_eq!(
        collision.path,
        fixture.cwd.join(".agents/skills/a/SKILL.md")
    );
    assert_eq!(collision.winner_path, Some(loaded.skills[0].path.clone()));
    assert!(
        loaded
            .diagnostics
            .iter()
            .any(|d| d.message.starts_with("invalid YAML:"))
    );
}

#[test]
fn frontmatter_types_newlines_bom_and_utf16_limits() {
    let fixture = Fixture::new();
    let root = fixture.home.join("skills");
    for (dir, content) in [
        (
            "fallback",
            "\u{feff}---\rname: 42\rdescription: '  Keep spaces  '\r---\rBody".to_owned(),
        ),
        ("missing", "No frontmatter".into()),
        ("wrong-type", "---\ndescription: 42\n---\n".into()),
        ("empty", "---\ndescription: '   '\n---\n".into()),
        (
            "long",
            format!(
                "---\nname: {}\ndescription: '{}'\n---\n",
                "a".repeat(65),
                "😀".repeat(513)
            ),
        ),
        ("broken", "---\nname: \"unterminated\n---\n".into()),
        (
            "duplicate",
            "---\nname: first\nname: second\ndescription: Duplicate names are invalid YAML.\n---\n"
                .into(),
        ),
        (
            "unterminated",
            "---\nname: x\ndescription: ignored without delimiter\n".into(),
        ),
        ("scalar", "---\n42\n---\n".into()),
        ("bom-space", "---\ndescription: '\u{feff}'\n---\n".into()),
    ] {
        write(&root.join(dir).join("SKILL.md"), &content);
    }
    write(&root.join("broken.md"), "---\nname: \"unterminated\n---\n");
    let loaded = fixture.load(true, vec![]);
    assert_eq!(loaded.skills.len(), 2);
    assert_eq!(loaded.skills[0].name, "fallback");
    assert_eq!(loaded.skills[0].description, "  Keep spaces  ");
    let messages: Vec<_> = loaded
        .diagnostics
        .iter()
        .map(|d| d.message.as_str())
        .collect();
    assert!(messages.contains(&"name exceeds 64 characters (65)"));
    assert!(messages.contains(&"description exceeds 1024 characters (1026)"));
    assert_eq!(
        messages
            .iter()
            .filter(|m| **m == "description is required")
            .count(),
        6
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.starts_with("invalid YAML:"))
            .count(),
        2
    );
    let duplicate = loaded
        .diagnostics
        .iter()
        .find(|d| d.path.ends_with("duplicate/SKILL.md"))
        .unwrap();
    assert!(duplicate.message.contains("duplicated key in mapping"));
}

#[test]
fn repairs_unquoted_prose_without_modifying_files_or_valid_yaml_values() {
    let fixture = Fixture::new();
    let root = fixture.home.join("skills");
    let cases = [
        (
            "prose",
            "description: What's included: builds and tests # comment\nargument-hint: <duration: e.g. 7d>\nmetadata:\n  short-description: Deploy to: production\ntags: [next,@supabase/ssr]\ndisable-model-invocation: true\n",
            "What's included: builds and tests",
            true,
        ),
        (
            "literal",
            "description: |-\n  Build for AWS: ECS\n  examples: keep: these\n\n  # literal comment\nargument-hint: <duration: e.g. 7d>\n",
            "Build for AWS: ECS\nexamples: keep: these\n\n# literal comment",
            false,
        ),
        (
            "folded",
            "description: >-\n  Build for AWS: ECS\n  and deploy\nargument-hint: @workspace\n",
            "Build for AWS: ECS and deploy",
            false,
        ),
        (
            "quoted",
            "description: \"Keep # and colon: here\"\nargument-hint: `target`\ndisable-model-invocation: 'true'\n",
            "Keep # and colon: here",
            false,
        ),
        (
            "anchored",
            "summary: &summary '  Keep spaces  '\ndescription: *summary\nargument-hint: {invalid\n",
            "  Keep spaces  ",
            false,
        ),
    ];
    for (name, fields, _, _) in cases {
        write(
            &root.join(name).join("SKILL.md"),
            &format!("---\n{fields}---\nBODY"),
        );
    }
    let loaded = fixture.load(true, vec![]);
    assert!(loaded.diagnostics.is_empty(), "{:?}", loaded.diagnostics);
    assert_eq!(loaded.skills.len(), cases.len());
    for (name, fields, description, manual) in cases {
        let skill = loaded
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .unwrap();
        assert_eq!(skill.description, description);
        assert_eq!(skill.disable_model_invocation, manual);
        assert_eq!(
            fs::read_to_string(&skill.path).unwrap(),
            format!("---\n{fields}---\nBODY")
        );
    }
}

#[test]
fn failed_scalar_repair_reports_the_original_yaml_error() {
    use saphyr::{LoadableYamlNode, YamlOwned};
    let fixture = Fixture::new();
    let yaml = "description: Build for AWS: ECS\nname: first\nname: second\n";
    let original = YamlOwned::load_from_str(yaml).unwrap_err();
    write(
        &fixture.home.join("skills/bad/SKILL.md"),
        &format!("---\n{yaml}---\n"),
    );
    let loaded = fixture.load(true, vec![]);
    assert!(loaded.skills.is_empty());
    assert_eq!(loaded.diagnostics.len(), 1);
    assert_eq!(
        loaded.diagnostics[0].message,
        format!("invalid YAML: {original}")
    );
}

#[test]
fn discovery_honors_root_files_stop_recursion_hidden_and_ignore_rules() {
    let fixture = Fixture::new();
    let root = fixture.home.join("skills");
    skill(&root.join("a.md"), "direct");
    skill(&root.join("nested/no.md"), "not-direct");
    skill(&root.join("nested/deeper/SKILL.md"), "deep");
    skill(&root.join("stop/SKILL.md"), "stop");
    skill(&root.join("stop/deeper/SKILL.md"), "stopped");
    for name in [
        ".hidden",
        "node_modules",
        "git-ignored",
        "ignored",
        "fd-ignored",
        "nested/local",
    ] {
        skill(&root.join(name).join("SKILL.md"), "excluded");
    }
    write(
        &root.join(".gitignore"),
        "git-ignored/\n*.md\n!a.md\n!SKILL.md\n",
    );
    write(&root.join(".ignore"), "ignored/\n");
    write(&root.join(".fdignore"), "fd-ignored/\n");
    write(&root.join("nested/.ignore"), "/local/\n");
    let loaded = fixture.load(true, vec![]);
    assert_eq!(
        loaded
            .skills
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["direct", "deep", "stop"]
    );
    assert!(loaded.diagnostics.is_empty());
    skill(&root.join("SKILL.md"), "whole-root");
    let loaded = fixture.load(true, vec![]);
    assert_eq!(loaded.skills.len(), 1);
    assert_eq!(loaded.skills[0].name, "whole-root");
}

#[cfg(unix)]
#[test]
fn follows_symlinks_deduplicates_real_files_and_stops_cycles() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let root = fixture.home.join("skills");
    let outside = fixture.temp.path().join("linked");
    skill(&outside.join("SKILL.md"), "linked");
    fs::create_dir_all(&root).unwrap();
    symlink(&outside, root.join("a")).unwrap();
    symlink(&outside, root.join("b")).unwrap();
    symlink(&root, root.join("cycle")).unwrap();
    symlink(root.join("absent"), root.join("broken")).unwrap();
    let loaded = fixture.load(true, vec![outside]);
    assert_eq!(loaded.skills.len(), 1);
    assert_eq!(loaded.diagnostics.len(), 1);
    assert_eq!(
        loaded.diagnostics[0].message,
        "skipping skill directory symlink cycle"
    );
}

#[tokio::test]
async fn context_registration_omits_disabled_skills_and_honors_cancellation() {
    let fixture = Fixture::new();
    write(
        &fixture.home.join("skills/a/SKILL.md"),
        "---\ndescription: Hidden\ndisable-model-invocation: true\n---\n",
    );
    let loaded = fixture.load(true, vec![]);
    let mut registry = Registry::default();
    loaded
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let input = TurnInput {
        cwd: fixture.cwd.clone(),
        content: vec![],
    };
    assert!(
        registry.context_providers()[0]
            .fragments(&input, CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(loaded.fragments(&input, cancel.clone()).await.is_err());
    assert!(
        SkillsModule::load(
            &fixture.cwd,
            &fixture.user,
            &fixture.home,
            true,
            &[],
            &cancel
        )
        .is_err()
    );
    skill(&fixture.home.join("skills/b/SKILL.md"), "visible");
    let loaded = fixture.load(true, vec![]);
    assert_eq!(
        loaded
            .fragments(&input, CancellationToken::new())
            .await
            .unwrap(),
        vec![loaded.catalog()]
    );
    assert!(!loaded.catalog().contains("BODY NEVER"));
}

#[tokio::test]
async fn skills_disabled_in_config_are_left_out_of_the_session() {
    let fixture = Fixture::new();
    skill(&fixture.home.join("skills/kept/SKILL.md"), "kept");
    skill(&fixture.home.join("skills/unwanted/SKILL.md"), "unwanted");
    let mut config = ModuleConfig::default();
    config.skills.disabled = vec!["unwanted".into()];
    let mut registry = Registry::default();
    fixture
        .load(true, vec![])
        .register(&mut registry, &config)
        .unwrap();
    let names: Vec<_> = registry.resources().into_iter().map(|r| r.name).collect();
    assert_eq!(names, ["kept"]);
    let input = TurnInput {
        cwd: fixture.cwd.clone(),
        content: vec![tiechui_plugin_api::protocol::InputContent::Text {
            text: "$unwanted".into(),
        }],
    };
    let fragments = registry.context_providers()[0]
        .fragments(&input, CancellationToken::new())
        .await
        .unwrap();
    assert!(fragments.iter().all(|text| !text.contains("unwanted")));
    let listing = registry.commands()[0]
        .run("", CancellationToken::new())
        .await
        .unwrap();
    assert!(listing.contains("kept") && !listing.contains("unwanted"));
}

#[tokio::test]
async fn explicit_skills_load_on_submission_with_exact_source_binding() {
    use tiechui_plugin_api::protocol::InputContent;
    let fixture = Fixture::new();
    let path = fixture.home.join("skills/review/SKILL.md");
    write(
        &path,
        "---\nname: review\ndescription: Review changes\ndisable-model-invocation: true\n---\nMANUAL_BODY\n",
    );
    let loaded = fixture.load(true, vec![]);
    let mut registry = Registry::default();
    loaded
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let command = registry
        .commands()
        .iter()
        .find(|command| command.info().name == "skills")
        .unwrap();
    assert!(command.interactive());
    let listing = command.run("", CancellationToken::new()).await.unwrap();
    assert!(
        listing.contains("review")
            && listing.contains("manual only")
            && listing.contains(path.to_str().unwrap())
    );
    assert!(!listing.contains("MANUAL_BODY"));
    let bound = format!("[$review](<{}>)", path.display());
    for text in ["use $review please", &bound, "`$ignored` then $review"] {
        let input = TurnInput {
            cwd: fixture.cwd.clone(),
            content: vec![InputContent::Text { text: text.into() }],
        };
        let fragments = loaded
            .fragments(&input, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(fragments.len(), 1);
        assert!(fragments[0].contains("MANUAL_BODY"));
    }
    for text in [
        "$unknown",
        "$review-extra",
        r"\$review",
        "`$review`",
        "```rust\n$review\n```",
        "~~~\n$review\n~~~",
        "! echo $review",
        "[$review](</different/SKILL.md>)",
    ] {
        let input = TurnInput {
            cwd: fixture.cwd.clone(),
            content: vec![InputContent::Text { text: text.into() }],
        };
        assert!(
            loaded
                .fragments(&input, CancellationToken::new())
                .await
                .unwrap()
                .is_empty(),
            "{text}"
        );
    }
    let input = TurnInput {
        cwd: fixture.cwd.clone(),
        content: vec![InputContent::Text { text: bound }],
    };
    fs::write(&path, "UPDATED_BODY").unwrap();
    assert!(
        loaded
            .fragments(&input, CancellationToken::new())
            .await
            .unwrap()[0]
            .contains("UPDATED_BODY")
    );
    fs::write(&path, "x".repeat(131073)).unwrap();
    assert!(
        loaded
            .fragments(&input, CancellationToken::new())
            .await
            .unwrap_err()
            .0
            .contains("128 KiB")
    );
    fs::remove_file(&path).unwrap();
    assert!(
        loaded
            .fragments(&input, CancellationToken::new())
            .await
            .unwrap_err()
            .0
            .contains("Cannot load skill")
    );
}

fn text(text: &str) -> Content {
    Content::Text {
        text: text.into(),
        opaque: None,
    }
}

/// A `read` call of `path` and its result.
fn read(id: &str, path: &Path, result: &str, is_error: bool) -> [Message; 2] {
    [
        Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall(ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": path}),
                opaque: None,
            })],
            opaque: None,
        },
        Message {
            role: Role::Tool,
            content: vec![Content::ToolResult(ToolResult {
                call_id: id.into(),
                content: vec![text(result)],
                is_error,
            })],
            opaque: None,
        },
    ]
}

#[tokio::test]
async fn checkpoint_restore_rereads_loaded_skills_latest_first_within_budget() {
    let fixture = Fixture::new();
    for name in ["alpha", "beta", "gamma", "delta"] {
        write(
            &fixture.home.join(format!("skills/{name}/SKILL.md")),
            &format!(
                "---\nname: {name}\ndescription: Fixture skill\n---\n{} BODY\n",
                name.to_uppercase()
            ),
        );
    }
    write(
        &fixture.home.join("skills/long/SKILL.md"),
        &format!(
            "---\nname: long\ndescription: Fixture skill\n---\n{}",
            "LONG ".repeat(2000)
        ),
    );
    let loaded = fixture.load(true, vec![]);
    let path = |name: &str| {
        loaded
            .skills
            .iter()
            .find(|skill| skill.name == name)
            .unwrap()
            .path
            .clone()
    };
    let content = |name: &str| fs::read_to_string(path(name)).unwrap();
    let summarized = [
        read("1", &path("beta"), &content("beta"), false),
        read("2", &path("gamma"), "missing", true),
        read("3", &path("long"), &content("long"), false),
        read("4", &path("alpha"), &content("alpha"), false),
        read("5", &path("beta"), &content("beta"), false),
    ]
    .concat();
    let previous = |source: &str, name: &str| RestoredContext {
        source: source.into(),
        key: path(name).to_string_lossy().into_owned(),
        version: String::new(),
        text: String::new(),
    };
    let mut context = RestoreContext {
        lifecycle: LifecycleContext {
            session_id: "s".into(),
            turn_id: String::new(),
            cwd: fixture.cwd.clone(),
            transcript_path: None,
            model: "m".into(),
        },
        summarized,
        retained: read("6", &path("beta"), &content("beta"), false).into(),
        previous: vec![previous("skills", "delta"), previous("other", "gamma")],
        item_tokens: 200,
        total_tokens: 1000,
        estimate: |text| (text.encode_utf16().count() as u64 * 2).div_ceil(7),
    };
    let items = loaded
        .restore(&context, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        items
            .iter()
            .map(|item| item.key.clone())
            .collect::<Vec<_>>(),
        ["alpha", "long", "delta"].map(|name| path(name).to_string_lossy().into_owned())
    );
    assert!(items.iter().all(|item| item.source == "skills"
        && item.version.starts_with("sha256:")
        && (context.estimate)(&item.text) <= 200));
    assert!(
        items[0]
            .text
            .starts_with("<skill name=\"alpha\" location=\"")
    );
    assert!(items[0].text.contains("ALPHA BODY") && items[0].text.ends_with("</skill>"));
    assert!(items[1].text.contains("LONG LONG") && items[1].text.contains("[Truncated; read "));
    context.retained.clear();
    let items = loaded
        .restore(&context, CancellationToken::new())
        .await
        .unwrap();
    assert!(items.iter().any(|item| item.text.contains("BETA BODY")));
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(loaded.restore(&context, cancel).await.is_err());
}
