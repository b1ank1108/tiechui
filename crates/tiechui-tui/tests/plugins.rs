mod support;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tiechui_protocol::*;
use tiechui_tui::{App, Options, palette::ColorLevel, theme::Theme};

fn event(app: &mut App, event: EventKind) {
    app.event(Event {
        submission_id: None,
        thread_id: ThreadId("thread".into()),
        epoch: SessionEpoch(0),
        event,
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
fn ask(app: &mut App, prompt: PluginPrompt) {
    event(
        app,
        EventKind::ApprovalRequested {
            turn_id: None,
            approval_id: ApprovalId("plugin".into()),
            request: ApprovalRequest::Plugin {
                plugin: "fixture".into(),
                prompt,
            },
        },
    );
}

#[test]
fn panel_tree_and_status_at_both_widths_and_palettes() {
    let mut snapshots = vec![];
    for light in [false, true] {
        for width in [40, 80] {
            let mut app = app(light, width);
            event(
                &mut app,
                EventKind::Extension {
                    name: PLUGIN_EVENT.into(),
                    data: serde_json::json!(PluginStatus {
                        plugin: "fixture".into(),
                        text: "ready".into()
                    }),
                },
            );
            let status = support::dump(&app.live().0);
            ask(
                &mut app,
                PluginPrompt::Panel {
                    panel: PluginPanel {
                        title: "Build report".into(),
                        nodes: vec![
                            PanelNode::Text {
                                text: "Plain text node".into(),
                            },
                            PanelNode::Markdown {
                                text: "**Markdown** and `code`".into(),
                            },
                            PanelNode::List {
                                children: vec![PanelNode::Text {
                                    text: "First list entry".into(),
                                }],
                            },
                            PanelNode::Progress {
                                label: "Build".into(),
                                value: 2,
                                total: 3,
                            },
                            PanelNode::Form {
                                fields: vec![PanelField {
                                    id: "answer".into(),
                                    label: "Comment".into(),
                                    required: true,
                                }],
                            },
                        ],
                    },
                },
            );
            snapshots.push(format!(
                "{width} columns / {}\n{status}\n{}",
                if light { "light" } else { "dark" },
                support::dump(&app.live().0)
            ));
            app.paste("Looks good");
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            assert!(app.take_submissions().iter().any(|s| matches!(&s.op, Op::ApprovalResponse { decision: ApprovalDecision::Accept { content: Some(v) }, .. } if v["answer"] == "Looks good")));
        }
    }
    insta::assert_snapshot!("plugin_panel_status", snapshots.join("\n\n"));
}

#[test]
fn typed_prompts_return_decisions_without_sending_model_input() {
    let mut app = app(false, 80);
    for prompt in [
        PluginPrompt::Confirm {
            title: "Authorize".into(),
            text: "Allow capabilities?".into(),
        },
        PluginPrompt::Input {
            title: "Input".into(),
            text: "Your answer".into(),
        },
        PluginPrompt::Select {
            title: "Select".into(),
            text: "Choose".into(),
            options: vec!["First".into(), "Second".into()],
        },
    ] {
        let input = matches!(&prompt, PluginPrompt::Input { .. });
        ask(&mut app, prompt);
        if input {
            app.paste("answer");
        }
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let submissions = app.take_submissions();
        assert!(submissions.iter().any(|s| matches!(
            &s.op,
            Op::ApprovalResponse {
                decision: ApprovalDecision::Accept { .. },
                ..
            }
        )));
        assert!(
            !submissions
                .iter()
                .any(|s| matches!(&s.op, Op::UserInput { .. }))
        );
        event(
            &mut app,
            EventKind::ApprovalResolved {
                approval_id: ApprovalId("plugin".into()),
            },
        );
    }
}

#[test]
fn notification_is_transient_and_does_not_interrupt_the_composer_or_prompt() {
    let mut app = app(false, 80);
    app.paste("draft");
    let notify = |app: &mut App| {
        event(
            app,
            EventKind::Extension {
                name: PLUGIN_NOTICE_EVENT.into(),
                data: serde_json::json!("fixture: Notice: Finished"),
            },
        );
    };
    notify(&mut app);
    let rendered = support::dump(&app.live().0);
    assert!(rendered.contains("fixture: Notice: Finished"));
    assert!(rendered.contains("draft"));
    assert!(app.take_submissions().is_empty());
    app.key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    let rendered = support::dump(&app.live().0);
    assert!(rendered.contains("draftx"));
    assert!(!rendered.contains("fixture: Notice: Finished"));

    ask(
        &mut app,
        PluginPrompt::Input {
            title: "Input".into(),
            text: "Your answer".into(),
        },
    );
    notify(&mut app);
    app.paste("answer");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let submissions = app.take_submissions();
    assert_eq!(submissions.len(), 1);
    assert!(matches!(&submissions[0].op, Op::ApprovalResponse {
        decision: ApprovalDecision::Accept { content: Some(value) }, ..
    } if value["value"] == "answer"));
}
