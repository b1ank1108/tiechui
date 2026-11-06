use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui_core::text::Line;
use std::time::{Duration, Instant};
use tiechui_tui::selection::{MouseAction, Selection};

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
