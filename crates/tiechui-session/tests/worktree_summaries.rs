#![allow(clippy::unwrap_used)]

use tiechui_model::{Content, Message, ModelId, Role};
use tiechui_session::{EntryKind, ModelRef, SessionStore, summaries_in};
use tokio_util::sync::CancellationToken;

fn msg_entry(role: Role, text: &str, model: Option<ModelRef>) -> EntryKind {
    EntryKind::Message {
        message: Message {
            role,
            content: vec![Content::Text {
                text: text.into(),
                opaque: None,
            }],
            opaque: None,
        },
        usage: None,
        model,
        stop_reason: None,
        reasoning: None,
        details: None,
    }
}

fn seed_session(home: &std::path::Path, cwd: &std::path::Path, prompt: &str) {
    let cancel = CancellationToken::new();
    let mut store = SessionStore::create(home, cwd, &cancel).unwrap();
    store
        .append(msg_entry(Role::User, prompt, None), &cancel)
        .unwrap();
    let model = ModelRef {
        provider: "p".into(),
        model: ModelId("m".into()),
    };
    store
        .append(msg_entry(Role::Assistant, "ok", Some(model)), &cancel)
        .unwrap();
}

#[test]
fn summaries_in_merges_project_and_worktree_dirs_without_duplicates() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    let wt_one = temp.path().join("wt-one");
    let wt_two = temp.path().join("wt-two");
    let unrelated = temp.path().join("unrelated");
    for dir in [&project, &wt_one, &wt_two, &unrelated] {
        std::fs::create_dir_all(dir).unwrap();
    }

    seed_session(&home, &project, "in project root");
    seed_session(&home, &wt_one, "in wt one");
    seed_session(&home, &wt_two, "in wt two");
    seed_session(&home, &unrelated, "in unrelated");

    std::fs::remove_dir_all(&wt_two).unwrap();

    let cancel = CancellationToken::new();
    let canonical_project = project.canonicalize().unwrap();
    let (found, diagnostics) = summaries_in(
        &home,
        &[
            project.clone(),
            canonical_project,
            wt_one.clone(),
            wt_two.clone(),
        ],
        &cancel,
    )
    .unwrap();
    assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
    let mut messages: Vec<_> = found.iter().map(|s| s.first_message.as_str()).collect();
    messages.sort_unstable();
    assert_eq!(messages, ["in project root", "in wt one", "in wt two"]);
}
