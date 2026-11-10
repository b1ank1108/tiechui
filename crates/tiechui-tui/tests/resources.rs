mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tiechui_protocol::*;
use tiechui_tui::{
    App, Clipboard, Options,
    composer::Composer,
    information::Information,
    mentions::{self, Index, Mode},
    palette::ColorLevel,
    theme::Theme,
};

fn resource(kind: ResourceKind, name: &str) -> Resource {
    Resource {
        id: name.into(),
        kind,
        name: name.into(),
        description: "A bounded resource description".into(),
        path: None,
        owner: None,
        search_terms: vec!["中文".into()],
        reference: format!("real reference for {name}"),
        source: None,
        manual: false,
    }
}
fn catalog() -> Vec<Resource> {
    vec![
        resource(ResourceKind::Plugin, "Café"),
        resource(ResourceKind::Skill, "review"),
        resource(ResourceKind::Task, "inspect files"),
    ]
}
fn event(event: EventKind) -> Event {
    Event {
        submission_id: None,
        thread_id: ThreadId("t".into()),
        epoch: SessionEpoch(0),
        event,
    }
}
fn app() -> App {
    let mut app = App::new(
        Theme::fixture(ColorLevel::TrueColor, false),
        80,
        Options::default(),
    );
    app.event(event(EventKind::ThreadStarted {
        replay: false,
        thread: Thread {
            id: ThreadId("t".into()),
            epoch: SessionEpoch(0),
            turns: vec![],
        },
    }));
    app.event(event(EventKind::ResourcesUpdated {
        resources: catalog(),
    }));
    app.take_submissions();
    app
}
fn key(app: &mut App, key: KeyCode) {
    app.key(KeyEvent::new(key, KeyModifiers::NONE));
}
/// The bold half of the `Files  All` filter.
fn filter(app: &mut App) -> Option<String> {
    app.live()
        .0
        .iter()
        .flat_map(|l| l.spans.iter())
        .find(|s| {
            matches!(s.content.as_ref(), "Files" | "All")
                && s.style
                    .add_modifier
                    .contains(ratatui_core::style::Modifier::BOLD)
        })
        .map(|s| s.content.to_string())
}

