mod support;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::time::{Duration, Instant};
use tiechui_protocol::*;
use tiechui_tui::{App, Options, palette::ColorLevel, rewind::Rewind, theme::Theme};

fn fixture(page: RewindPage) -> RewindPanel {
    let stats = CheckpointStats {
        files: [
            (
                "src/login.rs".into(),
                Some(LineStats {
                    added: 24,
                    removed: 6,
                }),
            ),
            ("image.png".into(), None),
        ]
        .into(),
        error: None,
    };
    RewindPanel {
        rows: vec![
            RewindRow {
                session_id: "session".into(),
                entry_id: "one".into(),
                text: "实现登录页面".into(),
                timestamp: 0,
                stats: Some(stats.clone()),
            },
            RewindRow {
                session_id: "session".into(),
                entry_id: "two".into(),
                text: "Fix login and preserve the image 🦀".into(),
                timestamp: 0,
                stats: Some(stats.clone()),
            },
        ],
        selected: 1,
        page,
        preview: Some(stats),
    }
}
#[test]
fn list_and_preview_at_both_widths_and_palettes() {
    for page in [RewindPage::Messages, RewindPage::Actions] {
        let mut output = String::new();
        for light in [false, true] {
            for width in [40, 80] {
                let panel = Rewind::new(fixture(page));
                let rows = panel.render(width, 32, &Theme::fixture(ColorLevel::TrueColor, light));
                output.push_str(&format!(
                    "\n{width} columns, light={light}\n{}\n",
                    support::dump(&rows)
                ));
            }
        }
        insta::assert_snapshot!(format!("rewind_{page:?}"), output);
    }
}
#[test]
fn checking_conflicts_short_windows_and_repeated_confirmation_keys() {
    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    let mut panel = Rewind::new(fixture(RewindPage::Checking));
    for code in [KeyCode::Enter, KeyCode::Char('1')] {
        // No change, no implicit confirmation while the plan is being checked.
        let before = panel.render(80, 24, &theme);
        let _ = panel.key(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(before, panel.render(80, 24, &theme));
    }
    let mut data = fixture(RewindPage::Actions);
    data.preview = Some(CheckpointStats {
        error: Some("External modification: src/login.rs".into()),
        ..Default::default()
    });
    panel.update(data);
    let render = support::dump(&panel.render(80, 32, &theme));
    assert!(render.contains("External modification"));
    assert!(!render.contains("Restore code"));
    for width in [1, 12, 40, 80] {
        panel.update(fixture(RewindPage::Actions));
        let rows = panel.render(width, 12, &theme);
        assert!(rows.len() <= 12);
        assert!(
            rows.iter()
                .all(|row| tiechui_tui::wrap::line_width(row) <= width)
        );
        if width == 80 {
            let text = support::dump(&rows);
            assert!(text.contains("Never mind"));
            assert!(text.contains("confirms"));
        }
    }
}
fn event(event: EventKind) -> Event {
    Event {
        submission_id: None,
        thread_id: ThreadId("session".into()),
        epoch: SessionEpoch(0),
        event,
    }
}
fn ready() -> App {
    let mut app = App::new(
        Theme::fixture(ColorLevel::TrueColor, false),
        80,
        Options::default(),
    );
    app.event(event(EventKind::ThreadStarted {
        replay: false,
        thread: Thread {
            id: ThreadId("session".into()),
            epoch: SessionEpoch(0),
            turns: vec![],
        },
    }));
    app.event(event(EventKind::CommandsUpdated {
        commands: vec![CommandInfo {
            name: "rewind".into(),
            description: String::new(),
            argument_hint: None,
        }],
    }));
    app.take_submissions();
    app
}
fn esc(app: &mut App, now: Instant) {
    app.key_at(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), now);
}
#[test]
fn double_escape_timing_input_focus_interruptions_and_draft_payloads() {
    let mut app = ready();
    let now = Instant::now();
    esc(&mut app, now);
    assert!(support::dump(&app.live().0).contains("esc again to rewind"));
    esc(&mut app, now + Duration::from_millis(500));
    assert!(app.take_submissions().is_empty());
    esc(&mut app, now + Duration::from_millis(600));
    assert!(
        matches!(app.take_submissions()[0].op, Op::RunCommand { ref name, .. } if name == "rewind")
    );
    esc(&mut app, now);
    app.paste("");
    esc(&mut app, now);
    assert!(app.take_submissions().is_empty());
    app.focus(false);
    app.focus(true);
    esc(&mut app, now);
    assert!(app.take_submissions().is_empty());
    let turn = Turn {
        started_at: 0,
        ended_at: None,
        id: TurnId("turn".into()),
        status: TurnStatus::InProgress,
        items: vec![],
    };
    app.event(event(EventKind::TurnStarted { turn: turn.clone() }));
    esc(&mut app, now);
    assert!(matches!(app.take_submissions()[0].op, Op::Interrupt { .. }));
    app.event(event(EventKind::TurnCompleted {
        turn: Turn {
            started_at: 0,
            ended_at: None,
            status: TurnStatus::Interrupted,
            ..turn
        },
    }));
    esc(&mut app, now);
    assert!(app.take_submissions().is_empty());
    let mut repeat = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    repeat.kind = KeyEventKind::Repeat;
    app.key_at(repeat, now);
    assert!(app.take_submissions().is_empty());
    app.event(event(EventKind::ComposerRefilled {
        content: vec![
            InputContent::Text {
                text: "long\nfolded\npaste\n".repeat(20),
            },
            InputContent::Image {
                mime_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            },
        ],
    }));
    app.focus(true);
    esc(&mut app, now);
    assert!(support::dump(&app.live().0).contains("esc again to clear input"));
    esc(&mut app, now + Duration::from_millis(100));
    assert!(
        app.take_submissions()
            .iter()
            .any(|submission| matches!(&submission.op, Op::ClearDraft))
    );
    assert!(!support::dump(&app.live().0).contains("Image #"));
    app.paste("new input");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.take_submissions().iter().any(|submission| matches!(&submission.op, Op::UserInput { content, .. } if content == &vec![InputContent::Text { text: "new input".into() }])));
}
#[test]
fn panels_and_bash_escape_do_not_arm_and_rapid_packet_opens_once() {
    let mut app = ready();
    let now = Instant::now();
    app.event(event(EventKind::ApprovalRequested {
        turn_id: None,
        approval_id: ApprovalId("panel".into()),
        request: ApprovalRequest::Rewind {
            panel: fixture(RewindPage::Messages),
        },
    }));
    esc(&mut app, now);
    app.take_submissions();
    esc(&mut app, now);
    assert!(app.take_submissions().is_empty());
    app.paste("!echo draft");
    esc(&mut app, now);
    assert!(!app.escape_pending());
    app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    app.key_at(KeyEvent::new(KeyCode::Esc, KeyModifiers::ALT), now);
    assert_eq!(
        app.take_submissions()
            .iter()
            .filter(|submission| matches!(submission.op, Op::RunCommand { .. }))
            .count(),
        1
    );
}

