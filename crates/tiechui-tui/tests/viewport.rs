mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui_core::text::Line;
use std::time::{Duration, Instant};
use tiechui_protocol::*;
use tiechui_tui::{
    App, Options,
    context::ContextPanel,
    palette::ColorLevel,
    selection::Selection,
    terminal,
    theme::Theme,
    theme_picker::{self, Outcome, ThemePicker},
    wrap,
};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn text(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn event(kind: EventKind) -> Event {
    Event {
        submission_id: None,
        thread_id: ThreadId("t".into()),
        epoch: SessionEpoch(0),
        event: kind,
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
    app.take_history();
    app
}
fn complete(app: &mut App, id: &str, source: String) {
    app.event(event(EventKind::ItemCompleted {
        turn_id: TurnId("turn".into()),
        item: Item::Known(KnownItem::AssistantMessage {
            id: ItemId(id.into()),
            text: source,
        }),
    }));
}

#[test]
fn fullscreen_keeps_input_at_the_bottom_and_scrolls_without_losing_the_draft() {
    let mut app = app();
    app.resize(80, 20);
    complete(
        &mut app,
        "long",
        (0..60).map(|n| format!("ROW_{n:02}\n")).collect(),
    );
    app.paste("draft 界面");
    let (frame, cursor) = app.main_frame();
    assert!(app.fullscreen());
    assert_eq!(frame.len(), 20);
    assert!(text(&frame).contains("ROW_59"));
    assert!(!text(&frame).contains("ROW_00"));
    let cursor = cursor.unwrap();
    assert!(text(&frame[cursor.0..=cursor.0]).contains("draft 界面"));
    app.mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    });
    assert!(text(&app.main_frame().0).contains("esc latest"));
    app.key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
    assert!(text(&app.main_frame().0).contains("ROW_59"));
    app.key(key(KeyCode::PageUp));
    let (scrolled, after) = app.main_frame();
    assert_eq!(after, Some(cursor));
    assert!(!text(&scrolled).contains("ROW_59"));
    assert!(text(&scrolled).contains("esc latest"));
    complete(&mut app, "new", "NEW_OUTPUT".into());
    assert!(!text(&app.main_frame().0).contains("NEW_OUTPUT"));
    app.key(key(KeyCode::Esc));
    assert!(text(&app.main_frame().0).contains("NEW_OUTPUT"));
    assert!(text(&app.main_frame().0).contains("draft 界面"));
}

#[test]
fn fullscreen_stream_reflows_and_finishes_without_duplicate_rows() {
    let mut app = app();
    app.event(event(EventKind::ItemStarted {
        turn_id: TurnId("turn".into()),
        item: Item::Known(KnownItem::AssistantMessage {
            id: ItemId("live".into()),
            text: String::new(),
        }),
    }));
    let source = "STREAM_HEAD 界面 café 👩‍💻\nSTREAM_TAIL";
    app.event(event(EventKind::ItemDelta {
        turn_id: TurnId("turn".into()),
        item_id: ItemId("live".into()),
        delta: source.into(),
    }));
    for width in [40, 80, 20, 100] {
        app.resize(width, 24);
        let (frame, cursor) = app.main_frame();
        assert_eq!(frame.len(), 24);
        let rendered = text(&frame);
        assert_eq!(rendered.matches("STREAM_HEAD").count(), 1);
        assert_eq!(rendered.matches("STREAM_TAIL").count(), 1);
        assert!(frame.iter().all(|line| wrap::line_width(line) <= width));
        assert!(cursor.is_some_and(|(row, col)| row < 24 && col < width));
    }
    complete(&mut app, "live", source.into());
    let rendered = text(&app.main_frame().0);
    assert_eq!(rendered.matches("STREAM_HEAD").count(), 1);
    assert_eq!(rendered.matches("STREAM_TAIL").count(), 1);
}

