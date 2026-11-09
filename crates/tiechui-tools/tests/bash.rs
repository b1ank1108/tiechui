#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tiechui_model::{Content, ToolCall};
use tiechui_protocol::{SessionEpoch, ThreadId, TurnId};
use tiechui_tools::{BashTool, ExecutionScope, Tool, ToolContext, ToolOutput, ToolUpdate};
use tokio_util::sync::CancellationToken;

fn context() -> ToolContext {
    ToolContext {
        invoker: None,
        branch: Default::default(),
        parent_session: None,
        cwd: std::env::current_dir().unwrap(),
        cancel: CancellationToken::new(),
        updates: Arc::new(|_| {}),
        append_entry: Arc::new(|_| Ok(())),
        epoch: SessionEpoch(42),
        scope: ExecutionScope {
            thread_id: ThreadId("thread".into()),
            turn_id: TurnId("turn".into()),
        },
        origin: Default::default(),
    }
}

fn call(arguments: Value) -> ToolCall {
    ToolCall {
        id: "bash-call".into(),
        name: "bash".into(),
        arguments,
        opaque: None,
    }
}

fn text(output: &ToolOutput) -> &str {
    match &output.content[..] {
        [Content::Text { text, .. }] => text,
        _ => panic!("expected one text block"),
    }
}

#[tokio::test]
async fn result_text_for_empty_nonzero_signal_and_binary_output() {
    for (command, expected, code) in [
        ("true", "(no output)", 0),
        ("printf 'out\\n'; printf 'err\\n' >&2", "out\nerr\n", 0),
        ("exit 7", "(no output)\n\nCommand exited with code 7", 7),
        (
            "printf partial; exit 2",
            "partial\n\nCommand exited with code 2",
            2,
        ),
        ("printf '\\377\\342\\202'", "��", 0),
        #[cfg(unix)]
        (
            "kill -TERM $$",
            "(no output)\n\nCommand exited with code 143",
            143,
        ),
        #[cfg(windows)]
        (
            "kill -TERM $$",
            "(no output)\n\nCommand exited with code 3840",
            3840,
        ),
    ] {
        let result = BashTool::default()
            .run(call(json!({"command":command})), context())
            .await
            .unwrap();
        assert_eq!(text(&result), expected, "{command}");
        assert_eq!(result.is_error, code != 0);
        assert!(result.details.is_none());
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["exit_code"], code);
        assert_eq!(structured["truncated"], false);
        assert!(structured.get("full_output_path").is_none());
    }
}

#[tokio::test]
async fn control_character_sanitization_for_results_and_updates() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/bash-sanitization.json")).unwrap();
    let input = fixture["input"].as_str().unwrap();
    let expected = fixture["expected"].as_str().unwrap();
    let escaped = input
        .bytes()
        .map(|byte| format!("\\{byte:03o}"))
        .collect::<String>();
    for code in [0, 7] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut ctx = context();
        ctx.updates = {
            let events = events.clone();
            Arc::new(move |update| events.lock().unwrap().push(update))
        };
        let result = BashTool::default()
            .run(
                call(json!({"command":format!("printf '{escaped}'; exit {code}")})),
                ctx,
            )
            .await
            .unwrap();
        let final_text = if code == 0 {
            expected.to_string()
        } else {
            format!("{expected}\n\nCommand exited with code {code}")
        };
        assert_eq!(text(&result), final_text);
        assert_eq!(
            result.structured_content.as_ref().unwrap()["output"],
            expected
        );
        let events = events.lock().unwrap();
        assert_eq!(text(&events.last().unwrap().output), expected);
        for update in events
            .iter()
            .filter(|update| !update.output.content.is_empty())
        {
            assert!(expected.starts_with(text(&update.output)));
        }
    }
}

#[tokio::test]
async fn sanitization_handles_unicode_annotations_split_across_chunks() {
    let result = BashTool::default()
        .run(
            call(json!({
                "command":"printf 'before\\357'; sleep 0.02; printf '\\277\\271after\\n'"
            })),
            context(),
        )
        .await
        .unwrap();
    assert_eq!(text(&result), "beforeafter\n");
    assert_eq!(
        result.structured_content.unwrap()["output"],
        "beforeafter\n"
    );
    let empty = BashTool::default()
        .run(call(json!({"command":"printf '\\000\\033'"})), context())
        .await
        .unwrap();
    assert_eq!(text(&empty), "(no output)");
    assert_eq!(empty.structured_content.unwrap()["output"], "");
}

