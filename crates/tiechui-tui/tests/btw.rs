mod support;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tiechui_protocol::*;
use tiechui_tui::{
    App, Options,
    btw::{Outcome, Panel},
    palette::ColorLevel,
    status::{Git, Phase, Snapshot, StatusLineConfig},
    theme::Theme,
    wrap,
};

fn view() -> BtwView {
    BtwView { id: "side-1".into(), model: ModelInfo { provider: "fixture".into(), id: "side-model".into(), context_window: 10000, reasoning: true, thinking_levels: vec![ThinkingLevel::Off, ThinkingLevel::High] }, thinking: ThinkingLevel::High,
        messages: vec![BtwMessage { user: true, text: "How does this stay separate?".into(), ..Default::default() }, BtwMessage { text: "The **side conversation** keeps its own history.\n\n- No tools\n- Share explicitly".into(), thinking: "**Isolation**\n\nPrivate reasoning".into(), complete: true, ..Default::default() }], busy: false, context: Some(TokenUsage { input: 100, output: 20, ..Default::default() }), total: TokenUsage { input: 100, output: 20, ..Default::default() }, notice: None }
}
fn plain(lines: &[ratatui_core::text::Line<'_>]) -> String {
    lines
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
fn render(
    panel: &mut Panel,
    width: usize,
    rows: usize,
    light: bool,
) -> Vec<ratatui_core::text::Line<'static>> {
    panel.render(
        width,
        rows,
        &Theme::fixture(ColorLevel::TrueColor, light),
        &Snapshot {
            cwd: std::path::Path::new("/project"),
            home: None,
            hostname: None,
            model: None,
            thinking: ThinkingLevel::Off,
            context: None,
            total: &TokenUsage::default(),
            compacted: false,
            phase: Phase::Ready,
            permission: Default::default(),
            thread: Some("MAIN"),
            git: &Git::default(),
        },
        &StatusLineConfig::default(),
        "needs input",
    )
}
fn key(panel: &mut Panel, code: KeyCode, modifiers: KeyModifiers) -> Outcome {
    panel.key(KeyEvent::new(code, modifiers), &StatusLineConfig::default())
}
#[test]
fn panel_snapshots_at_both_widths_and_palettes() {
    let mut snapshots = Vec::new();
    for light in [false, true] {
        for width in [40, 80] {
            let mut panel = Panel::new(view());
            snapshots.push(format!(
                "{width} columns / {}\n{}",
                if light { "light" } else { "dark" },
                support::dump(&render(&mut panel, width, 24, light))
            ));
        }
    }
    insta::assert_snapshot!("side_panel", snapshots.join("\n\n"));
}
#[test]
fn modal_keys_draft_busy_and_close_are_isolated() {
    let mut panel = Panel::new(view());
    panel.paste("draft");
    panel.view.busy = true;
    assert!(matches!(
        key(&mut panel, KeyCode::Enter, KeyModifiers::NONE),
        Outcome::Open
    ));
    assert!(plain(&render(&mut panel, 80, 20, false)).contains("draft"));
    assert!(matches!(
        key(&mut panel, KeyCode::Char('/'), KeyModifiers::CONTROL),
        Outcome::Switch
    ));
    key(&mut panel, KeyCode::Char('o'), KeyModifiers::CONTROL);
    assert!(matches!(
        key(&mut panel, KeyCode::Char('/'), KeyModifiers::CONTROL),
        Outcome::Open
    ));
    key(&mut panel, KeyCode::Esc, KeyModifiers::NONE);
    assert!(matches!(
        key(&mut panel, KeyCode::Esc, KeyModifiers::NONE),
        Outcome::Action(BtwAction::Stop)
    ));
    key(&mut panel, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(matches!(
        panel.key(
            KeyEvent::new_with_kind(
                KeyCode::Char('d'),
                KeyModifiers::CONTROL,
                KeyEventKind::Repeat
            ),
            &StatusLineConfig::default()
        ),
        Outcome::Open
    ));
    panel.paste("/statusline");
    key(&mut panel, KeyCode::Enter, KeyModifiers::NONE);
    assert!(plain(&render(&mut panel, 80, 24, false)).contains("Configure Status Line"));
    key(&mut panel, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(matches!(
        key(&mut panel, KeyCode::Char('d'), KeyModifiers::CONTROL),
        Outcome::Action(BtwAction::Close)
    ));
}
#[test]
fn short_multiline_panel_keeps_cursor_and_footer_and_error_once() {
    let mut panel = Panel::new(view());
    panel.view.messages[1].text.clear();
    panel.view.messages[1].error = Some("ORIGINAL_FAILURE".into());
    assert_eq!(
        plain(&render(&mut panel, 80, 30, false))
            .matches("ORIGINAL_FAILURE")
            .count(),
        1
    );
    assert!(!plain(&render(&mut panel, 80, 30, false)).contains("No text response"));
    for i in 0..15 {
        if i > 0 {
            key(&mut panel, KeyCode::Enter, KeyModifiers::SHIFT);
        }
        panel.paste(&format!("LINE_{i:02}"));
    }
    for width in [20, 40, 80] {
        for rows in [8, 12] {
            let screen = render(&mut panel, width, rows, false);
            let text = plain(&screen);
            assert_eq!(screen.len(), rows);
            assert!(
                screen
                    .iter()
                    .all(|line| wrap::width(&plain(std::slice::from_ref(line))) <= width)
            );
            assert!(text.contains("LINE_14") && text.contains("Side"));
            assert!(panel.cursor.is_some_and(|(y, x)| y < rows && x < width));
        }
    }
}
fn event(kind: EventKind) -> Event {
    Event {
        submission_id: None,
        thread_id: ThreadId("main".into()),
        epoch: SessionEpoch(0),
        event: kind,
    }
}
#[test]
fn main_approval_waits_behind_panel_and_switch_preserves_draft() {
    let mut app = App::new(
        Theme::fixture(ColorLevel::TrueColor, false),
        80,
        Options::default(),
    );
    app.event(event(EventKind::ThreadStarted {
        thread: Thread {
            id: ThreadId("main".into()),
            epoch: SessionEpoch(0),
            turns: vec![],
        },
        replay: false,
    }));
    app.event(event(EventKind::Extension {
        name: BTW_EVENT.into(),
        data: serde_json::to_value(BtwEvent::Update {
            view: view(),
            open: true,
        })
        .unwrap(),
    }));
    app.paste("SIDE_DRAFT");
    app.event(event(EventKind::ApprovalRequested {
        turn_id: None,
        approval_id: ApprovalId("ask".into()),
        request: ApprovalRequest::Questions {
            questions: vec![Question {
                id: "q".into(),
                header: "Question".into(),
                question: "MAIN_QUESTION".into(),
                options: None,
            }],
        },
    }));
    let screen = plain(&app.pager_frame(24).unwrap());
    assert!(screen.contains("SIDE_DRAFT") && screen.contains("main needs input"));
    assert!(!screen.contains("MAIN_QUESTION"));
    app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL));
    assert!(app.pager_frame(24).is_none());
    assert!(plain(&app.live().0).contains("MAIN_QUESTION"));
    app.key(KeyEvent::new(KeyCode::Char('7'), KeyModifiers::CONTROL));
    assert!(app.pager_frame(24).is_none());
    app.event(event(EventKind::ApprovalResolved {
        approval_id: ApprovalId("ask".into()),
    }));
    for code in [
        KeyCode::Char('/'),
        KeyCode::Char('7'),
        KeyCode::Char('_'),
        KeyCode::Char('\x1f'),
    ] {
        app.key(KeyEvent::new(code, KeyModifiers::CONTROL));
        assert!(plain(&app.pager_frame(24).unwrap()).contains("SIDE_DRAFT"));
        app.key(KeyEvent::new(code, KeyModifiers::CONTROL));
        assert!(app.pager_frame(24).is_none());
    }
    app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL));
    app.event(event(EventKind::ApprovalRequested {
        turn_id: None,
        approval_id: ApprovalId("permission".into()),
        request: ApprovalRequest::Permission {
            actor: None,
            call_id: "call".into(),
            tool: "bash".into(),
            arguments: serde_json::json!({"command":"pwd"}),
            surface: "bash".into(),
            values: vec!["pwd".into()],
            matched_pattern: None,
            origin: "fixture".into(),
            session_patterns: vec![],
        },
    }));
    let screen = plain(&app.pager_frame(24).unwrap());
    assert!(screen.contains("SIDE_DRAFT") && screen.contains("main needs approval"));
    app.event(event(EventKind::ThreadStarted {
        thread: Thread {
            id: ThreadId("main".into()),
            epoch: SessionEpoch(1),
            turns: vec![],
        },
        replay: false,
    }));
    app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL));
    assert!(app.pager_frame(24).is_none());
}
#[test]
fn thinking_control_row_flips_one_reply_and_ctrl_t_resets_all() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mut panel = Panel::new(view());
    let control = |panel: &mut Panel, label: &str| {
        let screen = render(panel, 80, 24, false);
        let row = screen
            .iter()
            .position(|line| plain(std::slice::from_ref(line)) == label)
            .expect(label);
        (plain(&screen), u16::try_from(row).unwrap())
    };
    let mouse = |panel: &mut Panel, kind, column, row| {
        panel.mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    };
    let (screen, row) = control(&mut panel, "    + Show thinking");
    assert!(!screen.contains("Private reasoning"));
    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    for column in [1, 2, 3, 4, 18, 19, 20, 21] {
        mouse(&mut panel, MouseEventKind::Moved, column, row);
        let screen = render(&mut panel, 80, 24, false);
        let style = if (2..21).contains(&column) {
            theme
                .accent
                .add_modifier(ratatui_core::style::Modifier::BOLD)
        } else {
            theme.dim
        };
        assert_eq!(
            screen[usize::from(row)].spans[1].style,
            style,
            "column {column}"
        );
    }
    mouse(&mut panel, MouseEventKind::Down(MouseButton::Left), 21, row);
    assert!(
        !control(&mut panel, "    + Show thinking")
            .0
            .contains("Private reasoning")
    );
    mouse(&mut panel, MouseEventKind::Down(MouseButton::Left), 2, row);
    let (screen, row) = control(&mut panel, "    − Hide thinking");
    assert!(screen.contains("Private reasoning") && !screen.contains("Thought ·"));
    mouse(&mut panel, MouseEventKind::Down(MouseButton::Left), 20, row);
    control(&mut panel, "    + Show thinking");
    key(&mut panel, KeyCode::Char('t'), KeyModifiers::CONTROL);
    assert!(
        control(&mut panel, "    − Hide thinking")
            .0
            .contains("Private reasoning")
    );
    let narrow = render(&mut panel, 19, 40, false);
    let row = narrow
        .iter()
        .position(|line| plain(std::slice::from_ref(line)) == "    − Hide thinking")
        .unwrap();
    for column in [18, 19, 20] {
        mouse(
            &mut panel,
            MouseEventKind::Moved,
            column,
            u16::try_from(row).unwrap(),
        );
        let narrow = render(&mut panel, 19, 40, false);
        let style = if column < 19 {
            theme
                .accent
                .add_modifier(ratatui_core::style::Modifier::BOLD)
        } else {
            theme.dim
        };
        assert_eq!(narrow[row].spans[1].style, style, "narrow column {column}");
    }
}