#[test]
fn fullscreen_selection_freezes_transcript_but_keeps_composer_live() {
    let mut app = app();
    app.resize(80, 24);
    complete(&mut app, "first", "SELECT_ME 界面".into());
    app.paste("still editable");
    app.main_frame();
    for (kind, column) in [
        (MouseEventKind::Down(MouseButton::Left), 2),
        (MouseEventKind::Drag(MouseButton::Left), 11),
        (MouseEventKind::Up(MouseButton::Left), 11),
    ] {
        app.mouse(MouseEvent {
            kind,
            column,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
    }
    complete(&mut app, "second", "LATER_OUTPUT".into());
    app.set_notice("live status".into());
    let (frame, _) = app.main_frame();
    assert!(!text(&frame).contains("LATER_OUTPUT"));
    assert!(text(&frame).contains("still editable"));
    assert!(text(&frame).contains("live status"));
    assert_eq!(frame.len(), 24);
    assert_eq!(app.take_copy().as_deref(), Some("SELECT_ME"));
    app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(app.take_copy().as_deref(), Some("SELECT_ME"));
    app.key(key(KeyCode::Esc));
    assert!(text(&app.main_frame().0).contains("LATER_OUTPUT"));
}

fn select_main_text(app: &mut App, release: bool) {
    app.main_frame();
    for (kind, column) in [
        (MouseEventKind::Down(MouseButton::Left), 2),
        (MouseEventKind::Drag(MouseButton::Left), 8),
        (MouseEventKind::Up(MouseButton::Left), 11),
    ] {
        if !release && kind == MouseEventKind::Up(MouseButton::Left) {
            break;
        }
        app.mouse(MouseEvent {
            kind,
            column,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
    }
}

#[test]
fn copy_setting_applies_at_startup_and_when_a_session_opens() {
    let mut app = App::new(
        Theme::fixture(ColorLevel::TrueColor, false),
        80,
        Options {
            copy_on_select: false,
            ..Options::default()
        },
    );
    app.event(event(EventKind::ThreadStarted {
        replay: false,
        thread: Thread {
            id: ThreadId("t".into()),
            epoch: SessionEpoch(0),
            turns: vec![],
        },
    }));
    complete(&mut app, "first", "SELECT_ME".into());
    select_main_text(&mut app, true);
    assert!(app.take_copy().is_none());
    for enabled in [false, true, false] {
        app.opened(app.cwd().to_owned(), Default::default(), enabled, vec![]);
        select_main_text(&mut app, true);
        assert_eq!(app.take_copy().as_deref(), enabled.then_some("SELECT_ME"));
        app.key(key(KeyCode::Enter));
        assert_eq!(app.take_copy().as_deref(), Some("SELECT_ME"));
    }
}

#[test]
fn resize_focus_loss_session_open_and_footer_scroll_cancel_selection_and_pending_copy() {
    for release in [false, true] {
        for cancel in 0..4 {
            let mut app = app();
            complete(&mut app, "first", "SELECT_ME".into());
            select_main_text(&mut app, release);
            match cancel {
                0 => app.resize(80, 24),
                1 => app.focus(false),
                2 => app.opened(app.cwd().to_owned(), Default::default(), true, vec![]),
                _ => app.mouse(MouseEvent {
                    kind: MouseEventKind::ScrollDown,
                    column: 0,
                    row: 23,
                    modifiers: KeyModifiers::NONE,
                }),
            }
            app.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: 11,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
            assert!(app.take_copy().is_none());
            assert!(app.main_frame().1.is_some());
        }
    }
}

#[test]
fn closing_viewer_discards_pending_copy() {
    let mut app = app();
    complete(&mut app, "first", "SELECT_ME".into());
    app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    let page = app.pager_frame(24).unwrap();
    let row = page
        .iter()
        .position(|line| text(std::slice::from_ref(line)).contains("SELECT_ME"))
        .unwrap() as u16;
    for (kind, column) in [
        (MouseEventKind::Down(MouseButton::Left), 2),
        (MouseEventKind::Up(MouseButton::Left), 11),
    ] {
        app.mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }
    app.key(key(KeyCode::Char('q')));
    assert!(app.pager_frame(24).is_none());
    assert!(app.take_copy().is_none());
}

#[test]
fn copy_footer_expires_and_clears_on_key_while_selection_stays() {
    let mut app = app();
    complete(&mut app, "first", "SELECT_ME".into());
    select_main_text(&mut app, true);
    assert_eq!(app.take_copy().as_deref(), Some("SELECT_ME"));
    let now = Instant::now();
    app.copy_finished(9, true, now);
    assert!(app.copy_hint_pending());
    app.expire_copy_hint(now + Duration::from_millis(1999));
    assert!(text(&app.main_frame().0).contains("✓ Sent 9 chars to clipboard"));
    app.expire_copy_hint(now + Duration::from_secs(2));
    assert!(!app.copy_hint_pending());
    assert!(!text(&app.main_frame().0).contains("✓ Sent 9 chars to clipboard"));
    app.copy_finished(9, false, now);
    assert!(text(&app.main_frame().0).contains("Selection too long to copy (9 chars)"));
    app.key(key(KeyCode::Enter));
    assert_eq!(app.take_copy().as_deref(), Some("SELECT_ME"));
    assert!(!text(&app.main_frame().0).contains("Selection too long to copy (9 chars)"));

    app.key(key(KeyCode::Esc));
    app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    app.pager_frame(24);
    app.mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 0,
        row: 2,
        modifiers: KeyModifiers::NONE,
    });
    app.copy_finished(9, true, now);
    assert!(text(&app.pager_frame(24).unwrap()).contains("✓ Sent 9 chars to clipboard"));
    app.expire_copy_hint(now + Duration::from_secs(2));
    assert!(!text(&app.pager_frame(24).unwrap()).contains("✓ Sent 9 chars to clipboard"));
}

#[test]
fn fullscreen_bounds_and_overlay_return_preserve_the_main_view() {
    let mut app = app();
    complete(&mut app, "first", "MAIN_HISTORY".into());
    for (width, height) in [(1, 1), (20, 6), (40, 12), (80, 24)] {
        app.resize(width, height);
        let (frame, cursor) = app.main_frame();
        assert_eq!(frame.len(), height);
        assert!(cursor.is_none_or(|(row, col)| row < height && col < width));
    }
    app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(text(&app.pager_frame(24).unwrap()).contains("T R A N S C R I P T"));
    app.key(key(KeyCode::Esc));
    assert!(app.pager_frame(24).is_none());
    assert!(text(&app.main_frame().0).contains("MAIN_HISTORY"));
    let inline = App::new(
        Theme::fixture(ColorLevel::TrueColor, false),
        80,
        Options {
            no_alt_screen: true,
            ..Options::default()
        },
    );
    assert!(!inline.fullscreen());
}

#[test]
fn resize_rebuilds_cjk_wide_lines_and_final_stream_without_duplicates() {
    let mut app = app();
    complete(&mut app, "old", "BEGIN 界面 café 👩‍💻 ".repeat(8));
    app.take_history();
    app.event(event(EventKind::ItemStarted {
        turn_id: TurnId("turn".into()),
        item: Item::Known(KnownItem::AssistantMessage {
            id: ItemId("live".into()),
            text: String::new(),
        }),
    }));
    let source = "STREAM 界面 and a long line of words ".repeat(5) + "\n";
    app.event(event(EventKind::ItemDelta {
        turn_id: TurnId("turn".into()),
        item_id: ItemId("live".into()),
        delta: source.clone(),
    }));
    app.take_history();
    for width in [40, 17, 100, 1, 80] {
        app.resize(width, 24);
        let rows = app
            .reflow(Instant::now() + Duration::from_secs(1), 1000)
            .unwrap();
        assert_eq!(
            text(&rows).matches("BEGIN").count(),
            if width < 7 { 0 } else { 8 }
        );
        if width >= 17 {
            assert_eq!(text(&rows).matches("STREAM").count(), 5);
        }
        assert!(
            rows.iter()
                .all(|row| wrap::line_width(row) <= width || width == 1)
        );
        assert!(app.take_history().is_empty());
    }
    complete(&mut app, "live", source + "FINAL");
    let rows = app
        .reflow(Instant::now() + Duration::from_secs(1), 1000)
        .unwrap();
    assert_eq!(text(&rows).matches("STREAM").count(), 5);
    assert_eq!(text(&rows).matches("FINAL").count(), 1);
    assert!(app.take_history().is_empty());
    app.resize(80, 40);
    assert!(app.reflow(Instant::now(), 3).is_none());
    assert_eq!(
        app.reflow(Instant::now() + Duration::from_secs(1), 3)
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn terminal_caps_and_late_fragmented_replies() {
    for (name, cap) in [
        ("vscode", 1000),
        ("WezTerm", 3500),
        ("Alacritty", 10000),
        ("unknown", 1000),
    ] {
        assert_eq!(
            terminal::reflow_cap(&|key| (key == "TERM_PROGRAM").then(|| name.into())),
            cap
        );
    }
    assert_eq!(
        terminal::reflow_cap(&|key| (key == "WT_SESSION").then(|| "test".into())),
        9001
    );
    let mut filter = terminal::ReplyFilter::default();
    assert_eq!(filter.key(key(KeyCode::Char('x'))).0.len(), 1);
    for terminator in [
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        KeyEvent::new(KeyCode::Char('\\'), KeyModifiers::ALT),
    ] {
        assert!(
            filter
                .key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT))
                .0
                .is_empty()
        );
        for ch in "11;rgb:ff/ff/ff".chars() {
            assert!(filter.key(key(KeyCode::Char(ch))).0.is_empty());
        }
        assert_eq!(
            filter.key(terminator).1.unwrap().1,
            Some(tiechui_tui::palette::Rgb(255, 255, 255))
        );
        assert_eq!(
            filter.key(key(KeyCode::Char('z'))).0,
            vec![key(KeyCode::Char('z'))]
        );
    }
    filter.key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT));
    assert_eq!(filter.key(key(KeyCode::Char('x'))).0.len(), 2);
    filter.key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::ALT));
    for ch in "10;rgb:1/2/3".chars() {
        filter.key(key(KeyCode::Char(ch)));
    }
    assert!(filter.key(key(KeyCode::Esc)).0.is_empty());
    assert!(filter.key(key(KeyCode::Char('\\'))).1.unwrap().0.is_some());
}

