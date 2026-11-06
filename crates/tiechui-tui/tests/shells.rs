#![allow(clippy::unwrap_used)]
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;
use tiechui_protocol::*;
use tiechui_tui::{
    App, Options,
    palette::ColorLevel,
    shells::{Outcome, Panel, indicator},
    theme::Theme,
};

fn task(id: &str, mode: ShellMode, state: ShellState) -> ShellTask {
    ShellTask {
        id: id.into(),
        command: format!("run {id}\nsecond line"),
        cwd: "/project".into(),
        mode,
        state,
        user: false,
        started_at: 0,
        ended_at: Some(102_000),
        exit_code: (!state.active()).then_some(0),
    }
}

fn view(tasks: Vec<ShellTask>, open: bool) -> ShellsView {
    ShellsView {
        revision: 1,
        tasks,
        open,
        detail: None,
    }
}

fn text(lines: &[ratatui_core::text::Line<'_>]) -> String {
    lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

fn key(panel: &mut Panel, code: KeyCode) -> Outcome {
    panel.key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn indicator_counts_only_active_background_tasks() {
    use ShellMode::*;
    use ShellState::*;
    assert_eq!(indicator(&view(vec![], false)), None);
    let mut tasks = vec![
        task("sh1", Background, Running),
        task("sh2", Foreground, Running),
        task("sh3", Background, Completed),
    ];
    assert_eq!(indicator(&view(tasks.clone(), false)).unwrap(), "1 shell");
    tasks.push(task("sh4", Background, Stopping));
    tasks.push(task("sh5", Background, Starting));
    assert_eq!(indicator(&view(tasks, false)).unwrap(), "3 shells");
}

#[test]
fn panel_lists_active_first_and_sends_actions_for_the_selection() {
    use ShellMode::*;
    use ShellState::*;
    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    let mut panel = Panel::new(view(
        vec![
            task("sh1", Background, Failed),
            task("sh2", Foreground, Running),
            task("sh3", Background, Running),
        ],
        true,
    ));
    let screen = text(&panel.render(80, 8, &theme));
    let rows: Vec<_> = screen.lines().collect();
    assert!(rows[1].starts_with("› sh2") && rows[2].contains("sh3") && rows[3].contains("sh1"));
    assert!(
        rows[1].contains("foreground") && rows[1].contains("01:42") && rows[1].contains("run sh2")
    );
    assert!(!screen.contains("second line"));
    assert!(
        matches!(key(&mut panel, KeyCode::Char('b')), Outcome::Send(data) if data == json!({"background":"sh2"}))
    );
    key(&mut panel, KeyCode::Down);
    assert!(
        matches!(key(&mut panel, KeyCode::Char('b')), Outcome::Open),
        "already background"
    );
    assert!(
        matches!(key(&mut panel, KeyCode::Char('x')), Outcome::Send(data) if data == json!({"stop":"sh3"}))
    );
    // Updates keep the selected task, not the selected row.
    panel.update(view(
        vec![
            task("sh0", Background, Running),
            task("sh3", Background, Running),
        ],
        false,
    ));
    assert!(
        matches!(key(&mut panel, KeyCode::Char('x')), Outcome::Send(data) if data == json!({"stop":"sh3"}))
    );
    assert!(
        matches!(key(&mut panel, KeyCode::Enter), Outcome::Send(data) if data == json!({"detail":"sh3"}))
    );
    let mut detail = view(vec![task("sh3", Background, Running)], false);
    detail.detail = Some(ShellDetail {
        id: "sh3".into(),
        output: "building\n\u{1b}[31mdone".into(),
        log_path: Some("/tmp/log".into()),
    });
    panel.update(detail);
    let screen = text(&panel.render(40, 12, &theme));
    for expected in [
        "Shell sh3 · background · running",
        "cwd: /project",
        "$ run sh3",
        "$ second line",
        "building",
        "done",
        "Log: /tmp/log",
        "x stop · Esc back",
    ] {
        assert!(screen.contains(expected), "{expected}\n{screen}");
    }
    assert!(!screen.contains('\u{1b}'));
    assert!(
        matches!(key(&mut panel, KeyCode::Esc), Outcome::Send(data) if data == json!({"close":true}))
    );
    assert!(matches!(key(&mut panel, KeyCode::Esc), Outcome::Close));
}

#[test]
fn indicator_stays_while_ready_and_panel_controls_go_to_the_command() {
    let mut app = App::new(
        Theme::fixture(ColorLevel::TrueColor, false),
        60,
        Options::default(),
    );
    let event = |event| Event {
        submission_id: None,
        thread_id: ThreadId("main".into()),
        epoch: SessionEpoch(0),
        event,
    };
    app.event(event(EventKind::ThreadStarted {
        thread: Thread {
            id: ThreadId("main".into()),
            epoch: SessionEpoch(0),
            turns: vec![],
        },
        replay: false,
    }));
    let shells = |tasks, open| {
        event(EventKind::Extension {
            name: SHELLS_EVENT.into(),
            data: json!(view(tasks, open)),
        })
    };
    app.event(shells(
        vec![task("sh1", ShellMode::Background, ShellState::Running)],
        false,
    ));
    assert!(text(&app.live().0).contains("1 shell · /shells"));
    assert!(!app.context_open());
    app.event(shells(
        vec![task("sh1", ShellMode::Foreground, ShellState::Running)],
        true,
    ));
    assert!(!text(&app.live().0).contains("shell · /shells"));
    assert!(app.context_open());
    app.take_submissions();
    app.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
    let ops: Vec<_> = app.take_submissions().into_iter().map(|s| s.op).collect();
    assert!(
        matches!(&ops[..], [Op::CommandInput { name, data }] if name == "shells" && data == &json!({"background":"sh1"})),
        "{ops:?}"
    );
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.context_open());
}
