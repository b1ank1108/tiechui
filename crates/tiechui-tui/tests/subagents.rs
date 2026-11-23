#![allow(clippy::unwrap_used)]
mod support;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;
use tiechui_protocol::*;
use tiechui_tui::{
    palette::ColorLevel,
    subagents::{Outcome, Panel},
    theme::Theme,
    tools::{Tool, View},
};

fn records() -> Vec<AgentDispatch> {
    vec![AgentDispatch {
        id: "dispatch".into(),
        session_id: "main".into(),
        mode: "parallel".into(),
        background: false,
        started_at: 0,
        tasks: vec![
            AgentTask {
                id: String::new(),
                name: None,
                background: false,
                fork: false,
                started_at: 0,
                agent: "default".into(),
                task: "Update the parser 👩‍💻 中文".into(),
                state: AgentState::Completed,
                output: "Updated parser.rs.\nTests pass.".into(),
                thinking: "Check boundaries before editing.".into(),
                diagnostic: None,
                output_path: None,
                thinking_path: None,
                usage: TokenUsage {
                    input: 120,
                    output: 30,
                    ..Default::default()
                },
                turns: 2,
                model: "alpha/fast".into(),
            },
            AgentTask {
                id: String::new(),
                name: None,
                background: false,
                fork: false,
                started_at: 0,
                agent: "reviewer".into(),
                task: "Review parser changes".into(),
                state: AgentState::Cancelled,
                output: "Finished reviewing the first file.".into(),
                thinking: String::new(),
                diagnostic: None,
                output_path: None,
                thinking_path: None,
                usage: TokenUsage::default(),
                turns: 0,
                model: "beta/judge".into(),
            },
        ],
    }]
}
fn key(panel: &mut Panel, code: KeyCode, modifiers: KeyModifiers) -> Outcome {
    panel.key(KeyEvent::new(code, modifiers))
}