#[test]
fn selection_preserves_unicode_revision_and_copy_shortcuts() {
    let mut selection = Selection::default();
    selection.frame(vec![Line::from("café 界 👩‍💻"), Line::from("second")]);
    let mouse = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let now = Instant::now();
    selection.mouse(
        mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
        now,
        true,
    );
    selection.mouse(
        mouse(MouseEventKind::Drag(MouseButton::Left), 10, 0),
        now,
        true,
    );
    selection.mouse(
        mouse(MouseEventKind::Up(MouseButton::Left), 10, 0),
        now,
        true,
    );
    assert_eq!(selection.selected_text().as_deref(), Some("café 界 👩‍💻"));
    assert_eq!(
        text(&selection.frame(vec![Line::from("changed")])),
        "café 界 👩‍💻\nsecond"
    );
    assert!(selection.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    assert_eq!(selection.take_copy().as_deref(), Some("café 界 👩‍💻"));
    assert!(selection.key(key(KeyCode::Left)));
    assert_eq!(selection.selected_text().as_deref(), Some("café 界"));
    assert!(selection.key(key(KeyCode::Esc)));
    assert!(selection.selected_text().is_none());
    for count in 1..=3 {
        let at = now + Duration::from_millis(count * 10);
        selection.mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), 1, 0),
            at,
            true,
        );
        selection.mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1, 0), at, true);
        match count {
            1 => assert!(selection.selected_text().is_none()),
            2 => assert_eq!(selection.selected_text().as_deref(), Some("café")),
            _ => assert_eq!(selection.selected_text().as_deref(), Some("café 界 👩‍💻")),
        }
    }
}

