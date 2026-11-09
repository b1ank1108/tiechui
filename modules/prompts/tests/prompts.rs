#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::sync::Arc;

use tiechui_module_prompts::{PromptsModule, render};
use tiechui_plugin_api::protocol::CommandInfo;
use tiechui_plugin_api::{
    CancellationToken, Command, Module, ModuleConfig, Registry, ToolError, async_trait,
};

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

#[test]
fn prompts_load_from_user_then_trusted_project_roots() {
    let root = tempfile::tempdir().unwrap();
    let (home, cwd) = (root.path().join("home"), root.path().join("work"));
    write(
        &home.join("prompts/review.md"),
        "---\ndescription: \"Review: the diff\"\nargument-hint: [files] [focus]\n---\n\nReview $ARGUMENTS.\n",
    );
    write(&home.join("prompts/notes.txt"), "not a prompt");
    write(&cwd.join(".agents/prompts/review.md"), "Shadowed");
    write(
        &cwd.join(".claude/commands/commit.md"),
        "\u{feff}Write a commit message.\r\nKeep it short.\r\n",
    );
    write(&cwd.join(".claude/commands/two words.md"), "x");

    let untrusted = PromptsModule::load(&cwd, &home, false);
    assert_eq!(untrusted.prompts.len(), 1);
    assert_eq!(untrusted.warnings.len(), 2);
    assert!(untrusted.warnings[0].ends_with("skipping untrusted project prompts"));

    let module = PromptsModule::load(&cwd, &home, true);
    let review = &module.prompts[0];
    assert_eq!(
        (
            review.name.as_str(),
            review.description.as_str(),
            review.source
        ),
        ("review", "Review: the diff", "user")
    );
    assert_eq!(review.argument_hint.as_deref(), Some("[files] [focus]"));
    assert_eq!(review.body, "Review $ARGUMENTS.");
    let commit = &module.prompts[1];
    assert_eq!(
        (
            commit.name.as_str(),
            commit.description.as_str(),
            commit.source
        ),
        ("commit", "Write a commit message.", "project")
    );
    assert_eq!(commit.body, "Write a commit message.\nKeep it short.");
    assert_eq!(module.prompts.len(), 2);
    assert!(module.warnings[0].ends_with("/review is already defined"));
    assert!(module.warnings[1].ends_with("the file name is not a command name"));
}

#[test]
fn arguments_replace_the_placeholder_or_follow_the_body() {
    let root = tempfile::tempdir().unwrap();
    write(&root.path().join("prompts/a.md"), "Fix $ARGUMENTS now");
    write(&root.path().join("prompts/b.md"), "Explain this");
    let module = PromptsModule::load(root.path(), root.path(), false);
    let [a, b] = &module.prompts[..] else {
        unreachable!()
    };
    assert_eq!(a.expand(" the bug "), "Fix the bug now");
    assert_eq!(b.expand("main.rs"), "Explain this\n\nmain.rs");
    assert_eq!(b.expand(""), "Explain this");
}

#[test]
fn rendered_files_read_back_to_the_same_fields() {
    let root = tempfile::tempdir().unwrap();
    let text = render(
        "Say \"hi\"\nto: $USER",
        Some("[name]"),
        "\nHello $ARGUMENTS\n",
    );
    write(&root.path().join("prompts/greet.md"), &text);
    let module = PromptsModule::load(root.path(), root.path(), false);
    let greet = &module.prompts[0];
    assert_eq!(greet.description, "Say \"hi\" to: $USER");
    assert_eq!(greet.argument_hint.as_deref(), Some("[name]"));
    assert_eq!(greet.body, "Hello $ARGUMENTS");
    assert!(!render("d", Some(""), "b").contains("argument-hint"));
}

struct Builtin;

#[async_trait]
impl Command for Builtin {
    fn info(&self) -> CommandInfo {
        CommandInfo {
            name: "status".into(),
            description: "Built in".into(),
            argument_hint: None,
        }
    }
    async fn run(&self, _: &str, _: CancellationToken) -> Result<String, ToolError> {
        Ok("built in".into())
    }
}

#[tokio::test]
async fn registered_commands_keep_their_names_and_prompts_expand_without_a_session() {
    let root = tempfile::tempdir().unwrap();
    write(&root.path().join("prompts/status.md"), "Shadowed");
    write(
        &root.path().join("prompts/fix.md"),
        "---\nargument-hint: <issue>\n---\nFix $ARGUMENTS",
    );
    let mut registry = Registry::default();
    registry.register_command(Arc::new(Builtin));
    PromptsModule::load(root.path(), root.path(), false)
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let commands = registry.commands();
    let infos: Vec<_> = commands.iter().map(|command| command.info()).collect();
    assert_eq!(infos[0].description, "Built in");
    assert_eq!(
        infos[1],
        CommandInfo {
            name: "fix".into(),
            description: "Fix $ARGUMENTS".into(),
            argument_hint: Some("<issue>".into()),
        }
    );
    assert_eq!(infos.len(), 2);
    let fix = &commands[1];
    assert!(fix.interactive() && fix.allows_busy(""));
    let text = fix.run("#12", CancellationToken::new()).await.unwrap();
    assert_eq!(text, "Fix #12");
}