#[test]
fn main_previews_fit_available_columns_without_splitting_graphemes() {
    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    for width in [40, 100] {
        for grapheme in ["e\u{301}", "👩‍💻", "中"] {
            let columns = tiechui_tui::wrap::width(grapheme);
            let source = grapheme.repeat(width + 1);
            let mut record = records().remove(0);
            record.tasks.truncate(1);
            record.tasks[0].task = source.clone();
            record.tasks[0].output = source;
            let mut tool = Tool::new("dispatch".into(), "subagent".into(), json!({}));
            tool.output = Some(json!({"details": record}));
            let agent = tiechui_tui::wrap::width(&record.tasks[0].agent);
            let lines = tiechui_tui::tools::render(&[tool], width, &theme, View::History);
            assert_eq!(lines.len(), 3);
            let fit = |columns_left: usize| format!("{}…", grapheme.repeat(columns_left / columns));
            assert!(lines[1].to_string().ends_with(&fit(width - agent - 9)));
            assert_eq!(lines[2].to_string(), format!("      {}", fit(width - 7)));
            assert!(lines.iter().all(|line| line.width() <= width));
        }
    }
}
#[test]
fn main_and_viewer_at_both_widths_and_palettes() {
    let mut snapshots = vec![];
    for light in [false, true] {
        for width in [40, 80] {
            let theme = Theme::fixture(ColorLevel::TrueColor, light);
            let mut tool = Tool::new("dispatch".into(), "subagent".into(), json!({}));
            tool.partial = Some(json!({"details":records()[0]}));
            snapshots.push(format!(
                "{width} / light={light} / waiting\n{}",
                support::dump(&tiechui_tui::tools::render(
                    &[tool.clone()],
                    width,
                    &theme,
                    View::History
                ))
            ));
            tool.output = tool.partial.take();
            snapshots.push(format!(
                "{width} / light={light} / finished\n{}",
                support::dump(&tiechui_tui::tools::render(
                    &[tool],
                    width,
                    &theme,
                    View::Transcript
                ))
            ));
            let mut panel = Panel::new(records());
            key(&mut panel, KeyCode::Right, KeyModifiers::NONE);
            snapshots.push(format!(
                "{width} / light={light} / viewer\n{}",
                support::dump(&panel.render(width, 14, &theme, &[], true))
            ));
            key(&mut panel, KeyCode::Enter, KeyModifiers::NONE);
            key(&mut panel, KeyCode::Char('t'), KeyModifiers::CONTROL);
            snapshots.push(format!(
                "{width} / light={light} / output\n{}",
                support::dump(&panel.render(width, 14, &theme, &[], false))
            ));
        }
    }
    insta::assert_snapshot!("subagents", snapshots.join("\n\n"));
}
#[test]
fn viewer_search_by_model_group_thinking_cancel_and_navigation() {
    let mut active = records();
    active[0].tasks[0].state = AgentState::Running;
    let mut panel = Panel::new(active);
    key(&mut panel, KeyCode::Right, KeyModifiers::NONE);
    assert!(
        matches!(key(&mut panel, KeyCode::Char('x'), KeyModifiers::CONTROL), Outcome::Send(data) if data == json!({"stop": "dispatch"}))
    );
    let mut panel = Panel::new(records());
    key(&mut panel, KeyCode::Right, KeyModifiers::NONE);
    assert!(matches!(
        key(&mut panel, KeyCode::Char('x'), KeyModifiers::CONTROL),
        Outcome::Open
    ));
    key(&mut panel, KeyCode::Char('f'), KeyModifiers::CONTROL);
    for c in "beta/judge".chars() {
        key(&mut panel, KeyCode::Char(c), KeyModifiers::NONE);
    }
    key(&mut panel, KeyCode::Enter, KeyModifiers::NONE);
    key(&mut panel, KeyCode::Right, KeyModifiers::NONE);
    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    let screen = support::dump(&panel.render(80, 14, &theme, &[], true));
    assert!(
        screen.contains("reviewer")
            && screen.contains("beta/judge")
            && !screen.contains("alpha/fast"),
        "{screen}"
    );
    key(&mut panel, KeyCode::Enter, KeyModifiers::NONE);
    let screen = support::dump(&panel.render(80, 14, &theme, &[], true));
    assert!(
        screen.contains("beta/judge") && screen.contains("Finished reviewing"),
        "{screen}"
    );
    key(&mut panel, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut panel, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(matches!(
        key(&mut panel, KeyCode::Enter, KeyModifiers::NONE),
        Outcome::Close
    ));
}

#[test]
fn indicator_counts_active_background_agents_and_updates_status_bar() {
    use tiechui_tui::{App, Options, subagents::indicator};
    let task = |id: &str, background: bool, state: AgentState| AgentTask {
        id: id.into(),
        name: None,
        background,
        fork: false,
        started_at: 0,
        agent: "default".into(),
        task: format!("task {id}"),
        state,
        output: String::new(),
        thinking: String::new(),
        diagnostic: None,
        output_path: None,
        thinking_path: None,
        usage: TokenUsage::default(),
        turns: 0,
        model: "alpha/fast".into(),
    };
    let view = |tasks: Vec<AgentTask>| SubagentsView {
        records: vec![AgentDispatch {
            id: "d1".into(),
            session_id: "main".into(),
            mode: "parallel".into(),
            background: true,
            started_at: 0,
            tasks,
        }],
        open: false,
        pending_notifications: 0,
    };
    assert_eq!(indicator(&view(vec![])), None);
    let one = view(vec![
        task("ag1", true, AgentState::Running),
        task("ag2", false, AgentState::Running),
        task("ag3", true, AgentState::Completed),
    ]);
    assert_eq!(indicator(&one).as_deref(), Some("1 agent"));
    let two = view(vec![
        task("ag1", true, AgentState::Running),
        task("ag2", true, AgentState::Waiting),
    ]);
    assert_eq!(indicator(&two).as_deref(), Some("2 agents"));

    let mut app = App::new(
        Theme::fixture(ColorLevel::TrueColor, false),
        80,
        Options::default(),
    );
    let ev = |event| Event {
        submission_id: None,
        thread_id: ThreadId("main".into()),
        epoch: SessionEpoch(0),
        event,
    };
    app.event(ev(EventKind::ThreadStarted {
        thread: Thread {
            id: ThreadId("main".into()),
            epoch: SessionEpoch(0),
            turns: vec![],
        },
        replay: false,
    }));
    app.event(ev(EventKind::Extension {
        name: SUBAGENTS_EVENT.into(),
        data: json!(one),
    }));
    app.paste("draft");
    assert_eq!(
        app.take_submissions()
            .into_iter()
            .map(|s| s.op)
            .collect::<Vec<_>>(),
        [Op::InputState { blocked: true }]
    );
    app.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(
        app.take_submissions()
            .into_iter()
            .map(|s| s.op)
            .collect::<Vec<_>>(),
        [Op::InputState { blocked: false }]
    );
    let live_text = app
        .live()
        .0
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(live_text.contains("1 agent · /subagents"), "{live_text}");
    app.event(ev(EventKind::Extension {
        name: SHELLS_EVENT.into(),
        data: json!(ShellsView {
            revision: 1,
            tasks: vec![ShellTask {
                id: "sh1".into(),
                command: "sleep 10".into(),
                cwd: "/project".into(),
                mode: ShellMode::Background,
                state: ShellState::Running,
                user: false,
                started_at: 0,
                ended_at: None,
                exit_code: None,
            }],
            open: false,
            detail: None,
        }),
    }));
    let both = app
        .live()
        .0
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        both.contains("1 shell · /shells   1 agent · /subagents"),
        "{both}"
    );
    app.event(ev(EventKind::Extension {
        name: SHELLS_EVENT.into(),
        data: json!(ShellsView {
            revision: 2,
            tasks: vec![],
            open: false,
            detail: None,
        }),
    }));
    app.resize(18, 24);
    let narrow = app
        .live()
        .0
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        narrow.contains("1 agent") && !narrow.contains("/subagents"),
        "{narrow}"
    );
    app.resize(80, 24);
    app.event(ev(EventKind::Extension {
        name: SUBAGENTS_EVENT.into(),
        data: json!(two),
    }));
    let live_two = app
        .live()
        .0
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(live_two.contains("2 agents · /subagents"), "{live_two}");
    app.event(ev(EventKind::Extension {
        name: SUBAGENTS_EVENT.into(),
        data: json!(view(vec![task("ag1", true, AgentState::Completed)])),
    }));
    let settled = app
        .live()
        .0
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!settled.contains("agent"), "{settled}");
}