#[test]
fn pager_wheel_pauses_follow_and_returns_to_end() {
    let mut app = app();
    complete(
        &mut app,
        "many",
        (0..80).map(|n| format!("ROW-{n}\n")).collect(),
    );
    app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(app.context_open());
    assert!(text(&app.pager_frame(12).unwrap()).contains("ROW-79"));
    app.mouse(MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 4,
        row: 4,
        modifiers: KeyModifiers::NONE,
    });
    assert!(!text(&app.pager_frame(12).unwrap()).contains("ROW-79"));
    app.key(key(KeyCode::End));
    assert!(text(&app.pager_frame(12).unwrap()).contains("ROW-79"));
    app.key(key(KeyCode::Esc));
    assert!(!app.context_open());
}

#[test]
fn theme_preview_cancel_save_and_custom_reload() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("custom.tmTheme"),
        include_bytes!("../assets/themes/Catppuccin Latte.tmTheme"),
    )
    .unwrap();
    std::fs::write(directory.path().join("bad.tmTheme"), "invalid").unwrap();
    let entries = theme_picker::catalog(Some(directory.path()));
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == "custom" && entry.custom)
    );
    assert!(!entries.iter().any(|entry| entry.name == "bad"));
    let original = Theme::fixture(ColorLevel::TrueColor, false);
    let mut picker = ThemePicker::new(entries, "automatic", &original);
    let Outcome::Preview(Some(preview)) = picker.key(key(KeyCode::Down)) else {
        panic!("preview")
    };
    let mut changed = original.clone();
    changed.syntax = Some(preview);
    let markdown = "```rs\nlet x = 42;\n```\n\n| Type |\n| --- |\n| value |";
    let colored = tiechui_tui::markdown::render(markdown, 80, &changed);
    assert_ne!(
        colored,
        tiechui_tui::markdown::render(markdown, 80, &original)
    );
    assert!(
        colored
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.content.contains("let") && span.style.fg.is_some())
    );
    assert!(
        colored
            .iter()
            .flat_map(|line| &line.spans)
            .any(|span| span.content.contains("Type") && span.style.fg.is_some())
    );
    assert_ne!(
        tiechui_tui::syntax::highlight("let x = 42;", "rs", &original),
        tiechui_tui::syntax::highlight("let x = 42;", "rs", &changed)
    );
    assert!(matches!(
        picker.key(key(KeyCode::Esc)),
        Outcome::Cancel(None)
    ));
    assert!(
        matches!(picker.key(key(KeyCode::Enter)), Outcome::Save(name) if name == "Catppuccin Latte")
    );
    let mut app = app();
    complete(&mut app, "code", markdown.into());
    app.take_history();
    app.paste("/theme");
    app.key(key(KeyCode::Enter));
    app.key(key(KeyCode::Down));
    assert!(!app.reflow_pending());
    app.key(key(KeyCode::Esc));
    assert!(!app.reflow_pending());
    assert!(app.take_history().is_empty());
    assert!(app.take_theme_save().is_none());
    app.paste("/theme");
    app.key(key(KeyCode::Enter));
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Enter));
    assert_eq!(app.take_theme_save().as_deref(), Some("Catppuccin Latte"));
    assert!(!app.reflow_pending());
    assert!(app.take_history().is_empty());
    app.reload_theme(Some("Catppuccin Mocha".into()));
    app.paste("/theme");
    app.key(key(KeyCode::Enter));
    assert!(text(&app.live().0).contains("› Catppuccin Mocha"));

    let mut custom = App::new(
        original,
        80,
        Options {
            themes_dir: Some(directory.path().to_owned()),
            syntax_theme: Some("custom".into()),
            ..Default::default()
        },
    );
    custom.event(event(EventKind::ThreadStarted {
        replay: false,
        thread: Thread {
            id: ThreadId("t".into()),
            epoch: SessionEpoch(0),
            turns: vec![],
        },
    }));
    complete(&mut custom, "code", "```rs\nlet x = 42;\n```".into());
    let before = custom.take_history();
    std::fs::write(
        directory.path().join("custom.tmTheme"),
        include_bytes!("../assets/themes/Catppuccin Mocha.tmTheme"),
    )
    .unwrap();
    custom.reload_theme(Some("custom".into()));
    assert!(!custom.reflow_pending());
    assert!(custom.take_history().is_empty());
    custom.resize(81, 24);
    let after = custom
        .reflow(Instant::now() + Duration::from_secs(1), 1000)
        .unwrap();
    assert_eq!(text(&before), text(&after));
    assert_ne!(before, after);
}

