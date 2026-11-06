mod support;

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tiechui_tui::cells::Cell;
use tiechui_tui::palette::ColorLevel;
use tiechui_tui::theme::Theme;
use tiechui_tui::tools::{self, Tool, View};

use support::dump;

const MARKDOWN: &str = "# Title\n\nSome **bold**, *italic*, `code` and [a link](https://example.com).\nA second source line.\n\n## Section\n\n### Detail\n\n- one\n- two with a longer item that wraps past the narrow width\n  - nested\n\n1. first\n2. second\n\n> quoted text\n\n```rust\nfn main() {}\n```\n\n| Name | Value |\n| --- | --- |\n| 中文 | 42 |\n\n---\n\nDone.";

fn tool(name: &str, arguments: Value, output: Option<(&str, bool)>) -> Tool {
    let mut tool = Tool::new(format!("{name}-id"), name.into(), arguments);
    if let Some((text, error)) = output {
        tool.output = Some(json!({"content": [{"kind": "text", "text": text}], "is_error": error}));
        tool.error = error;
        tool.duration = Some(Duration::from_millis(1500));
    }
    tool
}

#[test]
fn call_tool_matches_direct_cards_in_every_view_and_search_summarizes_names() {
    let theme = Theme::fixture(ColorLevel::TrueColor, false);
    let now = Instant::now();
    for direct in [
        tool(
            "mcp__fixture__echo",
            json!({"value":"hello"}),
            Some(("hello", false)),
        ),
        tool(
            "mcp__fixture__echo",
            json!({"value":"hello"}),
            Some(("denied", true)),
        ),
        tool("mcp__fixture__echo", json!({}), None),
        tool(
            "read",
            json!({"path":"src/lib.rs"}),
            Some(("contents", false)),
        ),
        edit(),
    ] {
        let wrapped = Tool {
            name: "call_tool".into(),
            arguments: json!({"name":direct.name, "arguments":direct.arguments}),
            ..direct.clone()
        };
        for width in [40, 80] {
            for view in [
                View::History,
                View::Transcript,
                View::Live {
                    phase: Duration::ZERO,
                    now,
                },
            ] {
                assert_eq!(
                    tools::render(std::slice::from_ref(&wrapped), width, &theme, view),
                    tools::render(std::slice::from_ref(&direct), width, &theme, view)
                );
            }
        }
    }
    let mut search = tool(
        "tool_search",
        json!({"query":"echo"}),
        Some(("full definition", false)),
    );
    search.output.as_mut().unwrap()["details"] = json!({"found":["mcp__fixture__echo"]});
    let lines = tools::render(&[search], 80, &theme, View::History);
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Called tool_search"));
    assert!(text.contains("Found 1 tools: mcp__fixture__echo"));
    assert!(!text.contains("full definition"));
}

fn edit() -> Tool {
    let mut tool = Tool::new(
        "edit-id".into(),
        "edit".into(),
        json!({"path": "src/main.rs"}),
    );
    tool.output = Some(json!({
        "content": [{"kind": "text", "text": "Successfully replaced 1 block(s) in src/main.rs."}],
        "details": {
            "diff": "  1 fn main() {\n- 2     println!(\"old\");\n+ 2     println!(\"new\");\n  3 }"
        }
    }));
    tool.duration = Some(Duration::from_millis(1500));
    tool
}

fn cells() -> Vec<Cell> {
    vec![
        Cell::User("fix the bug\nsecond line 中文".into()),
        Cell::Thinking("**Inspecting the parser**\n\nThe tokenizer drops `\\r`.".into()),
        Cell::Tools(vec![
            tool("read", json!({"path": "src/lib.rs"}), Some(("x", false))),
            tool("read", json!({"path": "src/app.rs"}), Some(("x", false))),
            tool(
                "grep",
                json!({"pattern": "fn main", "path": "src"}),
                Some(("x", false)),
            ),
            tool("ls", json!({"path": "."}), Some(("x", false))),
        ]),
        Cell::Tools(vec![tool(
            "bash",
            json!({"command": "cargo test\n--quiet"}),
            Some(("line 1\n\nline 2\nline 3\nline 4\nline 5", false)),
        )]),
        Cell::Tools(vec![tool(
            "bash",
            json!({"command": "false"}),
            Some(("Command exited with code 1", true)),
        )]),
        Cell::Tools(vec![tool(
            "read",
            json!({"path": "missing.rs"}),
            Some(("ENOENT: no such file", true)),
        )]),
        Cell::Tools(vec![edit()]),
        Cell::Tools(vec![tool(
            "exa__web_search",
            json!({"query": "rust"}),
            Some(("", false)),
        )]),
        Cell::Separator,
        Cell::Assistant(MARKDOWN.into()),
        Cell::Worked(Duration::from_secs(125)),
        Cell::Error("provider returned 500".into()),
        Cell::Interrupted,
    ]
}