#[test]
fn background_cells_two_second_hint_task_ids_and_subagent_tools_render() {
    use ratatui_core::style::Modifier;
    use std::time::{Duration, Instant};

    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    let t0 = Instant::now();
    let mut dispatch = records().remove(0);
    dispatch.background = false;
    dispatch.tasks = vec![AgentTask {
        id: "ag1".into(),
        name: Some("worker".into()),
        background: false,
        fork: false,
        started_at: 0,
        agent: "default".into(),
        task: "Inspect workspace".into(),
        state: AgentState::Running,
        output: String::new(),
        thinking: String::new(),
        diagnostic: None,
        output_path: None,
        thinking_path: None,
        usage: TokenUsage::default(),
        turns: 0,
        model: "alpha/fast".into(),
    }];

    let mut live_tool = Tool::new("call-1".into(), "subagent".into(), json!({}));
    live_tool.started = Some(t0);
    live_tool.partial = Some(json!({"details": dispatch}));
    let early = tiechui_tui::tools::render(
        std::slice::from_ref(&live_tool),
        80,
        &theme,
        View::Live {
            phase: Duration::ZERO,
            now: t0 + Duration::from_millis(1999),
        },
    );
    let early_str = early[0].to_string();
    assert!(early_str.contains("Delegating"), "{early_str}");
    assert!(!early_str.contains("ctrl+b background"), "{early_str}");
    assert!(
        early[1].to_string().contains("ag1 (worker) · default"),
        "{}",
        early[1]
    );

    let after_2s = tiechui_tui::tools::render(
        std::slice::from_ref(&live_tool),
        80,
        &theme,
        View::Live {
            phase: Duration::ZERO,
            now: t0 + Duration::from_secs(2),
        },
    );
    let after_str = after_2s[0].to_string();
    assert!(
        after_str.contains("Delegating") && after_str.contains("ctrl+b background"),
        "{after_str}"
    );
    let history = tiechui_tui::tools::render(&[live_tool], 80, &theme, View::History);
    assert!(!history[0].to_string().contains("ctrl+b background"));

    dispatch.background = true;
    dispatch.tasks[0].background = true;
    dispatch.tasks[0].name = None;
    let mut bg_tool = Tool::new("call-1".into(), "subagent".into(), json!({}));
    bg_tool.output = Some(json!({"details": dispatch.clone()}));
    let bg_lines = tiechui_tui::tools::render(&[bg_tool], 80, &theme, View::History);
    assert!(
        bg_lines[0].to_string().contains("Delegated in background"),
        "{}",
        bg_lines[0]
    );
    assert_eq!(
        bg_lines[0].spans[0].style,
        theme.accent.add_modifier(Modifier::BOLD)
    );
    assert!(
        bg_lines[1].to_string().contains("ag1 · default"),
        "{}",
        bg_lines[1]
    );

    let mut send_resume = Tool::new(
        "send-1".into(),
        "subagent_send".into(),
        json!({"to": "ag1", "message": "continue"}),
    );
    send_resume.output = Some(json!({"details": dispatch}));
    let send_lines = tiechui_tui::tools::render(&[send_resume], 80, &theme, View::History);
    assert!(
        send_lines[0]
            .to_string()
            .contains("Delegated in background")
    );

    for (name, args, text) in [
        (
            "subagent_send",
            json!({"to": "ag1", "message": "hi"}),
            "Queued message for ag1.",
        ),
        ("subagent_stop", json!({"id": "ag1"}), "Stopped ag1."),
        ("subagent_list", json!({}), "ag1  background  running"),
    ] {
        let mut plain_tool = Tool::new(name.into(), name.into(), args);
        plain_tool.output =
            Some(json!({"content": [{"kind": "text", "text": text}], "is_error": false}));
        let rendered = tiechui_tui::tools::render(&[plain_tool], 80, &theme, View::History)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains(&format!("Called {name}")), "{rendered}");
        assert!(rendered.contains(text), "{rendered}");
    }
}