#[test]
fn theme_picker_keeps_bounds_and_hints_at_short_and_wide_sizes() {
    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    let picker = ThemePicker::new(theme_picker::catalog(None), "automatic", &theme);
    for width in [20, 40, 80, 100, 160] {
        for height in [1, 6, 12, 24] {
            let rows = picker.render(width, height, &theme);
            assert!(rows.len() <= height);
            assert!(rows.iter().all(|row| wrap::line_width(row) <= width));
            assert!(text(&rows[rows.len() - 1..]).contains("preview"));
        }
    }
}

#[test]
fn theme_picker_snapshots() {
    for light in [false, true] {
        for width in [40, 80] {
            let theme = Theme::fixture(ColorLevel::TrueColor, light);
            let picker = ThemePicker::new(theme_picker::catalog(None), "automatic", &theme);
            insta::assert_snapshot!(
                format!(
                    "theme_picker_{width}_{}",
                    if light { "light" } else { "dark" }
                ),
                support::dump(&picker.render(width, 24, &theme))
            );
        }
    }
}

#[test]
fn long_session_visible_frames_stay_within_fixed_budget() {
    let mut app = app();
    let messages: Vec<_> = (0..20_000)
        .map(|n| ContextItem {
            label: format!("assistant #{n}"),
            category: "Assistant".into(),
            tokens: 10,
            text: "history".into(),
            ..Default::default()
        })
        .collect();
    for n in 0..20_000 {
        complete(&mut app, &format!("{n}"), format!("Message {n} 界面"));
    }
    app.take_history();
    app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    app.pager_frame(24);
    let mut context = ContextPanel::new(
        ContextSnapshot {
            messages,
            ..Default::default()
        },
        ContextView::Messages,
    );
    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    context.render(80, 24, &theme);
    let start = Instant::now();
    for _ in 0..100 {
        assert_eq!(app.pager_frame(24).unwrap().len(), 24);
        context.key(key(KeyCode::Down));
        assert_eq!(context.render(80, 24, &theme).len(), 24);
    }
    app.key(key(KeyCode::Esc));
    for _ in 0..100 {
        assert_eq!(app.main_frame().0.len(), 24);
        assert!(app.take_history().is_empty());
    }
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "visible frames exceeded the two-second offline budget: {:?}",
        start.elapsed()
    );
}