#[test]
fn detach_with_refilled_images_is_a_command_and_saves_removal_before_opening() {
    let mut app = ready();
    app.event(event(EventKind::ComposerRefilled {
        content: vec![InputContent::Image {
            mime_type: "image/png".into(),
            data: "cG5n".into(),
        }],
    }));
    app.paste("/rewind detach");
    app.take_submissions();
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let submissions = app.take_submissions();
    assert!(matches!(&submissions[0].op, Op::ClearDraft));
    assert!(
        matches!(&submissions[1].op, Op::RunCommand { name, arguments } if name == "rewind" && arguments == "detach")
    );
    assert!(
        !submissions
            .iter()
            .any(|submission| matches!(submission.op, Op::UserInput { .. }))
    );
}

#[test]
fn image_only_refill_submits_payload_without_display_placeholder() {
    let mut app = ready();
    let content = vec![InputContent::Image {
        mime_type: "image/png".into(),
        data: "cG5n".into(),
    }];
    app.event(event(EventKind::ComposerRefilled {
        content: content.clone(),
    }));
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let submissions = app.take_submissions();
    assert!(matches!(&submissions[0].op, Op::UserInput { content: sent, .. } if sent == &content));
}

#[test]
fn editing_refilled_text_never_saves_and_image_removal_clears_once() {
    for images in [false, true] {
        let mut app = ready();
        let mut content = vec![InputContent::Text {
            text: "original".into(),
        }];
        if images {
            content.push(InputContent::Image {
                mime_type: "image/png".into(),
                data: "cG5n".into(),
            });
        }
        app.event(event(EventKind::ComposerRefilled { content }));
        for c in " edits".chars() {
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.paste(" pasted text");
        assert!(
            app.take_submissions().is_empty(),
            "typing and pasting must not move the leaf"
        );
        // Deleting an actual image token clears saved attachments, not the text edits.
        if images {
            app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
            for _ in 0.." edits pasted text".len() {
                app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
            }
            assert!(app.take_submissions().is_empty());
            app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
            assert!(matches!(&app.take_submissions()[0].op, Op::ClearDraft));
            app.paste("more typing");
            app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(app.take_submissions().is_empty());
        } else {
            app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert!(app.take_submissions().is_empty());
        }
    }
}

#[test]
fn reload_restore_preserves_nonempty_composer_but_explicit_refill_replaces_it() {
    let mut app = ready();
    app.paste("unsent local text");
    app.event(Event {
        epoch: SessionEpoch(1),
        ..event(EventKind::ThreadStarted {
            replay: false,
            thread: Thread {
                id: ThreadId("session".into()),
                epoch: SessionEpoch(1),
                turns: vec![],
            },
        })
    });
    let content = vec![
        InputContent::Text {
            text: "saved prompt".into(),
        },
        InputContent::Image {
            mime_type: "image/png".into(),
            data: "cG5n".into(),
        },
    ];
    app.event(Event {
        epoch: SessionEpoch(1),
        ..event(EventKind::ComposerRestored {
            content: content.clone(),
        })
    });
    let text = support::dump(&app.live().0);
    assert!(text.contains("unsent local text"));
    assert!(!text.contains("saved prompt"));
    assert!(!text.contains("Image #"));
    app.event(Event {
        epoch: SessionEpoch(1),
        ..event(EventKind::ComposerRefilled { content })
    });
    let text = support::dump(&app.live().0);
    assert!(text.contains("saved prompt"));
    assert!(text.contains("Image #"));
    assert!(!text.contains("unsent local text"));
}

#[test]
fn reload_does_not_replay_or_resurrect_command_text_and_branch_navigation_replays() {
    let mut app = ready();
    let turns = vec![Turn {
        started_at: 0,
        ended_at: None,
        id: TurnId("old".into()),
        status: TurnStatus::Completed,
        items: vec![Item::Known(KnownItem::AssistantMessage {
            id: ItemId("old-answer".into()),
            text: "old branch answer".into(),
        })],
    }];
    app.event(event(EventKind::ItemCompleted {
        turn_id: TurnId("old".into()),
        item: turns[0].items[0].clone(),
    }));
    app.take_history();
    app.event(event(EventKind::ComposerRefilled {
        content: vec![InputContent::Text {
            text: "refilled prompt".into(),
        }],
    }));
    esc(&mut app, Instant::now());
    esc(&mut app, Instant::now());
    let typing = Instant::now() - Duration::from_secs(1);
    for (index, c) in "/reload".chars().enumerate() {
        app.key_at(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            typing + Duration::from_millis(index as u64 * 20),
        );
    }
    assert!(app.take_submissions().is_empty());
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        app.take_switch(),
        Some((tiechui_tui::Target::Reload, SessionEpoch(1)))
    );
    app.opened(app.cwd().to_owned(), Default::default(), true, vec![]);
    app.event(Event {
        epoch: SessionEpoch(1),
        ..event(EventKind::ThreadStarted {
            replay: false,
            thread: Thread {
                id: ThreadId("session".into()),
                epoch: SessionEpoch(1),
                turns: turns.clone(),
            },
        })
    });
    assert!(
        app.take_history().is_empty(),
        "reload must not reprint history"
    );
    assert!(!support::dump(&app.live().0).contains("/reload"));
    app.event(Event {
        epoch: SessionEpoch(2),
        ..event(EventKind::ThreadStarted {
            replay: true,
            thread: Thread {
                id: ThreadId("session".into()),
                epoch: SessionEpoch(2),
                turns,
            },
        })
    });
    assert!(support::dump(&app.take_history()).contains("old branch answer"));
}

#[test]
fn idle_bash_escape_clears_the_editor_without_arming_rewind() {
    let mut app = ready();
    app.paste("!echo draft");
    esc(&mut app, Instant::now());
    assert!(!support::dump(&app.live().0).contains("echo draft"));
    assert!(!app.escape_pending());
    assert!(app.take_submissions().is_empty());
}