#[test]
fn panel_supports_background_and_stop_shortcuts_and_routes_command_input() {
    use tiechui_tui::{App, Options};
    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    let dispatches = vec![AgentDispatch {
        id: "d1".into(),
        session_id: "main".into(),
        mode: "parallel".into(),
        background: false,
        started_at: 0,
        tasks: vec![
            AgentTask {
                id: "ag1".into(),
                name: Some("fg".into()),
                background: false,
                fork: false,
                started_at: 0,
                agent: "default".into(),
                task: "foreground work".into(),
                state: AgentState::Running,
                output: String::new(),
                thinking: String::new(),
                diagnostic: None,
                output_path: None,
                thinking_path: None,
                usage: TokenUsage::default(),
                turns: 0,
                model: "alpha/fast".into(),
            },
            AgentTask {
                id: "ag2".into(),
                name: None,
                background: true,
                fork: true,
                started_at: 0,
                agent: "fork".into(),
                task: "background fork".into(),
                state: AgentState::Waiting,
                output: String::new(),
                thinking: String::new(),
                diagnostic: None,
                output_path: None,
                thinking_path: None,
                usage: TokenUsage::default(),
                turns: 0,
                model: "alpha/fast".into(),
            },
        ],
    }];
    let mut panel = Panel::new(dispatches.clone());
    assert!(matches!(
        key(&mut panel, KeyCode::Char('b'), KeyModifiers::NONE),
        Outcome::Open
    ));
    assert!(matches!(
        key(&mut panel, KeyCode::Char('x'), KeyModifiers::NONE),
        Outcome::Open
    ));

    key(&mut panel, KeyCode::Down, KeyModifiers::NONE);
    let fg_screen = panel
        .render(80, 14, &theme, &[], false)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(fg_screen.contains("ag1 (fg) · default"), "{fg_screen}");
    assert!(fg_screen.contains("b background"), "{fg_screen}");
    assert!(fg_screen.contains("stop"), "{fg_screen}");
    assert!(
        matches!(key(&mut panel, KeyCode::Char('b'), KeyModifiers::NONE), Outcome::Send(v) if v == json!({"background": "ag1"}))
    );
    assert!(
        matches!(key(&mut panel, KeyCode::Char('x'), KeyModifiers::NONE), Outcome::Send(v) if v == json!({"stop": "ag1"}))
    );
    assert!(
        matches!(key(&mut panel, KeyCode::Char('x'), KeyModifiers::CONTROL), Outcome::Send(v) if v == json!({"stop": "ag1"}))
    );

    key(&mut panel, KeyCode::Enter, KeyModifiers::NONE);
    assert!(
        matches!(key(&mut panel, KeyCode::Char('b'), KeyModifiers::NONE), Outcome::Send(v) if v == json!({"background": "ag1"}))
    );
    assert!(
        matches!(key(&mut panel, KeyCode::Char('x'), KeyModifiers::CONTROL), Outcome::Send(v) if v == json!({"stop": "ag1"}))
    );
    key(&mut panel, KeyCode::Esc, KeyModifiers::NONE);

    key(&mut panel, KeyCode::Down, KeyModifiers::NONE);
    let bg_screen = panel
        .render(80, 14, &theme, &[], false)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!bg_screen.contains("b background"), "{bg_screen}");
    assert!(matches!(
        key(&mut panel, KeyCode::Char('b'), KeyModifiers::NONE),
        Outcome::Open
    ));
    assert!(
        matches!(key(&mut panel, KeyCode::Char('x'), KeyModifiers::NONE), Outcome::Send(v) if v == json!({"stop": "ag2"}))
    );

    let mut app = App::new(theme, 80, Options::default());
    let ev = |event| Event {
        submission_id: None,
        thread_id: ThreadId("main".into()),
        epoch: SessionEpoch(0),
        event,
    };
    app.event(ev(EventKind::ThreadStarted {
        thread: Thread {
            id: ThreadId("main".into()),
            epoch: SessionEpoch(0),
            turns: vec![],
        },
        replay: false,
    }));
    app.event(ev(EventKind::Extension {
        name: SUBAGENTS_EVENT.into(),
        data: json!(SubagentsView {
            records: dispatches,
            open: true,
            pending_notifications: 0,
        }),
    }));
    app.take_submissions();
    app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    app.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE));
    app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    let ops: Vec<_> = app.take_submissions().into_iter().map(|s| s.op).collect();
    assert_eq!(
        ops,
        vec![
            Op::CommandInput {
                name: "subagents".into(),
                data: json!({"background": "ag1"}),
            },
            Op::CommandInput {
                name: "subagents".into(),
                data: json!({"stop": "ag1"}),
            },
        ]
    );
}