#[test]
fn fullscreen_control_rows_light_up_and_expand_cells_in_place() {
    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    let mut app = app();
    app.resize(80, 30);
    for item in [
        KnownItem::ToolCall {
            id: ItemId("c".into()),
            name: "bash".into(),
            arguments: serde_json::json!({"command": "cargo test"}),
        },
        KnownItem::ToolResult {
            id: ItemId("r".into()),
            call_id: ItemId("c".into()),
            output: serde_json::json!({"content": [{"kind": "text", "text": "OUT_1\nOUT_2\nOUT_3\nOUT_4\nOUT_5\nOUT_6"}]}),
            is_error: false,
        },
        KnownItem::Compaction {
            id: ItemId("checkpoint".into()),
            summary: "## Goal\nSUMMARY_BODY".into(),
            tokens_before: 9500,
            degraded: false,
        },
    ] {
        app.event(event(EventKind::ItemCompleted {
            turn_id: TurnId("turn".into()),
            item: Item::Known(item),
        }));
    }
    let row = |app: &mut App, label: &str| {
        let frame = app.main_frame().0;
        let found = frame
            .iter()
            .position(|line| text(std::slice::from_ref(line)) == label);
        (frame, found)
    };
    let (frame, Some(more)) = row(&mut app, "    + 3 lines") else {
        panic!("no control row");
    };
    let rendered = text(&frame);
    assert!(
        !rendered.contains("OUT_3") && rendered.contains("OUT_4"),
        "{rendered}"
    );
    assert!(
        rendered.contains("◇ Compacted context · 9.5k tokens summarized\n"),
        "{rendered}"
    );
    let mouse = |app: &mut App, kind, column: u16, row: usize| {
        app.mouse(MouseEvent {
            kind,
            column,
            row: u16::try_from(row).unwrap(),
            modifiers: KeyModifiers::NONE,
        });
    };
    let click = |app: &mut App, column, row| {
        mouse(app, MouseEventKind::Down(MouseButton::Left), column, row);
        mouse(app, MouseEventKind::Up(MouseButton::Left), column, row);
    };
    for column in [1, 2, 3, 4, 12, 13, 14, 15, 79, 80] {
        mouse(&mut app, MouseEventKind::Moved, column, more);
        let style = if (2..15).contains(&column) {
            theme
                .accent
                .add_modifier(ratatui_core::style::Modifier::BOLD)
        } else {
            theme.dim
        };
        assert_eq!(
            app.main_frame().0[more].spans[1].style,
            style,
            "column {column}"
        );
    }
    click(&mut app, 1, more);
    assert!(!text(&app.main_frame().0).contains("OUT_1"));
    click(&mut app, 15, more);
    assert!(!text(&app.main_frame().0).contains("OUT_1"));
    click(&mut app, 2, more);
    let (frame, Some(less)) = row(&mut app, "    − Show less") else {
        panic!("no expanded control");
    };
    assert!(text(&frame).contains("OUT_1"));
    click(&mut app, 15, less);
    assert!(row(&mut app, "    + 3 lines").1.is_some());
    click(&mut app, 14, more);
    let (_, Some(less)) = row(&mut app, "    − Show less") else {
        panic!("right padding did not expand output");
    };
    click(&mut app, 6, less);
    let (_, Some(summary)) = row(&mut app, "    + Show summary") else {
        panic!("no summary control");
    };
    click(&mut app, 18, summary);
    let (frame, hide) = row(&mut app, "    − Hide summary");
    assert!(hide.is_some() && text(&frame).contains("SUMMARY_BODY"));
}
