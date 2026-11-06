mod support;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tiechui_protocol::*;
use tiechui_tui::{App, Options, palette::ColorLevel, theme::Theme};
fn event(app: &mut App, kind: EventKind) {
    app.event(Event {
        submission_id: None,
        thread_id: ThreadId("thread".into()),
        epoch: SessionEpoch(0),
        event: kind,
    });
}
fn app(light: bool, width: usize) -> App {
    let mut app = App::new(
        Theme::fixture(ColorLevel::TrueColor, light),
        width,
        Options::default(),
    );
    event(
        &mut app,
        EventKind::ThreadStarted {
            replay: false,
            thread: Thread {
                epoch: SessionEpoch(0),
                id: ThreadId("thread".into()),
                turns: vec![],
            },
        },
    );
    app
}
#[test]
fn goal_summary_and_status_both_widths_and_palettes() {
    let mut snapshots = vec![];
    for light in [false, true] {
        for width in [40, 80] {
            let mut app = app(light, width);
            event(&mut app, EventKind::CommandOutput { name: "goal".into(), text: "Goal\nStatus: active\nObjective: Verify the local artifacts\nTime used: 1m 04s\nTokens used: 1.2K\n\n/goal edit · /goal pause · /goal clear".into() });
            event(
                &mut app,
                EventKind::Extension {
                    name: "tiechui.goal.status".into(),
                    data: serde_json::json!("Pursuing goal (1.2K tokens · 1m 04s)"),
                },
            );
            let mut lines = app.take_history();
            lines.extend(app.live().0);
            snapshots.push(format!(
                "{width} columns / {}\n{}",
                if light { "light" } else { "dark" },
                support::dump(&lines)
            ));
        }
    }
    insta::assert_snapshot!("goal_summary_status", snapshots.join("\n\n"));
}
#[test]
fn editor_uses_composer_and_cancel_never_submits_the_draft() {
    let mut app = app(false, 80);
    let prompt = |text: &str| EventKind::ApprovalRequested {
        turn_id: None,
        approval_id: ApprovalId("editor".into()),
        request: ApprovalRequest::Editor {
            title: "Edit goal".into(),
            text: text.into(),
        },
    };
    event(&mut app, prompt("original"));
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    app.paste("updated");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.take_submissions().iter().any(|s| matches!(&s.op, Op::ApprovalResponse { decision: ApprovalDecision::Accept { content: Some(v) }, .. } if v == "updated")));
    event(&mut app, prompt("keep"));
    app.paste("discard");
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.take_submissions().iter().any(|s| matches!(
        &s.op,
        Op::ApprovalResponse {
            decision: ApprovalDecision::Cancel,
            ..
        }
    )));
}
#[test]
fn main_drafts_publish_readiness_and_idle_escape_interrupts_module_work() {
    let mut app = app(false, 80);
    event(
        &mut app,
        EventKind::Extension {
            name: "tiechui.goal.status".into(),
            data: serde_json::json!("Pursuing goal (0 tokens · 0s)"),
        },
    );
    app.paste("draft");
    assert!(
        app.take_submissions()
            .iter()
            .any(|s| s.op == Op::InputState { blocked: true })
    );
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        app.take_submissions()
            .iter()
            .any(|s| s.op == Op::InterruptIdle)
    );
}
