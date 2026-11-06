mod support;
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
fn status(app: &mut App, text: Option<&str>) {
    event(
        app,
        EventKind::Extension {
            name: "tiechui.loop.status".into(),
            data: serde_json::json!(text),
        },
    );
}
#[test]
fn loop_confirmation_list_and_status_both_widths_and_palettes() {
    let mut snapshots = vec![];
    for light in [false, true] {
        for width in [40, 80] {
            let mut app = app(light, width);
            event(&mut app, EventKind::CommandOutput { name: "loop".into(), text: "Scheduled 5f3a9c21: every 6 minutes (`*/6 * * * *` local time, 7m rounded to the nearest even period). Session-only; expires after 7 days. Cancel with /cron-delete 5f3a9c21. Running it now.".into() });
            event(&mut app, EventKind::CommandOutput { name: "cron-list".into(), text: "5f3a9c21 · every 6 minutes · session · next 2026-11-03 15:06 +08:00\n  check the deploy and report only changes\na1b2c3d4 · weekdays at 08:57 · durable · next 2026-11-04 08:57 +08:00\n  summarize overnight CI failures".into() });
            status(&mut app, Some("2 scheduled tasks · next 15:06"));
            let mut lines = app.take_history();
            lines.extend(app.live().0);
            snapshots.push(format!(
                "{width} columns / {}\n{}",
                if light { "light" } else { "dark" },
                support::dump(&lines)
            ));
        }
    }
    insta::assert_snapshot!("scheduled_tasks", snapshots.join("\n\n"));
}
#[test]
fn drafts_hold_scheduled_runs_back_while_tasks_exist() {
    let mut app = app(false, 80);
    app.paste("draft");
    assert!(app.take_submissions().is_empty());
    status(&mut app, Some("2 scheduled tasks · next 15:05"));
    assert!(support::dump(&app.live().0).contains("2 scheduled tasks · next 15:05"));
    assert_eq!(
        app.take_submissions()
            .into_iter()
            .map(|s| s.op)
            .collect::<Vec<_>>(),
        [Op::InputState { blocked: true }]
    );
    status(&mut app, None);
    assert!(!support::dump(&app.live().0).contains("scheduled"));
}