fn render(theme: &Theme, width: usize) -> String {
    cells()
        .iter()
        .map(|cell| dump(&cell.lines(width, theme)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[test]
fn cells_dark_truecolor() {
    insta::assert_snapshot!(render(&Theme::fixture(ColorLevel::TrueColor, false), 80));
}

#[test]
fn cells_light_truecolor() {
    insta::assert_snapshot!(render(&Theme::fixture(ColorLevel::TrueColor, true), 80));
}

#[test]
fn cells_ansi256() {
    insta::assert_snapshot!(render(&Theme::fixture(ColorLevel::Ansi256, false), 80));
}

#[test]
fn cells_ansi16_without_palette() {
    insta::assert_snapshot!(render(&Theme::fixture(ColorLevel::Ansi16, false), 80));
}

#[test]
fn cells_narrow() {
    insta::assert_snapshot!(render(&Theme::fixture(ColorLevel::TrueColor, false), 40));
}

fn running() -> Vec<Tool> {
    let mut bash = tool("bash", json!({"command": "sleep 5"}), None);
    bash.started = Some(Instant::now());
    let mut partial = tool("bash", json!({"command": "cargo build"}), None);
    partial.started = Some(Instant::now());
    partial.partial =
        Some(json!({"content": [{"kind": "text", "text": "Compiling a\nCompiling b"}]}));
    vec![
        tool("read", json!({"path": "a.rs"}), Some(("x", false))),
        tool("read", json!({"path": "b.rs"}), None),
        bash,
        partial,
        tool("exa__fetch", json!({}), None),
    ]
}

fn states(theme: &Theme, width: usize) -> String {
    let tools = running();
    let live = View::Live {
        phase: Duration::ZERO,
        now: Instant::now(),
    };
    let mut out = vec!["== running".to_owned()];
    for group in tools::groups(&tools) {
        out.push(dump(&tools::render(&tools[group], width, theme, live)));
    }
    out.push("== expanded".into());
    for cell in [
        Cell::Tools(vec![tool(
            "bash",
            json!({"command": "cargo test\n--quiet"}),
            Some(("line 1\n\nline 2\nline 3\nline 4\nline 5", false)),
        )]),
        Cell::Tools(vec![edit()]),
        Cell::Thinking("**Inspecting the parser**\n\nThe tokenizer drops `\\r`.".into()),
    ] {
        out.push(dump(&cell.render(width, theme, View::Transcript, true)));
    }
    out.join("\n\n")
}

#[test]
fn tool_states_dark() {
    insta::assert_snapshot!(states(&Theme::fixture(ColorLevel::TrueColor, false), 80));
}

#[test]
fn tool_states_light() {
    insta::assert_snapshot!(states(&Theme::fixture(ColorLevel::TrueColor, true), 80));
}

fn history(name: &str, arguments: Value, output: Value, error: bool) -> Tool {
    tool(name, arguments, Some((&output.to_string(), error)))
}

fn maintenance(theme: &Theme, width: usize) -> String {
    let status = |state: &str, used: u64| json!({"ok": true, "budget": {"input_tokens_estimate": used, "model_window_tokens": 128000, "pressure": if used > 90000 { "high" } else { "normal" }}, "maintenance": {"state": state}, "candidates": [{}, {}, {}], "has_more": false});
    let ranges = json!({"ranges": [
        {"candidate_id": "c1", "summary": "Goal: stop the parser timing out on large CRLF inputs.", "title": "Parser investigation"},
        {"candidate_id": "c2", "summary": "Removed duplicated CRLF fixtures."}
    ]});
    let compacted = json!({"ok": true, "applied": true, "blocks": [
        {"candidate_id": "c1", "ref": "b4", "start_ref": "m00012", "end_ref": "m00077"},
        {"candidate_id": "c2", "ref": "b5", "start_ref": "m00078", "end_ref": "m00090"}
    ], "effect": {"net_saved_tokens_estimate": 7900}, "maintenance": {"state": "cooldown"}});
    let refused = |code: &str, message: &str| json!({"ok": false, "error": {"code": code, "message": message}, "applied": false});
    let maintained = vec![
        history("context_status", json!({}), status("ready", 72000), false),
        history("compact_history", ranges.clone(), compacted, false),
    ];
    let mut read = tool("read_history", json!({"ref": "b4"}), None);
    read.started = Some(Instant::now());
    let groups = [
        ("== maintained", maintained.clone(), View::History),
        (
            "== deferred",
            vec![
                history(
                    "context_status",
                    json!({}),
                    status("cooldown", 96000),
                    false,
                ),
                history(
                    "compact_history",
                    ranges.clone(),
                    refused("MAINTENANCE_DEFERRED", "History was just maintained."),
                    true,
                ),
            ],
            View::History,
        ),
        (
            "== failed",
            vec![history(
                "compact_history",
                ranges,
                refused(
                    "CANDIDATE_CHANGED",
                    "1 of 2 candidates changed; nothing applied",
                ),
                true,
            )],
            View::History,
        ),
        (
            "== running",
            vec![
                history(
                    "search_history",
                    json!({"query": "parser timeout"}),
                    json!({"ok": true, "results": [{}, {}, {}], "has_more": false}),
                    false,
                ),
                read,
            ],
            View::Live {
                phase: Duration::ZERO,
                now: Instant::now(),
            },
        ),
        ("== maintained expanded", maintained, View::Transcript),
    ];
    let mut out = Vec::new();
    for (title, tools, view) in groups {
        assert_eq!(tools::groups(&tools).len(), 1);
        out.push(format!(
            "{title}\n{}",
            dump(&tools::render(&tools, width, theme, view))
        ));
    }
    for (title, degraded, view) in [
        ("== checkpoint", false, View::History),
        ("== checkpoint degraded", true, View::History),
        ("== checkpoint expanded", false, View::Transcript),
    ] {
        let cell = Cell::Compaction {
            summary: "## Goal\nContinue the implementation.".into(),
            tokens_before: 9500,
            degraded,
        };
        out.push(format!(
            "{title}\n{}",
            dump(&cell.render(width, theme, view, false))
        ));
    }
    let paused = Cell::Warning("Automatic compaction paused: still above the threshold.".into());
    out.push(format!("== paused\n{}", dump(&paused.lines(width, theme))));
    out.join("\n\n")
}

#[test]
fn context_maintenance_dark() {
    insta::assert_snapshot!(maintenance(
        &Theme::fixture(ColorLevel::TrueColor, false),
        80
    ));
}

#[test]
fn context_maintenance_ansi16_narrow() {
    insta::assert_snapshot!(maintenance(&Theme::fixture(ColorLevel::Ansi16, false), 40));
}

fn folds(theme: &Theme, width: usize) -> String {
    let cells = [
        (
            "== output",
            Cell::Tools(vec![tool(
                "bash",
                json!({"command": "cargo test"}),
                Some(("line 1\nline 2\nline 3\nline 4\nline 5", false)),
            )]),
        ),
        (
            "== explored",
            Cell::Tools(vec![
                tool(
                    "read",
                    json!({"path": "src/lib.rs"}),
                    Some(("pub mod app;", false)),
                ),
                tool(
                    "grep",
                    json!({"pattern": "fn main"}),
                    Some(("src/main.rs:1", false)),
                ),
            ]),
        ),
        (
            "== checkpoint",
            Cell::Compaction {
                summary: "## Goal\nContinue the implementation.".into(),
                tokens_before: 9500,
                degraded: false,
            },
        ),
        (
            "== thinking",
            Cell::ThinkingSummary("**Inspecting the parser**\n\nThe tokenizer drops `\\r`.".into()),
        ),
    ];
    let mut out = Vec::new();
    for (title, cell) in &cells {
        for unfolded in [false, true] {
            let (mut lines, control) = cell.folded(width, theme, unfolded, false);
            let control = control.expect(title);
            if !unfolded && *title == "== output" {
                lines[control] = tiechui_tui::cells::hovered(&lines[control], theme);
            }
            let state = if unfolded { " unfolded" } else { "" };
            out.push(format!("{title}{state}\n{}", dump(&lines)));
        }
    }
    out.join("\n\n")
}

#[test]
fn folds_dark() {
    insta::assert_snapshot!(folds(&Theme::fixture(ColorLevel::TrueColor, false), 80));
}

#[test]
fn folds_light() {
    insta::assert_snapshot!(folds(&Theme::fixture(ColorLevel::TrueColor, true), 80));
}