#[test]
fn background_and_active_viewer_at_both_widths_and_palettes() {
    use std::time::{Duration, Instant};

    let t0 = Instant::now();
    let dispatch = AgentDispatch {
        id: "d-bg".into(),
        session_id: "main".into(),
        mode: "parallel".into(),
        background: true,
        started_at: 0,
        tasks: vec![
            AgentTask {
                id: "ag1".into(),
                name: Some("audit".into()),
                background: false,
                fork: false,
                started_at: 0,
                agent: "default".into(),
                task: "Audit parser edge cases 👩‍💻 中文".into(),
                state: AgentState::Running,
                output: "Checking lexer...".into(),
                thinking: String::new(),
                diagnostic: None,
                output_path: None,
                thinking_path: None,
                usage: TokenUsage::default(),
                turns: 1,
                model: "alpha/fast".into(),
            },
            AgentTask {
                id: "ag2".into(),
                name: None,
                background: true,
                fork: true,
                started_at: 0,
                agent: "fork".into(),
                task: "Verify cache prefix".into(),
                state: AgentState::Waiting,
                output: String::new(),
                thinking: String::new(),
                diagnostic: None,
                output_path: None,
                thinking_path: None,
                usage: TokenUsage::default(),
                turns: 0,
                model: "beta/judge".into(),
            },
        ],
    };
    let mut snapshots = Vec::new();
    for light in [false, true] {
        for width in [40, 80] {
            let theme = Theme::fixture(ColorLevel::TrueColor, light);
            let mut live = Tool::new("d-bg".into(), "subagent".into(), json!({}));
            live.started = Some(t0);
            live.partial = Some(json!({"details": dispatch}));
            snapshots.push(format!(
                "{width} / light={light} / live_2s\n{}",
                support::dump(&tiechui_tui::tools::render(
                    &[live],
                    width,
                    &theme,
                    View::Live {
                        phase: Duration::ZERO,
                        now: t0 + Duration::from_secs(2),
                    },
                ))
            ));
            let mut bg_done = Tool::new("d-bg".into(), "subagent".into(), json!({}));
            bg_done.output = Some(json!({"details": dispatch}));
            snapshots.push(format!(
                "{width} / light={light} / background\n{}",
                support::dump(&tiechui_tui::tools::render(
                    &[bg_done],
                    width,
                    &theme,
                    View::History,
                ))
            ));
            let mut panel = Panel::new(vec![dispatch.clone()]);
            key(&mut panel, KeyCode::Down, KeyModifiers::NONE);
            snapshots.push(format!(
                "{width} / light={light} / active_viewer\n{}",
                support::dump(&panel.render(width, 12, &theme, &[], false))
            ));
        }
    }
    insta::assert_snapshot!("subagents_background", snapshots.join("\n\n"));
}

