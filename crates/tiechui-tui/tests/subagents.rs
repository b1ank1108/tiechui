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
        tasks: vec![
            AgentTask {
                agent: "worker".into(),
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
                model: "fixture/model".into(),
            },
            AgentTask {
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
                model: "fixture/model".into(),
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
fn viewer_search_group_thinking_cancel_and_navigation() {
    let mut panel = Panel::new(records());
    key(&mut panel, KeyCode::Right, KeyModifiers::NONE);
    assert!(
        matches!(key(&mut panel, KeyCode::Char('x'), KeyModifiers::CONTROL), Outcome::Cancel(id) if id == "dispatch")
    );
    key(&mut panel, KeyCode::Char('f'), KeyModifiers::CONTROL);
    for c in "reviewer".chars() {
        key(&mut panel, KeyCode::Char(c), KeyModifiers::NONE);
    }
    key(&mut panel, KeyCode::Enter, KeyModifiers::NONE);
    key(&mut panel, KeyCode::Right, KeyModifiers::NONE);
    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    let screen = support::dump(&panel.render(40, 14, &theme, &[], true));
    assert!(screen.contains("reviewer") && !screen.contains("worker"));
    key(&mut panel, KeyCode::Enter, KeyModifiers::NONE);
    let screen = support::dump(&panel.render(40, 14, &theme, &[], true));
    assert!(screen.contains("Finished reviewing"));
    key(&mut panel, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut panel, KeyCode::Char('s'), KeyModifiers::CONTROL);
    assert!(matches!(
        key(&mut panel, KeyCode::Enter, KeyModifiers::NONE),
        Outcome::Close
    ));
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
        }],
    }));
    app.paste("MAIN_DRAFT");
    app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::ALT));
    assert!(!app.context_open());
    app.event(event(EventKind::Extension {
        name: SUBAGENTS_EVENT.into(),
        data: json!(SubagentsView {
            records: records(),
            open: true
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