#[tokio::test]
async fn timeout_and_cancel_report_status_and_keep_output() {
    for (command, expected) in [
        ("sleep 30", "Command timed out after 0.05 seconds"),
        (
            "printf '\\001ready\\033'; sleep 30",
            "ready\n\nCommand timed out after 0.05 seconds",
        ),
    ] {
        let result = BashTool::default()
            .run(call(json!({"command":command,"timeout":0.05})), context())
            .await
            .unwrap_err();
        assert_eq!(result.0, expected);
    }
    let tiny_timeout = BashTool::default()
        .run(
            call(json!({"command":"sleep 30","timeout":1e-7})),
            context(),
        )
        .await
        .unwrap_err();
    assert_eq!(tiny_timeout.0, "Command timed out after 1e-7 seconds");
    let mut ctx = context();
    let cancel = ctx.cancel.clone();
    ctx.updates = Arc::new(move |update| {
        if !update.output.content.is_empty() && text(&update.output).contains("ready") {
            cancel.cancel();
        }
    });
    let error = BashTool::default()
        .run(call(json!({"command":"printf ready; sleep 30"})), ctx)
        .await
        .unwrap_err();
    assert_eq!(error.0, "ready\n\nCommand aborted");
    let ctx = context();
    ctx.cancel.cancel();
    assert_eq!(
        BashTool::default()
            .run(call(json!({"command":"exit 99"})), ctx)
            .await
            .unwrap_err()
            .0,
        "Command aborted"
    );
}

#[tokio::test]
async fn timeout_validation_and_missing_directory() {
    for (timeout, expected) in [
        (0.0, "Invalid timeout: must be a finite number of seconds"),
        (-1.0, "Invalid timeout: must be a finite number of seconds"),
        (
            2147483.648,
            "Invalid timeout: maximum is 2147483.647 seconds",
        ),
    ] {
        assert_eq!(
            BashTool::default()
                .run(call(json!({"command":"true","timeout":timeout})), context())
                .await
                .unwrap_err()
                .0,
            expected
        );
    }
    assert!(
        BashTool::default()
            .run(call(json!({"timeout":1})), context())
            .await
            .is_err()
    );
    let mut ctx = context();
    ctx.cwd = ctx.cwd.join("does-not-exist-tiechui-test");
    let expected = format!(
        "Working directory does not exist: {}\nCannot execute bash commands.",
        ctx.cwd.display()
    );
    assert_eq!(
        BashTool::default()
            .run(call(json!({"command":"true"})), ctx)
            .await
            .unwrap_err()
            .0,
        expected
    );
}

#[tokio::test]
async fn streaming_updates_are_cumulative_throttled_and_tagged() {
    let events = Arc::new(Mutex::new(Vec::<(tokio::time::Instant, ToolUpdate)>::new()));
    let mut ctx = context();
    ctx.updates = {
        let events = events.clone();
        Arc::new(move |update| {
            events
                .lock()
                .unwrap()
                .push((tokio::time::Instant::now(), update))
        })
    };
    let result = BashTool::default()
        .run(
            call(json!({"command":"printf a; sleep 0.02; printf b; sleep 0.2; printf c"})),
            ctx,
        )
        .await
        .unwrap();
    assert_eq!(text(&result), "abc");
    let events = events.lock().unwrap();
    assert!(events[0].1.output.content.is_empty());
    assert!(events[0].1.output.details.is_none());
    assert_eq!(text(&events.last().unwrap().1.output), "abc");
    assert!(
        events
            .iter()
            .skip(1)
            .any(|(_, update)| text(&update.output) == "ab")
    );
    for (_, update) in events.iter() {
        assert_eq!(update.call_id, "bash-call");
        assert_eq!(update.epoch, SessionEpoch(42));
    }
    // Final flush may happen inside the throttle window; intermediate updates may not.
    for window in events[1..events.len() - 1].windows(2) {
        assert!(window[1].0.duration_since(window[0].0) >= Duration::from_millis(95));
    }
}

#[tokio::test]
async fn truncation_returns_exact_notice_and_complete_private_spill() {
    let result = BashTool::default().run(call(json!({"command":"i=0; while [ $i -lt 2001 ]; do printf 'line\\001\\n'; i=$((i+1)); done"})), context()).await.unwrap();
    let details = result.details.as_ref().unwrap();
    let path = details["fullOutputPath"].as_str().unwrap();
    assert_eq!(
        text(&result),
        format!(
            "{}\n\n[Showing lines 2-2001 of 2001. Full output: {path}]",
            "line\n".repeat(1999) + "line"
        )
    );
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "line\u{1}\n".repeat(2001)
    );
    let structured = result.structured_content.as_ref().unwrap();
    assert_eq!(structured["output"], "line\n".repeat(2001));
    assert_eq!(structured["truncated"], false);
    assert!(structured.get("full_output_path").is_none());
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn cwd_comes_from_context() {
    let directory = tempfile::tempdir().unwrap();
    let mut ctx = context();
    ctx.cwd = directory.path().canonicalize().unwrap();
    let expected = ctx.cwd.clone();
    let command = if cfg!(windows) { "pwd -W" } else { "pwd" };
    let result = BashTool::default()
        .run(call(json!({"command":command})), ctx)
        .await
        .unwrap();
    let actual = text(&result).strip_suffix('\n').unwrap();
    assert_eq!(std::fs::canonicalize(actual).unwrap(), expected);
}
