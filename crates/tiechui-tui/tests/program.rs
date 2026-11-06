#![allow(clippy::unwrap_used)]
mod support;
use serde_json::json;
use tiechui_protocol::{ProgramCall, ProgramCallState, ProgramRun};
use tiechui_tui::{
    palette::ColorLevel,
    theme::Theme,
    tools::{self, Tool, View},
};

#[test]
fn collapsed_and_expanded_programs_show_recent_calls_failures_and_highlighted_code() {
    let record = ProgramRun {
        calls: (0..9)
            .map(|index| ProgramCall {
                id: format!("program.{index}"),
                name: if index == 8 { "bash" } else { "read" }.into(),
                arguments: if index == 8 {
                    json!({"command":"cargo test"})
                } else {
                    json!({"path":format!("src/file{index}.rs")})
                },
                state: if index == 8 {
                    ProgramCallState::Failed
                } else {
                    ProgramCallState::Completed
                },
                error: (index == 8).then(|| "Command exited with code 101".into()),
            })
            .collect(),
    };
    let mut tool = Tool::new(
        "program".into(),
        "run_code".into(),
        json!({"description":"Inspect sources and test", "code":"const files = await tools.find({pattern: '*.rs'});\nconsole.log(files);\nreturn await tools.bash({command: 'cargo test'});"}),
    );
    tool.output = Some(
        json!({"details":record,"content":[{"kind":"text","text":"Error: Command exited with code 101\ntool calls: 8 succeeded, 1 failed"}]}),
    );
    tool.error = true;
    let mut snapshots = vec![];
    for light in [false, true] {
        for width in [40, 80] {
            let theme = Theme::fixture(ColorLevel::TrueColor, light);
            for view in [View::History, View::Transcript] {
                let rows = tools::render(&[tool.clone()], width, &theme, view);
                assert!(rows.iter().all(|line| line.width() <= width));
                snapshots.push(format!(
                    "{width} / light={light} / {view:?}\n{}",
                    support::dump(&rows)
                ));
                if matches!(view, View::History) {
                    assert!(rows.iter().any(|line| line.to_string().contains("+3 more")));
                    assert!(rows.iter().any(|line| {
                        line.spans.iter().any(|span| {
                            span.content.contains("Ran cargo test") && span.style == theme.error
                        })
                    }));
                }
            }
        }
    }
    insta::assert_snapshot!("program", snapshots.join("\n\n"));
}

#[test]
fn running_program_uses_first_code_line_and_partial_details() {
    let mut tool = Tool::new(
        "program".into(),
        "run_code".into(),
        json!({"code":"await tools.read({path:'a.rs'});\nreturn 1;"}),
    );
    tool.partial = Some(
        json!({"details":ProgramRun { calls:vec![ProgramCall { id:"program.1".into(), name:"read".into(), arguments:json!({"path":"a.rs"}), state:ProgramCallState::Running, error:None }] }}),
    );
    let rows = tools::render(
        &[tool],
        80,
        &Theme::fixture(ColorLevel::TrueColor, false),
        View::History,
    );
    insta::assert_snapshot!("program_running", support::dump(&rows));
}
