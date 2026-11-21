use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui_core::text::Line;
use std::time::{Duration, Instant};
use tiechui_tui::selection::{DocSource, DocView, MouseAction, Selection, Viewport};

const DOWN: MouseEventKind = MouseEventKind::Down(MouseButton::Left);
const DRAG: MouseEventKind = MouseEventKind::Drag(MouseButton::Left);
const UP: MouseEventKind = MouseEventKind::Up(MouseButton::Left);

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn selection(text: &str) -> Selection {
    let mut selection = Selection::default();
    selection.frame(vec![Line::from(text.to_owned())]);
    selection
}

#[test]
fn matching_release_copies_once_and_reselecting_copies_again() {
    let mut selection = selection("copy this text");
    let now = Instant::now();
    for at in [now, now + Duration::from_secs(1)] {
        selection.mouse(mouse(UP, 9, 0), at, true);
        selection.mouse(mouse(DRAG, 9, 0), at, true);
        assert!(selection.take_copy().is_none());
        selection.mouse(mouse(DOWN, 0, 0), at, true);
        assert!(selection.take_copy().is_none());
        selection.mouse(mouse(DRAG, 4, 0), at, true);
        selection.mouse(
            mouse(MouseEventKind::Up(MouseButton::Right), 9, 0),
            at,
            true,
        );
        assert!(selection.take_copy().is_none());
        selection.mouse(mouse(UP, 9, 0), at, true);
        assert_eq!(selection.take_copy().as_deref(), Some("copy this"));
        assert!(selection.active());
        selection.mouse(mouse(UP, 13, 0), at, true);
        selection.frame(vec![Line::from("copy this text")]);
        assert_eq!(selection.selected_text().as_deref(), Some("copy this"));
        assert!(selection.take_copy().is_none());
    }
}

#[test]
fn release_coordinates_include_motion_without_a_drag_event_and_preserve_padding() {
    let mut selection = selection("  copy this");
    let now = Instant::now();
    selection.mouse(mouse(DOWN, 0, 0), now, true);
    selection.mouse(mouse(UP, 6, 0), now, true);
    assert_eq!(selection.take_copy().as_deref(), Some("  copy"));
}

#[test]
fn reverse_release_uses_frozen_wide_combining_and_emoji_graphemes() {
    let mut selection = selection("  中e\u{301}👩‍💻 done");
    let now = Instant::now();
    selection.mouse(mouse(DOWN, 7, 0), now, true);
    selection.mouse(mouse(DRAG, 4, 0), now, true);
    selection.frame(vec![Line::from("stream replaced this row")]);
    selection.mouse(mouse(UP, 3, 0), now, true);
    assert_eq!(selection.take_copy().as_deref(), Some("中e\u{301}👩‍💻"));
}

#[test]
fn stationary_double_and_triple_releases_copy_the_full_word_and_line() {
    let mut selection = selection("one two three");
    let now = Instant::now();
    for (click, expected) in [(0, None), (1, Some("two")), (2, Some("one two three"))] {
        let at = now + Duration::from_millis(click * 50);
        selection.mouse(mouse(DOWN, 5, 0), at, true);
        assert!(selection.take_copy().is_none());
        let action = selection.mouse(mouse(UP, 5, 0), at, true);
        if click == 0 {
            assert!(matches!(action, MouseAction::Click(_)));
        }
        assert_eq!(selection.take_copy().as_deref(), expected);
        selection.mouse(mouse(UP, 5, 0), at, true);
        assert!(selection.take_copy().is_none());
    }
}

#[test]
fn blank_selections_never_copy_even_explicitly() {
    let mut selection = selection(" \t  ");
    let now = Instant::now();
    selection.mouse(mouse(DOWN, 0, 0), now, true);
    selection.mouse(mouse(DRAG, 20, 0), now, true);
    selection.mouse(mouse(UP, 20, 0), now, true);
    assert!(selection.take_copy().is_none());
    selection.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(selection.take_copy().is_none());
}