#[test]
fn task_lines_omit_trailing_model_separator_when_model_is_empty() {
    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    let mut task = records().remove(0).tasks.remove(1);
    task.model.clear();
    let lines = tiechui_tui::subagents::task_lines(&task, 80, &theme, true, false);
    let rendered = support::dump(&lines);
    assert!(rendered.contains("0 turns · 0 in · 0 out"), "{rendered}");
    assert!(!rendered.contains("0 out ·"), "{rendered}");
}

#[test]
fn viewer_defers_parent_approval_and_empty_composer_shortcut_respects_drafts() {
    use tiechui_tui::{App, Options};
    let mut app = App::new(
        Theme::fixture(ColorLevel::TrueColor, false),
        80,
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
    app.event(event(EventKind::CommandsUpdated {
        commands: vec![CommandInfo {
            name: "subagents".into(),
            description: "Viewer".into(),
            argument_hint: None,
            kind: Default::default(),
        }],
    }));
    app.paste("MAIN_DRAFT");
    app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
    assert!(!app.context_open());
    app.event(event(EventKind::Extension {
        name: SUBAGENTS_EVENT.into(),
        data: json!(SubagentsView {
            records: records(),
            open: true,
            pending_notifications: 0,
        }),
    }));
    app.event(event(EventKind::ApprovalRequested {
        turn_id: None,
        approval_id: ApprovalId("approval".into()),
        request: ApprovalRequest::Questions {
            questions: vec![Question {
                id: "question".into(),
                header: "Parent".into(),
                question: "QUEUED_APPROVAL".into(),
                options: None,
            }],
        },
    }));
    let screen = support::dump(&app.pager_frame(20).unwrap());
    assert!(screen.contains("Main needs input") && !screen.contains("QUEUED_APPROVAL"));
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(app.pager_frame(20).is_none());
    assert!(support::dump(&app.live().0).contains("QUEUED_APPROVAL"));
}