fn screen(app: &mut App) -> String {
    app.live()
        .0
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn catalogs_filter_modes_owners_and_fold_unicode() {
    let mut resources = catalog();
    resources[0].owner = Some("package".into());
    resources[1].owner = Some("package".into());
    assert_eq!(mentions::catalog(&resources, "", Mode::All, false).len(), 2);
    assert!(mentions::catalog(&resources, "", Mode::Filesystem, false).is_empty());
    assert_eq!(
        mentions::catalog(&resources, "review", Mode::All, false)[0]
            .resource
            .as_ref()
            .unwrap()
            .kind,
        ResourceKind::Skill
    );
    assert_eq!(mentions::catalog(&resources, "", Mode::All, true).len(), 1);
    assert_eq!(
        mentions::catalog(&resources, "CFE", Mode::All, false)[0].path,
        "Café"
    );
    assert_eq!(
        mentions::catalog(&resources, "中文", Mode::All, false).len(),
        2
    );
    assert_eq!(Mode::All.cycle().cycle(), Mode::All);
    assert_eq!(Mode::All.cycle(), Mode::Filesystem);
}

#[test]
fn bare_at_defaults_to_files_and_shift_tab_exposes_resources() {
    let mut app = app();
    app.paste("@");
    assert!(app.take_index_request().is_some());
    app.set_index(Index::from_paths([
        "root.txt".into(),
        "dir/".into(),
        "dir/nested.txt".into(),
    ]));
    let files = screen(&mut app);
    assert!(files.contains("root.txt") && files.contains("dir/"));
    assert!(!files.contains("nested.txt") && !files.contains("Café"));
    app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert!(screen(&mut app).contains("@Café"));
    key(&mut app, KeyCode::Esc);
    app.event(event(EventKind::ResourcesUpdated {
        resources: catalog(),
    }));
    assert!(!screen(&mut app).contains("@Café"));
    key(&mut app, KeyCode::Backspace);
    app.paste("$review");
    key(&mut app, KeyCode::Tab);
    assert!(app.take_submissions().is_empty());
    key(&mut app, KeyCode::Enter);
    let submitted = app.take_submissions();
    assert!(
        matches!(&submitted[0].op, Op::UserInput { content, .. } if content == &[InputContent::Text { text: "real reference for review ".into() }])
    );
}

#[test]
fn completion_replaces_whole_tokens_and_preserves_atomic_neighbors() {
    let mut c = Composer::default();
    c.insert_image("image/png".into(), "AA".into());
    c.insert("@\"my notes.md\"\nnext");
    c.home();
    c.left();
    c.left();
    let (range, query, _) = c.resource_token().unwrap();
    assert_eq!(query, "my notes.md");
    c.complete_path(range, "\"my notes.md\" ");
    assert_eq!(c.text(), "[Image #1] \"my notes.md\" \nnext");
    assert_eq!(c.submission().1.len(), 1);
    c.undo();
    assert!(c.text().contains("@\"my notes.md\""));
    for text in [
        "user@example.com",
        "! echo @file",
        "$HOME",
        "$PATH",
        "$1",
        "$20",
        "${HOME}",
        "$(cmd)",
        "$?",
        r"\@file",
        r"\$review",
        "`@file`",
        "`$review",
        "```rust\n@file",
        "~~~\n$review",
    ] {
        c.set_text(text);
        assert!(c.resource_token().is_none(), "{text}");
    }
    c.set_text("use (@rev) after");
    c.home();
    for _ in 0..7 {
        c.right();
    }
    let (range, _, _) = c.resource_token().unwrap();
    c.replace_reference(range, "$review".into(), "real skill path".into());
    assert_eq!(c.submission().0, "use (real skill path ) after");
    c.backspace();
    c.backspace();
    assert_eq!(c.submission().0, "use () after");
    c.undo();
    assert!(c.submission().0.contains("real skill path"));
    c.set_text("$review");
    assert_eq!(c.submission().0, "$review");
}

#[test]
fn clipboard_results_are_bound_to_focus_draft_and_session() {
    let mut app = app();
    let ticket = app.clipboard_ticket().unwrap();
    app.focus(false);
    app.focus(true);
    app.clipboard_received(
        ticket,
        Some(Clipboard::Image("image/png".into(), "stale".into())),
    );
    assert!(!screen(&mut app).contains("Image"));
    let ticket = app.clipboard_ticket().unwrap();
    app.paste("edited");
    app.clipboard_received(
        ticket,
        Some(Clipboard::Image("image/png".into(), "stale".into())),
    );
    assert!(!screen(&mut app).contains("Image"));
    let ticket = app.clipboard_ticket().unwrap();
    app.clipboard_received(
        ticket,
        Some(Clipboard::Image("image/png".into(), "bytes".into())),
    );
    assert!(screen(&mut app).contains("[Image #1]"));
    key(&mut app, KeyCode::Enter);
    assert!(
        matches!(&app.take_submissions()[0].op, Op::UserInput { content, .. } if content.contains(&InputContent::Image { mime_type: "image/png".into(), data: "bytes".into() }))
    );
}

#[test]
fn loading_tab_and_reload_reject_stale_index_results() {
    let mut app = app();
    app.paste("@missing");
    let (_, generation, cancel) = app.take_index_request().unwrap();
    key(&mut app, KeyCode::Tab);
    assert!(app.take_submissions().is_empty());
    app.opened(
        std::env::current_dir().unwrap(),
        Default::default(),
        true,
        vec![],
    );
    assert!(cancel.is_cancelled());
    app.set_index_result(generation, Index::from_paths(["missing-old-file".into()]));
    assert!(!screen(&mut app).contains("missing-old-file"));
}

#[test]
fn resource_and_permissions_snapshots() {
    let mut shots = Vec::new();
    for light in [false, true] {
        for width in [40, 80] {
            let theme = Theme::fixture(ColorLevel::TrueColor, light);
            let mut matches = mentions::catalog(&catalog(), "", Mode::All, false);
            matches.extend(file_matches(
                Index::from_paths(["src/".into(), "src/中文 file.rs".into()]),
                "",
            ));
            shots.push(format!(
                "menu {width} light={light} · Files / All · Shift+Tab\n{}",
                support::dump(&mentions::popup(
                    &matches,
                    1,
                    false,
                    Mode::All,
                    "4 matches",
                    width,
                    11,
                    &theme
                ))
            ));
            let matched = mentions::catalog(&catalog(), "cfe", Mode::All, false);
            shots.push(format!(
                "accent-folded match\n{}",
                support::dump(&mentions::popup(
                    &matched,
                    0,
                    false,
                    Mode::All,
                    &format!("{} matches", matched.len()),
                    width,
                    11,
                    &theme
                ))
            ));
            let mut panel = Information::new("Permissions".into(), "Mode: Rules (interactive)\nPolicy: /user/.tiechui/config.toml\nProject: not loaded (untrusted)\nNo OS sandbox; session grants clear on reload.\nEffective rules (later matches win per surface):\n* · * · Allow · user\nbash · rm * · Ask · user\nTemporary session grants:\npath_read · /project/docs/* · Allow\nRecent decisions (this instance):\napproved · bash · rm * · approval".into());
            shots.push(format!(
                "permissions {width} light={light}\n{}",
                support::dump(&panel.render(width, 20, &theme))
            ));
        }
    }
    insta::assert_snapshot!(shots.join("\n\n"));
}

fn file_matches(mut index: Index, query: &str) -> Vec<mentions::Match> {
    index.query(query, "");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    while index.pending() {
        assert!(std::time::Instant::now() < deadline);
        index.tick();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    index.rows()
}

#[test]
fn skill_picker_restores_draft_and_inserts_at_saved_cursor() {
    let mut app = app();
    app.paste("before after");
    for _ in 0..5 {
        key(&mut app, KeyCode::Left);
    }
    let open = || {
        event(EventKind::Extension {
            name: RESOURCE_PICKER_EVENT.into(),
            data: serde_json::to_value(ResourcePickerRequest {
                kind: ResourceKind::Skill,
            })
            .unwrap(),
        })
    };
    app.event(open());
    assert!(screen(&mut app).contains("Skills"));
    key(&mut app, KeyCode::Esc);
    assert!(screen(&mut app).contains("before after"));
    app.event(open());
    app.opened(
        std::env::current_dir().unwrap(),
        Default::default(),
        true,
        vec![],
    );
    assert!(screen(&mut app).contains("before after"));
    app.event(event(EventKind::ResourcesUpdated {
        resources: catalog(),
    }));
    app.event(open());
    key(&mut app, KeyCode::Enter);
    assert!(app.take_submissions().is_empty());
    key(&mut app, KeyCode::Enter);
    assert!(
        matches!(&app.take_submissions()[0].op, Op::UserInput { content, .. } if content == &[InputContent::Text { text: "before real reference for review after".into() }])
    );
}

#[test]
fn directory_tab_scopes_search_enter_inserts_and_left_moves_cursor() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir(temp.path().join("中文 dir")).unwrap();
    let mut app = app();
    app.opened(temp.path().into(), Default::default(), true, vec![]);
    app.paste("@");
    app.set_index(Index::from_paths(["中文 dir/".into()]));
    key(&mut app, KeyCode::Tab);
    assert!(screen(&mut app).contains("@\"中文 dir/\""));
    let (root, _, _) = app.take_index_request().unwrap();
    assert_eq!(root, temp.path().join("中文 dir"));
    app.set_index(Index::from_paths(["child.txt".into()]));
    app.paste("chi");
    for _ in 0..100 {
        if !app.searching() {
            break;
        }
        app.search_step();
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    key(&mut app, KeyCode::Enter);
    assert!(screen(&mut app).contains("@\"中文 dir/child.txt\""));
    assert!(app.take_submissions().is_empty());
    key(&mut app, KeyCode::Enter);
    assert!(
        matches!(&app.take_submissions()[0].op, Op::UserInput { content, .. } if content == &[InputContent::Text { text: "@\"中文 dir/child.txt\" ".into() }])
    );

    app.paste("@");
    app.set_index(Index::from_paths(["中文 dir/".into()]));
    key(&mut app, KeyCode::Left);
    assert_ne!(filter(&mut app).as_deref(), Some("All"));
    app.paste("before");
    assert!(screen(&mut app).contains("before@"));
}

#[test]
fn streamed_resources_preserve_selection_and_new_mentions_reset_filter() {
    let mut app = app();
    app.paste("$");
    let mut resources = vec![
        resource(ResourceKind::Skill, "bravo"),
        resource(ResourceKind::Skill, "charlie"),
    ];
    app.event(event(EventKind::ResourcesUpdated {
        resources: resources.clone(),
    }));
    key(&mut app, KeyCode::Down);
    resources.insert(0, resource(ResourceKind::Skill, "alpha"));
    app.event(event(EventKind::ResourcesUpdated { resources }));
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Enter);
    assert!(
        matches!(&app.take_submissions()[0].op, Op::UserInput { content, .. } if content == &[InputContent::Text { text: "real reference for charlie ".into() }])
    );
    app.paste("@");
    app.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    assert_eq!(filter(&mut app).as_deref(), Some("All"));
    key(&mut app, KeyCode::Esc);
    app.paste(" @");
    assert_eq!(filter(&mut app).as_deref(), Some("Files"));
}

#[test]
fn same_name_skills_show_paths_and_preserve_the_selected_source_on_refresh() {
    let mut app = app();
    let mut resources = ["/agents/review/SKILL.md", "/claude/review/SKILL.md"]
        .map(|path| {
            let mut skill = resource(ResourceKind::Skill, "review");
            skill.id = format!("skill:{path}");
            skill.path = Some(path.into());
            skill.reference = format!("[$review](<{path}>)");
            skill.source = Some("user".into());
            skill.search_terms = vec![path.into()];
            skill
        })
        .to_vec();
    let selected_reference = resources[1].reference.clone();
    app.event(event(EventKind::ResourcesUpdated {
        resources: resources.clone(),
    }));
    app.paste("$review");
    let shown = screen(&mut app);
    assert!(shown.contains("/agents/review/SKILL.md"), "{shown}");
    assert!(shown.contains("/claude/review/SKILL.md"), "{shown}");
    key(&mut app, KeyCode::Down);
    resources.reverse();
    app.event(event(EventKind::ResourcesUpdated {
        resources: resources.clone(),
    }));
    key(&mut app, KeyCode::Tab);
    key(&mut app, KeyCode::Enter);
    assert!(
        matches!(&app.take_submissions()[0].op, Op::UserInput { content, .. }
        if content == &[InputContent::Text { text: format!("{selected_reference} ") }])
    );
    let matched = mentions::catalog(&resources, "claude", Mode::All, true);
    assert_eq!(matched.len(), 1);
    assert_eq!(
        matched[0].resource.as_ref().unwrap().reference,
        selected_reference
    );
}

#[test]
fn menus_stay_above_composer_and_keep_selection_and_cursor_on_short_screens() {
    for width in [40, 80] {
        let mut app = app();
        app.resize(width, 8);
        app.paste("$review");
        let (lines, cursor) = app.main_frame();
        let rows = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let selected = rows
            .iter()
            .position(|l| l.starts_with("  › $review"))
            .unwrap();
        let composer = rows
            .iter()
            .position(|l| l.starts_with("› $review"))
            .unwrap();
        assert!(selected < composer, "{rows:?}");
        assert_eq!(cursor.unwrap().0, composer);
        assert!(rows.len() <= 8);
    }
}

#[test]
fn manual_skills_match_descriptions_after_names_and_stale_bindings_do_not_rebind() {
    let mut manual = resource(ResourceKind::Skill, "release");
    manual.description = "review release notes".into();
    manual.manual = true;
    let review = resource(ResourceKind::Skill, "review");
    let found = mentions::catalog(&[manual.clone(), review], "review", Mode::All, true);
    assert_eq!(
        found.iter().map(|m| m.path.as_str()).collect::<Vec<_>>(),
        ["review", "release"]
    );
    let mut composer = Composer::default();
    composer.replace_reference(0..0, "$release".into(), "old-path".into());
    composer.rebind_references(&[manual]);
    assert_eq!(composer.submission().0, "old-path ");
    composer.backspace();
    composer.backspace();
    composer.insert("$release");
    assert_eq!(composer.submission().0, "$release");
}

#[test]
fn command_arguments_survive_caret_movement_and_no_match_stays_literal() {
    let mut app = app();
    app.event(event(EventKind::CommandsUpdated {
        commands: vec![CommandInfo {
            name: "custom".into(),
            description: "Custom action".into(),
            argument_hint: Some("input".into()),
        }],
    }));
    app.paste("  /custom  \"two words\"  tail ");
    for _ in 0..8 {
        key(&mut app, KeyCode::Left);
    }
    key(&mut app, KeyCode::Enter);
    assert_eq!(
        app.take_submissions()[0].op,
        Op::RunCommand {
            name: "custom".into(),
            arguments: " \"two words\"  tail ".into()
        }
    );
    app.paste("/nosuchcommand");
    assert!(screen(&mut app).contains("No matching commands"));
    key(&mut app, KeyCode::Enter);
    assert!(
        matches!(&app.take_submissions()[0].op, Op::UserInput { content, .. } if content == &[InputContent::Text { text: "/nosuchcommand".into() }])
    );
}