#[test]
fn clearing_scrolling_and_escape_cancel_gestures_and_discard_pending_copies() {
    let now = Instant::now();
    for released in [false, true] {
        for cancel in 0..4 {
            let mut selection = selection("copy this");
            selection.mouse(mouse(DOWN, 0, 0), now, true);
            selection.mouse(mouse(DRAG, 4, 0), now, true);
            if released {
                selection.mouse(mouse(UP, 4, 0), now, true);
            }
            match cancel {
                0 => selection.clear(),
                1 => {
                    selection.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                }
                2 => {
                    selection.mouse(mouse(MouseEventKind::ScrollUp, 0, 0), now, true);
                }
                _ => {
                    selection.mouse(mouse(MouseEventKind::ScrollDown, 0, 0), now, true);
                }
            }
            selection.mouse(mouse(UP, 4, 0), now, true);
            assert!(!selection.active());
            assert!(selection.take_copy().is_none());
        }
    }
}

#[test]
fn disabled_auto_copy_and_keyboard_adjustments_require_explicit_copy() {
    let now = Instant::now();
    for enabled in [false, true] {
        let mut selection = selection("copy this");
        for (kind, column) in [(DOWN, 0), (DRAG, 3), (UP, 4)] {
            selection.mouse(mouse(kind, column, 0), now, enabled);
        }
        assert_eq!(selection.take_copy().as_deref(), enabled.then_some("copy"));
        selection.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(selection.take_copy().is_none());
        for key in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('C'), KeyModifiers::SUPER),
        ] {
            assert!(selection.key(key));
            assert_eq!(selection.take_copy().as_deref(), Some("cop"));
        }
        selection.clear();
        assert!(!selection.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }
}

#[test]
fn document_selection_scroll_drag_extends_across_offsets_and_copies_once_on_release() {
    let doc: Vec<Line<'static>> = (0..20)
        .map(|n| Line::from(format!("L{n:02} 中e\u{301}👩‍💻 row")))
        .collect();
    let viewport = Viewport {
        top: 0,
        height: 5,
        rows: 8,
    };
    let mut offset = 0usize;
    let max_offset = doc.len() - viewport.height;
    let mut selection = Selection::default();
    let now = Instant::now();

    let view = |offset| DocView {
        source: DocSource::Main,
        viewport,
        offset,
    };

    let action = selection.doc_mouse(
        mouse(DOWN, 4, 1),
        now,
        true,
        view(offset),
        || doc.clone(),
        |_| (viewport, offset),
    );
    assert!(matches!(action, MouseAction::Handled));
    assert!(selection.active());
    assert!(selection.take_copy().is_none());

    for _ in 0..3 {
        let action = selection.doc_mouse(
            mouse(MouseEventKind::ScrollDown, 9, 3),
            now,
            true,
            view(offset),
            || panic!("snapshot must not clone again during drag"),
            |delta| {
                offset = offset.saturating_add_signed(delta).min(max_offset);
                (viewport, offset)
            },
        );
        assert!(matches!(action, MouseAction::Handled));
        assert!(selection.take_copy().is_none());
    }
    assert_eq!(offset, 9);

    let slice = selection.highlight_slice(doc[offset..offset + viewport.height].to_vec(), offset);
    assert!(slice[0].spans.iter().any(|s| {
        s.style
            .add_modifier
            .contains(ratatui_core::style::Modifier::REVERSED)
    }));

    selection.doc_mouse(
        mouse(UP, 9, 3),
        now,
        true,
        view(offset),
        || panic!("snapshot must not clone on release"),
        |_| (viewport, offset),
    );
    let copied = selection.take_copy().expect("copied cross-screen slice");
    assert!(copied.starts_with("中e\u{301}👩‍💻 row\nL02 "));
    assert!(copied.ends_with("\nL12 中e\u{301}👩‍💻"));
    assert!(selection.take_copy().is_none());

    selection.doc_mouse(
        mouse(MouseEventKind::ScrollDown, 0, 2),
        now,
        true,
        view(offset),
        || panic!("snapshot must not clone after release"),
        |delta| {
            offset = offset.saturating_add_signed(delta).min(max_offset);
            (viewport, offset)
        },
    );
    assert_eq!(offset, 12);
    assert!(selection.active());
    assert!(selection.take_copy().is_none());

    let shift_down = MouseEvent {
        kind: DOWN,
        column: 13,
        row: 4,
        modifiers: KeyModifiers::SHIFT,
    };
    selection.doc_mouse(
        shift_down,
        now + Duration::from_secs(1),
        true,
        view(offset),
        || panic!("Shift+Click must reuse frozen snapshot"),
        |_| (viewport, offset),
    );
    assert!(selection.take_copy().is_none());
    selection.doc_mouse(
        mouse(UP, 13, 4),
        now + Duration::from_secs(1),
        true,
        view(offset),
        || panic!("Shift+Click release must reuse frozen snapshot"),
        |_| (viewport, offset),
    );
    let extended = selection.take_copy().expect("copied extended slice");
    assert!(extended.starts_with("中e\u{301}👩‍💻 row\nL02 "));
    assert!(extended.ends_with("\nL16 中e\u{301}👩‍💻 row"));
}

#[test]
fn document_selection_ignores_outside_presses_and_reports_edge_autoscroll() {
    let doc: Vec<Line<'static>> = (0..16)
        .map(|n| Line::from(format!("ROW_{n:02}_END")))
        .collect();
    let pager_vp = Viewport {
        top: 1,
        height: 4,
        rows: 8,
    };
    let pager_view = |offset| DocView {
        source: DocSource::Pager,
        viewport: pager_vp,
        offset,
    };
    let mut selection = Selection::default();
    let now = Instant::now();

    for row in [0, 6] {
        let action = selection.doc_mouse(
            mouse(DOWN, 2, row),
            now,
            true,
            pager_view(4),
            || doc.clone(),
            |_| (pager_vp, 4),
        );
        assert!(matches!(action, MouseAction::Pass));
        assert!(!selection.active());
    }

    selection.doc_mouse(
        mouse(DOWN, 6, 3),
        now,
        true,
        pager_view(4),
        || doc.clone(),
        |_| (pager_vp, 4),
    );
    selection.doc_mouse(
        mouse(DRAG, 6, 0),
        now,
        true,
        pager_view(4),
        || doc.clone(),
        |_| (pager_vp, 4),
    );
    assert_eq!(selection.autoscroll_step(pager_vp, 4, 12), Some(-2));
    assert_eq!(selection.autoscroll_step(pager_vp, 0, 12), None);
    selection.apply_autoscroll(pager_vp, 0);
    selection.doc_mouse(
        mouse(UP, 6, 0),
        now,
        true,
        pager_view(0),
        || doc.clone(),
        |_| (pager_vp, 0),
    );
    let copied = selection.take_copy().expect("copied upward to top");
    assert!(copied.starts_with("ROW_00_END\n"));
    assert!(copied.ends_with("\nROW_06"));

    let main_vp = Viewport {
        top: 0,
        height: 5,
        rows: 8,
    };
    let main_view = |offset| DocView {
        source: DocSource::Main,
        viewport: main_vp,
        offset,
    };
    selection.doc_mouse(
        mouse(DOWN, 1, 0),
        now + Duration::from_secs(1),
        true,
        main_view(4),
        || doc.clone(),
        |_| (main_vp, 4),
    );
    selection.doc_mouse(
        mouse(DRAG, 5, 0),
        now + Duration::from_secs(1),
        true,
        main_view(4),
        || doc.clone(),
        |_| (main_vp, 4),
    );
    assert_eq!(selection.autoscroll_step(main_vp, 4, 11), None);

    selection.doc_mouse(
        mouse(DRAG, 5, 2),
        now + Duration::from_secs(1),
        true,
        main_view(4),
        || doc.clone(),
        |_| (main_vp, 4),
    );
    selection.doc_mouse(
        mouse(DRAG, 0, 0),
        now + Duration::from_secs(1),
        true,
        main_view(4),
        || doc.clone(),
        |_| (main_vp, 4),
    );
    assert_eq!(selection.autoscroll_step(main_vp, 4, 11), Some(-2));
}
