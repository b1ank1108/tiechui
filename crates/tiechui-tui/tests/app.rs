mod support;

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::text::Line;
use serde_json::json;
use tiechui_protocol::*;
use tiechui_tui::mentions::Index;
use tiechui_tui::palette::ColorLevel;
use tiechui_tui::resume::{Loaded, Part, PartKind, Read, Summary};
use tiechui_tui::theme::Theme;
use tiechui_tui::{App, Exit, Options, Target, elapsed};

fn text(lines: &[Line<'_>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn event(kind: EventKind) -> Event {
    Event {
        submission_id: None,
        thread_id: ThreadId("t".into()),
        epoch: SessionEpoch(0),
        event: kind,
    }
}

fn turn(status: TurnStatus) -> Turn {
    Turn {
        started_at: 0,
        ended_at: None,
        id: TurnId("turn".into()),
        status,
        items: vec![],
    }
}

fn started(turns: Vec<Turn>) -> EventKind {
    EventKind::ThreadStarted {
        replay: false,
        thread: Thread {
            id: ThreadId("t".into()),
            epoch: SessionEpoch(0),
            turns,
        },
    }
}

fn delta(id: &str, text: &str) -> EventKind {
    EventKind::ItemDelta {
        turn_id: TurnId("turn".into()),
        item_id: ItemId(id.into()),
        delta: text.into(),
    }
}

fn completed(item: KnownItem) -> EventKind {
    EventKind::ItemCompleted {
        turn_id: TurnId("turn".into()),
        item: Item::Known(item),
    }
}

fn options() -> Options {
    Options {
        startup: vec![Op::SetModel {
            provider: "p".into(),
            model: "m".into(),
        }],
        queue_hint: Some("alt+up"),
        ..Options::default()
    }
}

fn app_with(width: usize, options: Options) -> App {
    App::new(Theme::fixture(ColorLevel::TrueColor, false), width, options)
}

fn app() -> App {
    app_with(40, options())
}

/// A thread is started and the initial `SetModel` is drained.
fn ready(width: usize) -> App {
    let mut app = app_with(width, options());
    app.event(event(started(vec![])));
    app.take_submissions();
    app
}

#[test]
fn native_compaction_status_summary_and_resume() {
    let mut app = ready(100);
    app.event(event(EventKind::TurnStarted {
        turn: turn(TurnStatus::InProgress),
    }));
    for (reason, label) in [
        (
            CompactionReason::Manual,
            "◇ Compacting context (0s • esc to cancel)",
        ),
        (
            CompactionReason::Threshold,
            "◇ Auto-compacting (0s • esc to cancel)",
        ),
        (
            CompactionReason::Overflow,
            "◇ Context overflow, compacting (0s • esc to cancel)",
        ),
    ] {
        app.event(event(EventKind::CompactionStarted { reason }));
        let lines = text(&app.live().0).join("\n");
        assert!(lines.contains(label), "{lines}");
    }
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(
        app.take_submissions()
            .iter()
            .any(|submission| matches!(submission.op, Op::Interrupt { .. }))
    );
    let item = KnownItem::Compaction { id: ItemId("checkpoint".into()), summary: "## Goal\nContinue the implementation.\n\n<modified-files>\nsrc/lib.rs\n</modified-files>".into(), tokens_before: 9500, degraded: false };
    app.event(event(completed(item.clone())));
    app.event(event(EventKind::CompactionEnded {
        reason: CompactionReason::Manual,
        aborted: false,
        error: None,
        will_retry: false,
        paused: None,
    }));
    app.event(event(EventKind::TurnCompleted {
        turn: turn(TurnStatus::Completed),
    }));
    let history = text(&app.take_history()).join("\n");
    insta::assert_snapshot!("native_compaction_summary", history);
    app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(
        text(&app.pager_frame(30).unwrap())
            .join("\n")
            .contains("Continue the implementation.")
    );
    let mut resumed = ready(100);
    resumed.event(event(EventKind::ThreadStarted {
        replay: true,
        thread: Thread {
            id: ThreadId("t".into()),
            epoch: SessionEpoch(0),
            turns: vec![Turn {
                started_at: 0,
                ended_at: None,
                id: TurnId("saved".into()),
                status: TurnStatus::Completed,
                items: vec![Item::Known(item)],
            }],
        },
    }));
    assert_eq!(text(&resumed.take_history()).join("\n"), history);
}

#[test]
fn compaction_progress_degraded_checkpoint_and_pause_are_visible() {
    let mut app = ready(120);
    app.event(event(EventKind::TurnStarted {
        turn: turn(TurnStatus::InProgress),
    }));
    app.event(event(EventKind::CompactionStarted {
        reason: CompactionReason::Threshold,
    }));
    app.event(event(EventKind::CompactionProgress {
        reason: CompactionReason::Threshold,
        phase: tiechui_protocol::CompactionPhase::Summarize,
        retries: 1,
        shrinks: 2,
    }));
    let lines = text(&app.live().0).join("\n");
    assert!(
        lines.contains("Auto-compacting · summarizing · retry 1 · trimmed 2× (0s • esc to cancel)"),
        "{lines}"
    );
    app.event(event(completed(KnownItem::Compaction {
        id: ItemId("checkpoint".into()),
        summary: "Note: 4 earlier messages did not fit.".into(),
        tokens_before: 9500,
        degraded: true,
    })));
    app.event(event(EventKind::CompactionEnded {
        reason: CompactionReason::Threshold,
        aborted: false,
        error: None,
        will_retry: false,
        paused: Some("Automatic compaction paused: still above the threshold.".into()),
    }));
    app.event(event(EventKind::TurnCompleted {
        turn: turn(TurnStatus::Completed),
    }));
    let history = text(&app.take_history()).join("\n");
    assert!(
        history.contains(
            "◆ Compacted context · 9.5k tokens summarized · partial · ctrl+o to view\n  └ Earlier messages did not fit the summary request."
        ),
        "{history}"
    );
    assert!(
        history.contains("▲ Automatic compaction paused: still above the threshold."),
        "{history}"
    );
}

#[test]
fn retry_wait_pauses_activity_and_resumes_when_request_starts() {
    let at = Instant::now() - Duration::from_secs(400);
    let mut app = ready(100);
    app.event_at(
        event(EventKind::TurnStarted {
            turn: turn(TurnStatus::InProgress),
        }),
        at,
    );
    let waiting = || {
        event(EventKind::RequestRetry {
            turn_id: TurnId("turn".into()),
            attempt: 2,
            max_retries: Some(3),
            delay_ms: Some(4000),
            message: "overloaded".into(),
        })
    };
    app.event_at(waiting(), at + Duration::from_secs(10));
    // Repeated waiting notifications must not restart the pause.
    app.event_at(waiting(), at + Duration::from_secs(100));
    let lines = text(&app.live().0).join("\n");
    assert!(
        lines.contains("Retrying request (10s • esc to interrupt)"),
        "{lines}"
    );
    app.event_at(
        event(EventKind::RequestRetry {
            turn_id: TurnId("turn".into()),
            attempt: 2,
            max_retries: Some(3),
            delay_ms: None,
            message: "overloaded".into(),
        }),
        at + Duration::from_secs(380),
    );
    let lines = text(&app.live().0).join("\n");
    assert!(!lines.contains("Retrying request"));
    assert!(
        lines.contains("Working (30s • esc to interrupt)"),
        "{lines}"
    );
    app.event_at(waiting(), at + Duration::from_secs(400));
    app.event_at(
        event(EventKind::TurnCompleted {
            turn: turn(TurnStatus::Interrupted),
        }),
        at + Duration::from_secs(700),
    );
    assert!(!text(&app.live().0).join("\n").contains("Retrying request"));
}

fn running(width: usize) -> App {
    let mut app = ready(width);
    app.event(event(EventKind::TurnStarted {
        turn: turn(TurnStatus::InProgress),
    }));
    app
}

fn ops(app: &mut App) -> Vec<Op> {
    app.take_submissions().into_iter().map(|s| s.op).collect()
}

fn ids(app: &mut App) -> Vec<(String, Op)> {
    app.take_submissions()
        .into_iter()
        .map(|s| (s.id.0, s.op))
        .collect()
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn with(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

/// Types like a person: far enough apart that no paste burst is detected.
fn typed(app: &mut App, text: &str) {
    let mut at = Instant::now();
    for c in text.chars() {
        at += Duration::from_millis(50);
        app.key_at(key(KeyCode::Char(c)), at);
    }
}

fn input(text: &str) -> Vec<InputContent> {
    vec![InputContent::Text { text: text.into() }]
}

fn queued(id: &str, text: &str) -> QueuedInput {
    QueuedInput {
        submission_id: SubmissionId(id.into()),
        content: input(text),
    }
}

fn queue(steering: &[(&str, &str)], follow_up: &[(&str, &str)]) -> Event {
    event(EventKind::QueueUpdated {
        steering: steering.iter().map(|(id, t)| queued(id, t)).collect(),
        follow_up: follow_up.iter().map(|(id, t)| queued(id, t)).collect(),
    })
}

fn retracted(inputs: &[(&str, &str)]) -> Event {
    event(EventKind::InputRetracted {
        inputs: inputs.iter().map(|(id, t)| queued(id, t)).collect(),
    })
}

/// The composer rows between its two rules.
fn draft(app: &mut App) -> Vec<String> {
    let (lines, _) = app.live();
    let rules: Vec<usize> = text(&lines)
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with('─'))
        .map(|(i, _)| i)
        .collect();
    text(&lines[rules[0] + 1..rules[1]])
}

fn user(text: &str, delivery: InputDelivery) -> Op {
    Op::UserInput {
        content: input(text),
        delivery,
    }
}

fn interrupt() -> Op {
    Op::Interrupt {
        turn_id: TurnId("turn".into()),
    }
}

fn retract(ids: &[&str]) -> Op {
    Op::Retract {
        submission_ids: ids.iter().map(|id| SubmissionId((*id).into())).collect(),
    }
}

#[test]
fn streaming_commits_complete_lines_and_holds_open_fences() {
    let mut app = app();
    app.event(event(started(vec![])));
    assert_eq!(
        ops(&mut app),
        [Op::SetModel {
            provider: "p".into(),
            model: "m".into()
        }]
    );
    app.event(event(EventKind::TurnStarted {
        turn: turn(TurnStatus::InProgress),
    }));
    assert!(
        text(&app.live().0)
            .iter()
            .any(|line| line.starts_with("• Working (0s • esc to interrupt)"))
    );
    app.event(event(EventKind::ItemStarted {
        turn_id: TurnId("turn".into()),
        item: Item::Known(KnownItem::AssistantMessage {
            id: ItemId("a".into()),
            text: String::new(),
        }),
    }));
    app.event(event(delta("a", "first line\nsecond")));
    assert_eq!(text(&app.take_history()), ["• first line"]);
    assert_eq!(text(&app.live().0)[0], "  second");
    app.event(event(delta("a", " half\n```rust\nfn main() {}\n")));
    // The fence is still open: its closing border must not reach scrollback yet.
    assert_eq!(text(&app.take_history()), ["  second half"]);
    app.event(event(delta("a", "```\n")));
    assert_eq!(
        text(&app.take_history()),
        ["", "  ```rust", "    fn main() {}", "  ```"]
    );
    app.event(event(completed(KnownItem::AssistantMessage {
        id: ItemId("a".into()),
        text: "first line\nsecond half\n```rust\nfn main() {}\n```\nend".into(),
    })));
    assert_eq!(text(&app.take_history()), ["", "  end"]);
    app.event(event(completed(KnownItem::ToolCall {
        id: ItemId("c".into()),
        name: "bash".into(),
        arguments: json!({"command": "ls"}),
    })));
    app.event(event(completed(KnownItem::ToolResult {
        id: ItemId("r".into()),
        call_id: ItemId("c".into()),
        output: json!({"content": [{"kind": "text", "text": "a\nb"}], "is_error": false}),
        is_error: false,
    })));
    app.event(event(EventKind::TurnCompleted {
        turn: turn(TurnStatus::Interrupted),
    }));
    assert_eq!(
        text(&app.take_history()),
        [
            "",
            "• Ran ls",
            "  └ a",
            "    b",
            "",
            "■ Conversation interrupted - tell the",
            "  model what to do differently.",
        ]
    );
    assert!(!app.running());
}

#[test]
fn resumed_history_replays_and_stale_epochs_are_ignored() {
    let mut app = app();
    let mut history = turn(TurnStatus::Completed);
    history.items = vec![
        Item::Known(KnownItem::UserMessage {
            id: ItemId("u".into()),
            content: input("hello"),
        }),
        Item::Known(KnownItem::AssistantMessage {
            id: ItemId("a".into()),
            text: "hi".into(),
        }),
    ];
    app.event(event(started(vec![history])));
    let lines = text(&app.take_history());
    assert_eq!(lines[1], "› hello");
    assert_eq!(lines.last().map(String::as_str), Some("• hi"));
    let mut stale = event(EventKind::Error {
        message: "old".into(),
    });
    stale.thread_id = ThreadId("other".into());
    app.event(stale);
    assert!(app.take_history().is_empty());
}

#[test]
fn ctrl_c_clears_then_asks_before_quitting() {
    let mut app = ready(40);
    app.paste("draft");
    let ctrl_c = with(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(app.key(ctrl_c), Exit::Running);
    assert_eq!(draft(&mut app), ["›"]);
    assert_eq!(app.key(ctrl_c), Exit::Running);
    assert!(text(&app.live().0).contains(&"  Press Ctrl+C again to quit".to_owned()));
    assert_eq!(app.key(ctrl_c), Exit::Quit);
    assert_eq!(
        app.key(with(KeyCode::Char('d'), KeyModifiers::CONTROL)),
        Exit::Quit
    );
}

#[test]
fn idle_enter_and_tab_both_submit_and_double_esc_clears() {
    let mut app = ready(40);
    app.paste("hi\r\nthere");
    app.key(key(KeyCode::Enter));
    typed(&mut app, "two");
    app.key(key(KeyCode::Tab));
    assert_eq!(
        ops(&mut app),
        [
            user("hi\nthere", InputDelivery::FollowUp),
            user("two", InputDelivery::FollowUp)
        ]
    );
    typed(&mut app, "keep");
    app.key(key(KeyCode::Esc));
    assert_eq!(draft(&mut app), ["› keep"]);
    app.key(key(KeyCode::Esc));
    assert_eq!(draft(&mut app), ["›"]);
    assert!(ops(&mut app).is_empty());
}

#[test]
fn running_enter_steers_and_tab_queues_a_follow_up() {
    let mut app = running(40);
    typed(&mut app, "steer");
    app.key(key(KeyCode::Enter));
    typed(&mut app, "later");
    app.key(key(KeyCode::Tab));
    typed(&mut app, "alias");
    app.key(with(KeyCode::Enter, KeyModifiers::ALT));
    assert_eq!(
        ops(&mut app),
        [
            user("steer", InputDelivery::Steer),
            user("later", InputDelivery::FollowUp),
            user("alias", InputDelivery::FollowUp),
        ]
    );
}

#[test]
fn retrieval_takes_only_the_newest_follow_up_and_restores_its_exact_draft() {
    let mut app = running(40);
    typed(&mut app, "first");
    app.key(key(KeyCode::Tab));
    app.paste(&"x".repeat(1200));
    app.key(key(KeyCode::Tab));
    let sent = ids(&mut app);
    let (first, second) = (sent[0].0.clone(), sent[1].0.clone());
    app.event(queue(
        &[("s", "steer")],
        &[(&first, "first"), (&second, &"x".repeat(1200))],
    ));
    typed(&mut app, "current");
    app.key(with(KeyCode::Up, KeyModifiers::ALT));
    assert_eq!(ops(&mut app), [retract(&[&second])]);
    app.event(retracted(&[(&second, &"x".repeat(1200))]));
    assert_eq!(draft(&mut app), ["› [Pasted Content 1200 chars]"]);
    app.event(queue(&[("s", "steer")], &[(&first, "first")]));
    app.key(with(KeyCode::Left, KeyModifiers::SHIFT));
    assert_eq!(ops(&mut app), [retract(&[&first])]);
}

#[test]
fn esc_with_steering_interrupts_and_keeps_queue_and_draft() {
    let mut app = running(40);
    app.event(queue(&[("s1", "steer")], &[("f1", "later")]));
    typed(&mut app, "draft");
    app.key(key(KeyCode::Esc));
    assert_eq!(ops(&mut app), [interrupt()]);
    assert_eq!(draft(&mut app), ["› draft"]);
}

#[test]
fn esc_without_steering_retracts_follow_ups_into_the_draft() {
    let mut app = running(40);
    app.event(queue(&[], &[("f1", "one"), ("f2", "two")]));
    typed(&mut app, "draft");
    app.key(key(KeyCode::Esc));
    assert_eq!(ops(&mut app), [retract(&["f1", "f2"]), interrupt()]);
    app.event(retracted(&[("f1", "one"), ("f2", "two")]));
    assert_eq!(draft(&mut app), ["› one", "  two", "  draft"]);
    // Nothing is resent automatically.
    app.event(event(EventKind::TurnCompleted {
        turn: turn(TurnStatus::Interrupted),
    }));
    assert!(ops(&mut app).is_empty());
}

#[test]
fn second_esc_while_interrupting_restores_steering_to_the_draft() {
    let mut app = running(40);
    app.event(queue(&[("s1", "fix it")], &[("f1", "then this")]));
    app.key(key(KeyCode::Esc));
    app.key(key(KeyCode::Esc));
    assert_eq!(ops(&mut app), [interrupt(), retract(&["s1", "f1"])]);
    app.event(retracted(&[("s1", "fix it"), ("f1", "then this")]));
    assert_eq!(draft(&mut app), ["› fix it", "  then this"]);
}

#[test]
fn completion_tab_accepts_before_queueing() {
    let mut app = running(80);
    typed(&mut app, "see @");
    assert!(app.take_index_request().is_some());
    app.set_index(Index::from_paths([
        "src/app.rs".into(),
        "docs/my notes.md".into(),
    ]));
    typed(&mut app, "note");
    finish_search(&mut app);
    app.key(key(KeyCode::Tab));
    assert_eq!(draft(&mut app), ["› see @\"docs/my notes.md\""]);
    assert!(ops(&mut app).is_empty());
    typed(&mut app, "@zzz");
    app.set_index(Index::default());
    // No results: Tab does not submit, Enter sends the original text.
    app.key(key(KeyCode::Tab));
    assert!(ops(&mut app).is_empty());
    app.key(key(KeyCode::Enter));
    assert_eq!(
        ops(&mut app),
        [user("see @\"docs/my notes.md\" @zzz", InputDelivery::Steer)]
    );
}

#[test]
fn large_pastes_collapse_and_expand_on_submit() {
    let mut app = ready(80);
    app.paste("a\nb\nc");
    assert_eq!(draft(&mut app), ["› a", "  b", "  c"]);
    app.key(with(KeyCode::Char('u'), KeyModifiers::CONTROL));
    app.key(with(KeyCode::Char('u'), KeyModifiers::CONTROL));
    app.key(with(KeyCode::Char('u'), KeyModifiers::CONTROL));
    app.key(with(KeyCode::Char('u'), KeyModifiers::CONTROL));
    app.key(with(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(draft(&mut app), ["›"]);
    app.paste("a\r\nb\nc\n");
    app.paste("a\nb\nc\n");
    assert_eq!(
        draft(&mut app),
        ["› [Pasted Content 6 chars][Pasted Content 6 chars #2]"]
    );
    app.key(key(KeyCode::Enter));
    assert_eq!(
        ops(&mut app),
        [user("a\nb\nc\na\nb\nc\n", InputDelivery::FollowUp)]
    );
}

#[test]
fn fast_unbracketed_input_treats_enter_and_tab_as_text() {
    let mut app = ready(80);
    let at = Instant::now();
    for (offset, code) in [
        KeyCode::Char('a'),
        KeyCode::Char('b'),
        KeyCode::Enter,
        KeyCode::Char('c'),
        KeyCode::Tab,
        KeyCode::Char('d'),
    ]
    .into_iter()
    .enumerate()
    {
        app.key_at(key(code), at + Duration::from_millis(offset as u64));
    }
    assert!(ops(&mut app).is_empty());
    assert_eq!(draft(&mut app), ["› ab", "  c    d"]);
    app.key_at(key(KeyCode::Enter), at + Duration::from_millis(200));
    assert_eq!(ops(&mut app), [user("ab\nc    d", InputDelivery::FollowUp)]);
}

#[test]
fn history_persists_and_browses_from_an_empty_draft() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state/input-history.jsonl");
    let mut app = app_with(
        40,
        Options {
            history: Some(path.clone()),
            ..options()
        },
    );
    app.event(event(started(vec![])));
    for entry in ["one", "two"] {
        app.paste(entry);
        app.key(key(KeyCode::Enter));
    }
    let mut app = app_with(
        40,
        Options {
            history: Some(path),
            ..options()
        },
    );
    app.event(event(started(vec![])));
    app.key(key(KeyCode::Up));
    assert_eq!(draft(&mut app), ["› two"]);
    app.key(key(KeyCode::Up));
    assert_eq!(draft(&mut app), ["› one"]);
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Down));
    assert_eq!(draft(&mut app), ["›"]);
    app.key(key(KeyCode::Up));
    typed(&mut app, "!");
    // Edited recall leaves history mode; Up moves the cursor instead.
    app.key(key(KeyCode::Up));
    assert_eq!(draft(&mut app), ["› two!"]);
}

#[test]
fn recalled_slash_commands_keep_browsing_history() {
    let mut app = ready(60);
    typed(&mut app, "one");
    app.key(key(KeyCode::Enter));
    typed(&mut app, "/statusline");
    app.key(key(KeyCode::Enter));
    app.key(key(KeyCode::Esc));
    app.key(key(KeyCode::Up));
    assert_eq!(draft(&mut app), ["› /statusline"]);
    assert!(
        !text(&app.live().0)
            .iter()
            .any(|r| r.contains("Tab complete"))
    );
    app.key(key(KeyCode::Up));
    assert_eq!(draft(&mut app), ["› one"]);
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Backspace));
    assert!(
        text(&app.live().0)
            .iter()
            .any(|r| r.contains("Tab complete"))
    );
}

#[test]
fn stash_swaps_drafts_and_restores_after_a_temporary_submit() {
    let mut app = ready(40);
    app.paste("long draft");
    let alt_s = with(KeyCode::Char('s'), KeyModifiers::ALT);
    app.key(alt_s);
    assert_eq!(draft(&mut app), ["›"]);
    assert!(
        text(&app.live().0)
            .iter()
            .any(|l| l.starts_with("  Draft stashed"))
    );
    typed(&mut app, "quick");
    app.key(key(KeyCode::Enter));
    assert_eq!(ops(&mut app), [user("quick", InputDelivery::FollowUp)]);
    assert_eq!(draft(&mut app), ["› long draft"]);
    app.key(alt_s);
    app.key(alt_s);
    assert_eq!(draft(&mut app), ["› long draft"]);
}

#[test]
fn editing_keys_follow_defaults() {
    let mut app = ready(40);
    typed(&mut app, "hello 世界 👍🏽");
    app.key(with(KeyCode::Char('w'), KeyModifiers::CONTROL));
    assert_eq!(draft(&mut app), ["› hello 世界"]);
    app.key(with(KeyCode::Char('a'), KeyModifiers::CONTROL));
    app.key(with(KeyCode::Char('d'), KeyModifiers::ALT));
    assert_eq!(draft(&mut app), ["›  世界"]);
    app.key(with(KeyCode::Char('y'), KeyModifiers::CONTROL));
    assert_eq!(draft(&mut app), ["› hello 世界"]);
    app.key(with(KeyCode::Char('-'), KeyModifiers::CONTROL));
    assert_eq!(draft(&mut app), ["›  世界"]);
    app.key(with(KeyCode::Char(']'), KeyModifiers::CONTROL));
    app.key(key(KeyCode::Char('界')));
    app.key(with(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert_eq!(draft(&mut app), ["›  世"]);
    app.key(with(KeyCode::Enter, KeyModifiers::SHIFT));
    app.key(with(KeyCode::Char('j'), KeyModifiers::CONTROL));
    assert_eq!(draft(&mut app), ["›  世", "", ""]);
}

#[test]
fn question_mark_on_empty_input_shows_shortcuts() {
    let mut app = ready(60);
    app.key(key(KeyCode::Char('?')));
    let lines = text(&app.live().0);
    assert!(lines.iter().any(|l| l.starts_with("  alt+s")));
    assert_eq!(draft(&mut app), ["›"]);
    app.key(key(KeyCode::Esc));
    assert!(!text(&app.live().0).iter().any(|l| l.starts_with("  alt+s")));
    typed(&mut app, "a?");
    assert_eq!(draft(&mut app), ["› a?"]);
}

fn screen(app: &mut App) -> String {
    let (lines, cursor) = app.live();
    let mut out = text(&lines).join("\n");
    out.push_str(&format!("\n[cursor {cursor:?}]"));
    out
}

fn states(width: usize) -> String {
    let mut shots = Vec::new();
    let mut app = ready(width);
    shots.push(("empty", screen(&mut app)));
    app.paste("first line\nsecond line that is long enough to wrap at forty columns\nthird");
    shots.push(("multi-line", screen(&mut app)));
    let mut app = ready(width);
    app.resize(width, 8);
    for n in 1..=9 {
        if n > 1 {
            app.key(with(KeyCode::Char('j'), KeyModifiers::CONTROL));
        }
        app.paste(&format!("line {n}"));
    }
    for _ in 0..4 {
        app.key(key(KeyCode::Up));
    }
    shots.push(("scroll hints", screen(&mut app)));
    let mut app = running(width);
    app.event(queue(
        &[("s1", "use the second approach")],
        &[("f1", "then run the tests"), ("f2", "a\nb\nc\nd")],
    ));
    shots.push(("queued previews", screen(&mut app)));
    let mut app = ready(width);
    app.set_index(Index::from_paths([
        "src/".into(),
        "src/app.rs".into(),
        "src/apply.rs".into(),
    ]));
    typed(&mut app, "@ap");
    finish_search(&mut app);
    shots.push(("menu open", screen(&mut app)));
    shots
        .into_iter()
        .map(|(name, shot)| format!("== {name}\n{shot}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn composer_states_80() {
    insta::assert_snapshot!(states(80));
}

#[test]
fn composer_states_40() {
    insta::assert_snapshot!(states(40));
}

fn started_item(item: KnownItem) -> Event {
    event(EventKind::ItemStarted {
        turn_id: TurnId("turn".into()),
        item: Item::Known(item),
    })
}

fn call(id: &str, name: &str, arguments: serde_json::Value) -> KnownItem {
    KnownItem::ToolCall {
        id: ItemId(id.into()),
        name: name.into(),
        arguments,
    }
}

fn result(id: &str, text: &str, is_error: bool) -> KnownItem {
    KnownItem::ToolResult {
        id: ItemId(format!("r-{id}")),
        call_id: ItemId(id.into()),
        output: json!({"content": [{"kind": "text", "text": text}], "is_error": is_error}),
        is_error,
    }
}

fn answer(id: &str, text: &str) -> [Event; 2] {
    [
        started_item(KnownItem::AssistantMessage {
            id: ItemId(id.into()),
            text: String::new(),
        }),
        event(completed(KnownItem::AssistantMessage {
            id: ItemId(id.into()),
            text: text.into(),
        })),
    ]
}

#[test]
fn parallel_results_commit_in_call_order_and_reads_merge() {
    let mut app = running(60);
    for (id, name, arguments) in [
        ("c1", "bash", json!({"command": "make"})),
        ("c2", "read", json!({"path": "a.rs"})),
        ("c3", "read", json!({"path": "b.rs"})),
    ] {
        app.event(started_item(call(id, name, arguments.clone())));
        app.event(event(completed(call(id, name, arguments))));
    }
    app.event(event(completed(result("c3", "x", false))));
    app.event(event(completed(result("c2", "x", false))));
    // The command is still running, so nothing after it may reach scrollback.
    assert!(app.take_history().is_empty());
    assert!(
        text(&app.live().0)
            .iter()
            .any(|l| l == "  └ Read a.rs, b.rs")
    );
    app.event(event(completed(result("c1", "built", false))));
    assert_eq!(text(&app.take_history()), ["• Ran make", "  └ built"]);
    for event in answer("a", "Done.") {
        app.event(event);
    }
    assert_eq!(
        text(&app.take_history()),
        [
            "",
            "• Explored",
            "  └ Read a.rs, b.rs",
            "",
            &"─".repeat(60),
            "",
            "• Done."
        ]
    );
}

#[test]
fn worked_for_needs_tools_and_more_than_a_minute() {
    let at = Instant::now();
    let mut app = ready(60);
    let turn_at = |app: &mut App, offset: u64, tools: bool| {
        app.event_at(
            event(EventKind::TurnStarted {
                turn: turn(TurnStatus::InProgress),
            }),
            at,
        );
        if tools {
            app.event(event(completed(call(
                "c",
                "bash",
                json!({"command": "ls"}),
            ))));
            app.event(event(completed(result("c", "x", false))));
        }
        for event in answer(&format!("a{offset}"), "ok") {
            app.event(event);
        }
        app.event_at(
            event(EventKind::TurnCompleted {
                turn: turn(TurnStatus::Completed),
            }),
            at + Duration::from_secs(offset),
        );
        text(&app.take_history())
    };
    assert!(
        !turn_at(&mut app, 60, true)
            .iter()
            .any(|l| l.contains("Worked"))
    );
    assert!(
        !turn_at(&mut app, 300, false)
            .iter()
            .any(|l| l.contains("Worked"))
    );
    let lines = turn_at(&mut app, 61, true);
    assert!(lines.last().unwrap().contains(" Worked for 1m 01s "));
}

#[test]
fn thinking_headings_drive_status_and_titled_summaries_print() {
    let mut app = running(60);
    app.event(started_item(KnownItem::Thinking {
        id: ItemId("t".into()),
        text: String::new(),
    }));
    app.event(event(delta("t", "**Reading the")));
    assert!(
        text(&app.live().0)
            .iter()
            .any(|l| l.starts_with("• Working (0s"))
    );
    app.event(event(delta("t", " parser**\n\nIt drops CR.")));
    assert!(
        text(&app.live().0)
            .iter()
            .any(|l| l.starts_with("• Reading the parser (0s • esc to interrupt)"))
    );
    app.event(event(completed(KnownItem::Thinking {
        id: ItemId("t".into()),
        text: "**Reading the parser**\n\nIt drops CR.".into(),
    })));
    assert_eq!(text(&app.take_history()), ["• It drops CR."]);
    app.event(event(completed(KnownItem::Thinking {
        id: ItemId("u".into()),
        text: "untitled musing".into(),
    })));
    assert!(app.take_history().is_empty());
    app.key(with(KeyCode::Char('t'), KeyModifiers::CONTROL));
    app.event(event(completed(KnownItem::Thinking {
        id: ItemId("v".into()),
        text: "shown now".into(),
    })));
    assert_eq!(text(&app.take_history()), ["", "• shown now"]);
    app.event(started_item(KnownItem::AssistantMessage {
        id: ItemId("a".into()),
        text: String::new(),
    }));
    assert!(
        !text(&app.live().0)
            .iter()
            .any(|l| l.contains("esc to interrupt"))
    );
}

#[test]
fn transcript_pager_shows_everything_expanded_and_closes() {
    let mut app = running(60);
    app.event(event(completed(call(
        "c",
        "bash",
        json!({"command": "seq 5"}),
    ))));
    app.event(event(completed(result("c", "1\n2\n3\n4\n5", false))));
    app.event(event(EventKind::TurnCompleted {
        turn: turn(TurnStatus::Interrupted),
    }));
    let scrollback = text(&app.take_history());
    assert!(scrollback.contains(&"    + 2 lines".to_owned()));
    assert_eq!(
        scrollback
            .iter()
            .filter(|l| l.contains("interrupted"))
            .count(),
        1
    );
    assert!(app.pager_frame(20).is_none());
    app.key(with(KeyCode::Char('o'), KeyModifiers::CONTROL));
    let page = text(&app.pager_frame(20).unwrap());
    assert_eq!(page.len(), 20);
    assert!(page[0].starts_with("/ T R A N S C R I P T /"));
    for row in ["  └ 1", "    5"] {
        assert!(page.contains(&row.to_owned()), "{row}");
    }
    app.key(key(KeyCode::Esc));
    assert!(app.pager_frame(20).is_none());
}

#[test]
fn elapsed_formats_seconds_minutes_and_hours() {
    assert_eq!(elapsed(Duration::from_secs(0)), "0s");
    assert_eq!(elapsed(Duration::from_secs(65)), "1m 05s");
    assert_eq!(elapsed(Duration::from_secs(3723)), "1h 02m 03s");
}

fn history(app: &mut App) -> Vec<String> {
    text(&app.take_history())
}

#[test]
fn the_slash_menu_completes_with_tab_and_runs_with_enter() {
    let mut app = ready(60);
    typed(&mut app, "/st");
    let rows = text(&app.live().0);
    assert!(
        rows.iter().any(|r| r.starts_with("> /statusline")),
        "{rows:#?}"
    );
    app.key(key(KeyCode::Tab));
    assert_eq!(draft(&mut app), ["› /statusline"]);
    assert!(
        !text(&app.live().0)
            .iter()
            .any(|r| r.contains("Tab complete"))
    );
    app.key(with(KeyCode::Backspace, KeyModifiers::NONE));
    app.key(key(KeyCode::Esc));
    assert!(
        !text(&app.live().0)
            .iter()
            .any(|r| r.contains("Tab complete"))
    );
    typed(&mut app, "x");
    app.key(with(KeyCode::Backspace, KeyModifiers::NONE));
    assert!(
        text(&app.live().0)
            .iter()
            .any(|r| r.contains("Tab complete"))
    );
    app.key(key(KeyCode::Enter));
    assert!(
        text(&app.live().0)
            .iter()
            .any(|r| r.contains("Configure Status Line"))
    );
    assert!(ops(&mut app).is_empty());
}

#[test]
fn module_commands_go_to_the_session_and_paths_stay_messages() {
    let mut app = ready(60);
    app.event(event(EventKind::CommandsUpdated {
        commands: vec![CommandInfo {
            name: "mcp".into(),
            description: "Show MCP servers".into(),
            argument_hint: None,
        }],
    }));
    typed(&mut app, "/mcp");
    assert!(
        text(&app.live().0)
            .iter()
            .any(|r| r.contains("Show MCP servers"))
    );
    app.key(key(KeyCode::Enter));
    assert_eq!(
        ops(&mut app),
        [Op::RunCommand {
            name: "mcp".into(),
            arguments: String::new()
        }]
    );
    app.event(event(EventKind::CommandOutput {
        name: "mcp".into(),
        text: "fs · ready · user\n  tools: fs__read".into(),
    }));
    assert_eq!(
        history(&mut app),
        ["• /mcp", "  fs · ready · user", "    tools: fs__read"]
    );
    app.paste("/tmp/build.log is empty");
    app.key(key(KeyCode::Enter));
    assert_eq!(
        ops(&mut app),
        [user("/tmp/build.log is empty", InputDelivery::FollowUp)]
    );
    typed(&mut app, "/trust allow");
    app.key(key(KeyCode::Enter));
    assert_eq!(app.take_trust().as_deref(), Some("allow"));
    typed(&mut app, "/quit");
    assert_eq!(app.key(key(KeyCode::Enter)), Exit::Quit);
}

#[test]
fn shell_drafts_show_a_red_marker_and_run_only_on_idle_enter() {
    let mut app = ready(40);
    typed(&mut app, "!ls -a");
    let (lines, cursor) = app.live();
    let row = lines
        .iter()
        .position(|l| text(std::slice::from_ref(l))[0].starts_with("! ls"))
        .unwrap();
    assert_eq!(
        lines[row].spans[0].style.fg,
        Theme::fixture(ColorLevel::TrueColor, false).error.fg
    );
    assert_eq!(cursor, Some((row, 7)));
    app.key(key(KeyCode::Tab));
    assert!(ops(&mut app).is_empty());
    app.key(key(KeyCode::Enter));
    assert_eq!(
        ops(&mut app),
        [Op::RunShell {
            command: "ls -a".into()
        }]
    );
    let mut busy = running(40);
    typed(&mut busy, "!pwd");
    busy.key(key(KeyCode::Enter));
    assert!(ops(&mut busy).is_empty());
    assert_eq!(draft(&mut busy), ["! pwd"]);
    assert!(
        text(&busy.live().0)
            .iter()
            .any(|r| r.contains("Shell commands run after the current turn"))
    );
}

fn info(provider: &str, levels: Vec<ThinkingLevel>) -> ModelInfo {
    ModelInfo {
        provider: provider.into(),
        id: "flash".into(),
        context_window: 400_000,
        reasoning: true,
        thinking_levels: levels,
    }
}

#[test]
fn model_and_thinking_pickers_switch_through_the_session() {
    let mut app = ready(60);
    let current = info("ds", vec![ThinkingLevel::Low, ThinkingLevel::High]);
    app.event(event(EventKind::SessionUpdated {
        model: current.clone(),
        thinking: ThinkingLevel::Low,
        context: None,
        total: TokenUsage::default(),
    }));
    app.event(event(EventKind::ModelsUpdated {
        models: vec![current, info("ds-anthropic", vec![ThinkingLevel::Off])],
    }));
    typed(&mut app, "/model");
    app.key(key(KeyCode::Enter));
    let rows = text(&app.live().0);
    assert!(
        rows.iter()
            .any(|r| r.starts_with("› 1. ds/flash ✓") && r.contains("400k window")),
        "{rows:#?}"
    );
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Enter));
    assert_eq!(
        ops(&mut app),
        [Op::SetModel {
            provider: "ds-anthropic".into(),
            model: "ds-anthropic/flash".into(),
        }]
    );
    assert_eq!(app.take_model_save().as_deref(), Some("ds-anthropic/flash"));
    typed(&mut app, "/thinking high");
    app.key(key(KeyCode::Enter));
    assert_eq!(
        ops(&mut app),
        [Op::SetThinking {
            level: ThinkingLevel::High
        }]
    );
    typed(&mut app, "/thinking");
    app.key(key(KeyCode::Enter));
    typed(&mut app, "deep");
    let rows = text(&app.live().0);
    assert!(
        rows.iter()
            .any(|r| r.starts_with("› 1. high") && r.contains("Deep reasoning (~16k tokens)")),
        "{rows:#?}"
    );
    assert!(!rows.iter().any(|r| r.contains("low ✓")));
    app.key(key(KeyCode::Esc));
    assert!(ops(&mut app).is_empty());
    assert!(!text(&app.live().0).iter().any(|r| r.contains("Thinking")));
    assert!(app.take_model_save().is_none());
}

#[test]
fn permissions_offers_the_modes_and_picks_by_digit() {
    let mut app = ready(80);
    app.event(event(EventKind::CommandsUpdated {
        commands: vec![CommandInfo {
            name: "permissions".into(),
            description: "Switch the mode".into(),
            argument_hint: None,
        }],
    }));
    typed(&mut app, "/permissions");
    app.key(key(KeyCode::Enter));
    let rows = text(&app.live().0);
    assert!(
        rows.iter().any(|r| r.starts_with("› 1. rules ✓")),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|r| r.contains("4. show")), "{rows:#?}");
    app.key(key(KeyCode::Char('2')));
    assert_eq!(
        ops(&mut app),
        [Op::RunCommand {
            name: "permissions".into(),
            arguments: "auto".into(),
        }]
    );
    typed(&mut app, "/permissions yolo");
    app.key(key(KeyCode::Enter));
    assert_eq!(
        ops(&mut app),
        [Op::RunCommand {
            name: "permissions".into(),
            arguments: "yolo".into(),
        }]
    );
}

fn asked(id: &str, request: ApprovalRequest) -> Event {
    event(EventKind::ApprovalRequested {
        turn_id: Some(TurnId("turn".into())),
        approval_id: ApprovalId(id.into()),
        request,
    })
}

fn bash(command: &str) -> ApprovalRequest {
    ApprovalRequest::Permission {
        actor: None,
        call_id: "c".into(),
        tool: "bash".into(),
        arguments: json!({ "command": command }),
        surface: "bash".into(),
        values: vec![command.into(), format!("/bin/{command}")],
        matched_pattern: Some("rm *".into()),
        origin: "user".into(),
        session_patterns: vec!["rm *".into()],
        preview: None,
    }
}

fn answered(app: &mut App) -> Vec<Op> {
    ops(app)
        .into_iter()
        .filter(|op| matches!(op, Op::ApprovalResponse { .. } | Op::Interrupt { .. }))
        .collect()
}

fn response(id: &str, decision: ApprovalDecision) -> Op {
    Op::ApprovalResponse {
        approval_id: ApprovalId(id.into()),
        decision,
    }
}

#[test]
fn permission_prompts_answer_once_for_the_session_or_with_a_reason() {
    let mut app = running(80);
    app.resize(80, 30);
    app.event(asked("a1", bash("rm -rf build")));
    let rows = text(&app.live().0);
    assert!(
        rows.iter()
            .any(|r| r.trim() == "Would you like to run the following command?"),
        "{rows:#?}"
    );
    assert_eq!(
        rows.iter().filter(|r| r.contains("rm -rf build")).count(),
        1
    );
    assert!(rows.iter().any(|r| r.trim() == "$ rm -rf build"));
    assert!(!rows.iter().any(|r| r.contains("Working")));
    assert!(
        !rows.iter().any(|r| r.starts_with('─')),
        "the composer gives way"
    );
    app.key(key(KeyCode::Char('2')));
    assert_eq!(
        answered(&mut app),
        [response(
            "a1",
            ApprovalDecision::AllowSession {
                both_directions: false
            }
        )]
    );
    assert!(text(&app.live().0).iter().any(|r| r.starts_with('─')));

    app.event(asked("a2", bash("rm -rf dist")));
    app.key(key(KeyCode::Up));
    app.key(key(KeyCode::Enter));
    let (lines, cursor) = app.live();
    let rows = text(&lines);
    let at = rows
        .iter()
        .position(|r| r.trim() == "Tell the agent what to do differently:")
        .unwrap();
    assert_eq!(cursor, Some((at + 1, 4)));
    typed(&mut app, "use trash");
    app.paste(" instead\n");
    app.key(key(KeyCode::Enter));
    assert_eq!(
        answered(&mut app),
        [response(
            "a2",
            ApprovalDecision::Deny {
                reason: Some("use trash instead".into())
            }
        )]
    );

    app.event(asked("a3", bash("rm -rf out")));
    app.key(key(KeyCode::Esc));
    assert_eq!(
        answered(&mut app),
        [
            response(
                "a3",
                ApprovalDecision::Deny {
                    reason: Some("Approval cancelled.".into())
                }
            ),
            Op::Interrupt {
                turn_id: TurnId("turn".into())
            },
        ]
    );
}

#[test]
fn waiting_for_an_answer_pauses_the_turn_timer() {
    let at = Instant::now();
    let mut app = ready(60);
    app.event_at(
        event(EventKind::TurnStarted {
            turn: turn(TurnStatus::InProgress),
        }),
        at,
    );
    app.event(event(completed(call(
        "c",
        "bash",
        json!({"command": "ls"}),
    ))));
    app.event_at(asked("a1", bash("ls")), at + Duration::from_secs(10));
    app.event_at(
        event(EventKind::ApprovalResolved {
            approval_id: ApprovalId("a1".into()),
        }),
        at + Duration::from_secs(300),
    );
    app.event(event(completed(result("c", "x", false))));
    app.event_at(
        event(EventKind::TurnCompleted {
            turn: turn(TurnStatus::Completed),
        }),
        at + Duration::from_secs(320),
    );
    let lines = text(&app.take_history());
    assert!(!lines.iter().any(|l| l.contains("Worked")), "{lines:#?}");
}

#[test]
fn elicitation_forms_validate_and_send_typed_content() {
    let mut app = running(80);
    app.resize(80, 30);
    let request = json!({
        "mode": "form",
        "message": "Who is deploying?",
        "requestedSchema": {
            "type": "object",
            "properties": {
                "confirm": {"type": "boolean", "title": "Confirm"},
                "name": {"type": "string", "title": "Name", "description": "Your login"},
            },
            "required": ["name"],
        },
    });
    app.event(asked(
        "e1",
        ApprovalRequest::Elicitation {
            server: "deploy".into(),
            request,
        },
    ));
    let rows = text(&app.live().0);
    assert!(
        rows.iter().any(|r| r.trim() == "Server: deploy"),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|r| r.trim() == "1/2 · Confirm"));
    app.key(key(KeyCode::Enter));
    app.key(key(KeyCode::Enter));
    let rows = text(&app.live().0);
    assert!(
        rows.iter()
            .any(|r| r.contains("Answer required fields before submitting.")),
        "{rows:#?}"
    );
    assert!(rows.iter().any(|r| r.trim() == "2/2 · Name (required)"));
    typed(&mut app, "ana");
    app.key(key(KeyCode::Enter));
    assert_eq!(
        answered(&mut app),
        [response(
            "e1",
            ApprovalDecision::Accept {
                content: Some(json!({"confirm": true, "name": "ana"}))
            }
        )]
    );
    app.event(asked(
        "e2",
        ApprovalRequest::Elicitation {
            server: "deploy".into(),
            request: json!({"message": "Continue?", "requestedSchema": {"type": "object", "properties": {}}}),
        },
    ));
    assert!(
        text(&app.live().0)
            .iter()
            .any(|r| r.contains("2. Deny  Decline this request and continue"))
    );
    app.key(key(KeyCode::Char('2')));
    assert_eq!(
        answered(&mut app),
        [response("e2", ApprovalDecision::Decline)]
    );
}

#[test]
fn prompt_layouts() {
    let mut shots = Vec::new();
    for (width, height) in [(80, 30), (44, 16)] {
        let mut app = running(width);
        app.resize(width, height);
        app.event(asked(
            "w",
            ApprovalRequest::Permission {
            actor: None,
                call_id: "c".into(),
                tool: "edit".into(),
                arguments: json!({"path": "src/lib.rs", "edits": [{"oldText": "a\nb", "newText": "c"}]}),
                surface: "path_write".into(),
                values: vec!["/repo/src/lib.rs".into()],
                matched_pattern: None,
                origin: "builtin".into(),
                session_patterns: vec!["/repo/src/*".into()],
                preview: None,
            },
        ));
        shots.push(format!("{width}x{height}\n{}", screen(&mut app)));
    }
    insta::assert_snapshot!(shots.join("\n\n"));
}

fn questions() -> ApprovalRequest {
    ApprovalRequest::Questions { questions: serde_json::from_value(json!([
        {"id":"plan","header":"Plan","question":"Which implementation should we use?","options":[
            {"label":"Small change (Recommended)","description":"Keep the current interface and fix the behavior."},
            {"label":"Redesign","description":"Replace the interface with a larger design."}]},
        {"id":"notes","header":"Details","question":"What else should we consider?"}
    ])).expect("valid questions") }
}

#[test]
fn ask_keeps_drafts_edits_text_and_requires_explicit_submission() {
    let mut app = running(80);
    app.event(asked("q", questions()));
    assert!(screen(&mut app).contains("2 unanswered"));
    app.key(key(KeyCode::Tab));
    typed(&mut app, "abcd");
    app.key(key(KeyCode::Left));
    typed(&mut app, "!");
    app.key(with(KeyCode::Char('n'), KeyModifiers::CONTROL));
    typed(&mut app, "free text");
    app.key(with(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert!(screen(&mut app).contains("abc!d"));
    assert!(screen(&mut app).contains("2 unanswered"));
    app.key(key(KeyCode::Enter));
    assert!(screen(&mut app).contains("free text"));
    app.key(key(KeyCode::Enter));
    assert_eq!(
        answered(&mut app),
        [response(
            "q",
            ApprovalDecision::Accept {
                content: Some(json!({"answers":{
                    "plan":{"answers":["Small change (Recommended)","user_note: abc!d"]},
                    "notes":{"answers":["user_note: free text"]}
                }}))
            }
        )]
    );
}

#[test]
fn ask_uncommitted_choices_and_notes_are_empty_until_proceed() {
    let mut app = running(80);
    app.event(asked("q", questions()));
    app.key(key(KeyCode::Char('3')));
    typed(&mut app, "hjkl 123 ");
    app.key(with(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.key(key(KeyCode::Enter));
    assert!(screen(&mut app).contains("Submit with unanswered questions?"));
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Enter));
    assert!(screen(&mut app).contains("hjkl 123"));
    assert!(answered(&mut app).is_empty());
    app.key(with(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.key(key(KeyCode::Enter));
    app.key(key(KeyCode::Enter));
    assert_eq!(
        answered(&mut app),
        [response(
            "q",
            ApprovalDecision::Accept {
                content: Some(json!({"answers":{
                    "plan":{"answers":[]}, "notes":{"answers":[]}
                }}))
            }
        )]
    );
}

#[test]
fn ask_escape_clears_notes_before_interrupting_and_paste_controls_are_not_navigation() {
    let mut app = running(80);
    app.event(asked("q", questions()));
    app.paste("hjkl 123 \x1b[A\x0e");
    assert!(screen(&mut app).contains("Question 1/2"));
    app.key(key(KeyCode::Esc));
    assert!(answered(&mut app).is_empty());
    assert!(!screen(&mut app).contains("hjkl 123"));
    assert!(screen(&mut app).contains("tab to add notes"));
    app.key(key(KeyCode::Esc));
    assert_eq!(
        answered(&mut app),
        [
            response("q", ApprovalDecision::Cancel),
            Op::Interrupt {
                turn_id: TurnId("turn".into())
            }
        ]
    );
}

#[test]
fn ask_confirmation_escape_and_backspace_return_to_unanswered_but_ctrl_c_interrupts() {
    for back in [KeyCode::Esc, KeyCode::Backspace] {
        let mut app = running(80);
        app.event(asked("q", questions()));
        app.key(key(KeyCode::Right));
        typed(&mut app, "committed note");
        app.key(key(KeyCode::Enter));
        assert!(screen(&mut app).contains("Submit with unanswered questions?"));
        app.key(key(back));
        assert!(screen(&mut app).contains("Question 1/2"));
        assert!(answered(&mut app).is_empty());
        app.key(key(KeyCode::Right));
        app.key(key(KeyCode::Enter));
        app.key(with(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(
            answered(&mut app),
            [
                response("q", ApprovalDecision::Cancel),
                Op::Interrupt {
                    turn_id: TurnId("turn".into())
                }
            ]
        );
    }
    let mut app = running(80);
    app.event(asked("q", questions()));
    app.key(key(KeyCode::Right));
    app.key(key(KeyCode::Esc));
    assert_eq!(answered(&mut app).len(), 2, "free text Escape interrupts");
}

#[test]
fn ask_escape_clears_visible_notes_even_when_options_have_focus() {
    let mut app = running(80);
    app.event(asked("q", questions()));
    app.key(key(KeyCode::Char('3')));
    app.paste("keep this note");
    app.key(key(KeyCode::Up));
    let display = screen(&mut app);
    assert!(display.contains("keep this note"));
    assert!(display.contains("←/→ to navigate questions"));
    assert!(display.contains("tab or esc to clear notes"));
    assert!(!display.contains("esc to interrupt"));
    app.key(key(KeyCode::Esc));
    assert!(answered(&mut app).is_empty());
    assert!(!screen(&mut app).contains("keep this note"));
    assert!(screen(&mut app).contains("tab to add notes"));
}

#[test]
fn ask_layouts() {
    let mut shots = vec![];
    for light in [false, true] {
        for (width, height) in [(80, 30), (40, 30), (40, 10)] {
            let mut app = App::new(
                Theme::fixture(ColorLevel::TrueColor, light),
                width,
                Options::default(),
            );
            app.resize(width, height);
            app.event(event(started(vec![])));
            app.event(asked("q", questions()));
            app.key(key(KeyCode::Down));
            shots.push(format!(
                "{width}x{height} light={light}\n{}",
                support::dump(&app.live().0)
            ));
            app.key(key(KeyCode::Char('3')));
            typed(&mut app, "Keep my draft");
            shots.push(format!("notes\n{}", support::dump(&app.live().0)));
            app.key(with(KeyCode::Char('n'), KeyModifiers::CONTROL));
            app.key(key(KeyCode::Enter));
            shots.push(format!("confirmation\n{}", support::dump(&app.live().0)));
        }
    }
    insta::assert_snapshot!(shots.join("\n\n"));
}

#[test]
fn ask_records_render_answers_notes_and_argument_fallbacks() {
    use tiechui_tui::tools::{Tool, View, render};
    let arguments = json!({"questions":[
        {"id":"plan","question":"Which implementation should we use?","options":[]},
        {"id":"details","question":"Anything else?"},
        {"id":"skipped","question":"Should this stay unanswered?"}
    ]});
    let mut shots = vec![];
    for light in [false, true] {
        for width in [40, 80] {
            let theme = Theme::fixture(ColorLevel::TrueColor, light);
            for status in ["submitted", "cancelled", "error"] {
                let mut tool =
                    Tool::new("q".into(), "request_user_input".into(), arguments.clone());
                tool.error = status != "submitted";
                tool.output = Some(if status == "submitted" {
                    json!({"details":{"kind":"ask","questions":arguments["questions"],"status":"submitted","response":{"answers":{
                        "plan":{"answers":["Small change (Recommended)","user_note: Preserve the existing interface and its documentation."]},
                        "details":{"answers":["user_note: A free-text answer"]},"skipped":{"answers":[]}
                    }}}})
                } else {
                    json!({"content":[{"kind":"text","text":if status == "cancelled" {"Operation aborted"} else {"request_user_input requires interactive TUI mode."}}],"is_error":true})
                });
                let lines = render(&[tool], width, &theme, View::History);
                let visible = text(&lines).join("\n");
                assert!(visible.contains(if status == "submitted" {
                    "Questions 2/3 answered"
                } else {
                    "Questions 0/3 answered"
                }));
                assert!(!visible.contains("user_note:"));
                if status == "cancelled" {
                    assert!(visible.contains("(interrupted)"));
                }
                shots.push(format!(
                    "{width} light={light} {status}\n{}",
                    support::dump(&lines)
                ));
            }
        }
    }
    insta::assert_snapshot!(shots.join("\n\n"));
}

#[test]
fn retry_is_a_control_command_with_no_arguments_and_separate_countdown() {
    let mut app = ready(100);
    typed(&mut app, "/retry now");
    app.key(key(KeyCode::Enter));
    assert!(ops(&mut app).is_empty());
    assert!(screen(&mut app).contains("Usage: /retry"));
    typed(&mut app, "/retry");
    app.key(key(KeyCode::Enter));
    assert_eq!(ops(&mut app), [Op::Retry]);
    app.event(event(EventKind::TurnStarted {
        turn: turn(TurnStatus::InProgress),
    }));
    app.event(event(EventKind::RequestRetry {
        turn_id: TurnId("turn".into()),
        attempt: 1,
        max_retries: Some(3),
        delay_ms: Some(30_000),
        message: "busy".into(),
    }));
    let display = screen(&mut app);
    assert!(display.contains("Retrying request"));
    assert!(display.contains("Retry 1/3 in 30s · Esc cancel · /retry now"));
    app.event(event(EventKind::RetryAvailable { available: true }));
    app.event(event(EventKind::TurnCompleted {
        turn: turn(TurnStatus::Interrupted),
    }));
    assert!(screen(&mut app).contains("/retry retry the failed request"));
    typed(&mut app, "x");
    assert!(!screen(&mut app).contains("/retry retry the failed request"));
}

/// Another session's thread, as a host switch starts it.
fn thread(id: &str, epoch: u64) -> Event {
    Event {
        submission_id: None,
        thread_id: ThreadId(id.into()),
        epoch: SessionEpoch(epoch),
        event: EventKind::ThreadStarted {
            replay: false,
            thread: Thread {
                id: ThreadId(id.into()),
                epoch: SessionEpoch(epoch),
                turns: vec![],
            },
        },
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_millis() as u64
}

fn summary(id: &str, cwd: &str, first: &str, modified_ago: u64, created_ago: u64) -> Summary {
    let now = now_ms();
    Summary {
        path: format!("/sessions/{id}.jsonl").into(),
        id: id.into(),
        cwd: cwd.into(),
        created_at: now - created_ago * 60_000,
        modified: now - modified_ago * 60_000,
        first_message: first.into(),
        text: format!("{first} answer about {id}"),
    }
}

/// The picker's screen, with local clock times cut, since they depend on the time zone.
fn page(app: &mut App, rows: usize) -> Vec<String> {
    text(&app.pager_frame(rows).expect("the picker is open"))
        .into_iter()
        .map(|line| match line.find(" ago · ") {
            Some(at) if line.contains("│ Created:") || line.contains("│ Updated:") => {
                line[..at + 4].to_owned()
            }
            _ => line,
        })
        .collect()
}

fn reads(app: &mut App) -> (u64, Vec<Read>) {
    let batch = app.take_reads().expect("reads");
    (batch.generation, batch.reads)
}

fn sessions() -> Vec<Summary> {
    vec![
        summary("a1", "/work/app", "fix the login form", 5, 3 * 1440),
        summary("b2", "/work/app", "write release notes", 120, 2),
    ]
}

#[test]
fn resume_picker_filters_sorts_and_switches_to_the_chosen_session() {
    // Listed before the picker opens: its dates are relative to that moment.
    let listed = sessions();
    let mut everywhere = sessions();
    everywhere.push(summary("c3", "/elsewhere", "tune the build", 1, 1));
    let mut app = ready(100);
    typed(&mut app, "/resume");
    app.key(key(KeyCode::Enter));
    let (generation, asked) = reads(&mut app);
    assert_eq!(asked, [Read::List { all: false }]);
    assert!(page(&mut app, 20).contains(&"Loading sessions…".to_owned()));
    app.loaded(
        generation,
        Loaded::List {
            all: false,
            sessions: Ok(listed),
        },
    );
    let rows = page(&mut app, 20);
    assert_eq!(rows[0], "Resume a previous session");
    assert!(
        rows[1].ends_with("Filter: Cwd  All    Sort: Updated  Created"),
        "{rows:#?}"
    );
    let titles: Vec<&String> = rows
        .iter()
        .filter(|r| r.starts_with("› ") || r.starts_with("  ") && !r.starts_with("   "))
        .collect();
    assert_eq!(titles[0], "› fix the login form");
    assert!(rows.contains(&"  5m ago".to_owned()), "{rows:#?}");
    assert!(rows.iter().any(|r| r.ends_with("1 / 2")));

    typed(&mut app, "release");
    let rows = page(&mut app, 20);
    assert!(rows.contains(&"› write release notes".to_owned()));
    assert!(!rows.iter().any(|r| r.contains("login")));
    typed(&mut app, " missing");
    assert!(page(&mut app, 20).contains(&"No matching sessions".to_owned()));
    app.key(key(KeyCode::Esc));
    assert!(page(&mut app, 20).contains(&"› fix the login form".to_owned()));

    app.key(key(KeyCode::Tab));
    app.key(key(KeyCode::Right));
    let rows = page(&mut app, 20);
    assert!(rows[1].ends_with("Sort: Updated  Created"));
    let at = |title: &str| rows.iter().position(|r| r.ends_with(title));
    assert!(
        at("write release notes") < at("fix the login form"),
        "newest created first"
    );
    assert!(
        rows.contains(&"› fix the login form".to_owned()),
        "the selection stays"
    );
    app.key(key(KeyCode::Down));
    app.key(key(KeyCode::Tab));
    app.key(key(KeyCode::Right));
    let (_, asked) = reads(&mut app);
    assert_eq!(asked, [Read::List { all: true }]);
    app.loaded(
        generation - 1,
        Loaded::List {
            all: true,
            sessions: Ok(vec![]),
        },
    );
    app.loaded(
        generation,
        Loaded::List {
            all: true,
            sessions: Ok(everywhere),
        },
    );
    let rows = page(&mut app, 20);
    assert!(
        rows.contains(&"  1m ago · /elsewhere".to_owned()),
        "{rows:#?}"
    );
    assert!(rows.contains(&"› tune the build".to_owned()), "{rows:#?}");

    app.key(key(KeyCode::Enter));
    assert!(app.pager_frame(20).is_none());
    assert_eq!(
        app.take_switch(),
        Some((Target::Resume("/sessions/c3.jsonl".into()), SessionEpoch(1)))
    );
    app.loaded(
        generation,
        Loaded::List {
            all: false,
            sessions: Ok(vec![]),
        },
    );
    app.opened(
        "/elsewhere".into(),
        Default::default(),
        true,
        vec!["config: odd".into()],
    );
    assert!(
        ops(&mut app).is_empty(),
        "nothing is sent before the next thread starts"
    );
    app.event(thread("c3", 1));
    app.event(event(EventKind::Error {
        message: "from the old session".into(),
    }));
    let history = text(&app.take_history()).join("\n");
    assert!(history.contains("config: odd"), "{history}");
    assert!(!history.contains("old session"));
    assert_eq!(
        app.thread().map(|(id, epoch)| (id.0.as_str(), epoch.0)),
        Some(("c3", 1))
    );
    assert_eq!(app.cwd(), std::path::Path::new("/elsewhere"));
}

#[test]
fn startup_picker_previews_read_only_and_leaving_starts_a_new_session() {
    let listed = sessions();
    let mut app = app_with(
        80,
        Options {
            pick: true,
            ..options()
        },
    );
    let (generation, _) = reads(&mut app);
    app.loaded(
        generation,
        Loaded::List {
            all: false,
            sessions: Ok(listed),
        },
    );
    assert!(
        page(&mut app, 20)
            .iter()
            .any(|r| r.contains("esc start new"))
    );
    app.key(with(KeyCode::Char('e'), KeyModifiers::CONTROL));
    let (_, asked) = reads(&mut app);
    assert_eq!(asked, [Read::Preview("/sessions/a1.jsonl".into())]);
    assert!(page(&mut app, 30).contains(&"  └ Loading conversation…".to_owned()));
    let part = |kind, text: &str| Part {
        kind,
        text: text.into(),
    };
    app.loaded(
        generation,
        Loaded::Preview {
            path: "/sessions/a1.jsonl".into(),
            parts: Ok(vec![
                part(PartKind::User, "fix the login form"),
                part(PartKind::Assistant, "Done: **validated** the email."),
                part(PartKind::Tool, "ok"),
            ]),
        },
    );
    let rows = page(&mut app, 30);
    for expected in [
        "⌄ fix the login form",
        "  │ Session:    a1",
        "  │ Created:    3d ago",
        "  │ Updated:    5m ago",
        "  │ Directory:  /work/app",
        "  │ Conversation:",
        "  │ fix the login form",
        "  │ Done: validated the email.",
        "  └ ok",
    ] {
        assert!(rows.contains(&expected.to_owned()), "{expected}: {rows:#?}");
    }
    app.key(with(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert!(app.take_reads().is_none(), "the preview is reused");
    let rows = page(&mut app, 20);
    assert_eq!(rows[0], "Conversation · fix the login form");
    assert!(
        rows.contains(&"› fix the login form".to_owned()),
        "{rows:#?}"
    );
    assert!(rows.contains(&"• Done: validated the email.".to_owned()));
    app.key(key(KeyCode::Esc));
    app.key(key(KeyCode::Esc));
    assert!(app.pager_frame(20).is_none());
    assert_eq!(app.take_switch(), Some((Target::New, SessionEpoch(0))));
    app.opened("/work/app".into(), Default::default(), true, vec![]);
    app.event(thread("n1", 0));
    typed(&mut app, "hi");
    app.key(key(KeyCode::Enter));
    assert_eq!(
        ops(&mut app),
        [
            Op::SetModel {
                provider: "p".into(),
                model: "m".into(),
            },
            user("hi", InputDelivery::FollowUp)
        ],
        "the command line's settings reach the new session, then the input"
    );
}

#[test]
fn new_keeps_the_model_and_reload_reopens_the_session() {
    let mut app = ready(60);
    app.event(event(EventKind::SessionUpdated {
        model: info("ds", vec![ThinkingLevel::Off, ThinkingLevel::High]),
        thinking: ThinkingLevel::High,
        context: None,
        total: TokenUsage::default(),
    }));
    typed(&mut app, "/new");
    app.key(key(KeyCode::Enter));
    assert_eq!(app.take_switch(), Some((Target::New, SessionEpoch(1))));
    app.opened("/work".into(), Default::default(), true, vec![]);
    app.event(thread("t2", 1));
    assert_eq!(
        ops(&mut app),
        [
            Op::SetModel {
                provider: "ds".into(),
                model: "ds/flash".into(),
            },
            Op::SetThinking {
                level: ThinkingLevel::High
            }
        ]
    );
    typed(&mut app, "/reload");
    app.key(key(KeyCode::Enter));
    assert_eq!(app.take_switch(), Some((Target::Reload, SessionEpoch(2))));
}

fn updated(thread: &str, epoch: u64) -> Event {
    Event {
        thread_id: ThreadId(thread.into()),
        epoch: SessionEpoch(epoch),
        ..event(EventKind::SessionUpdated {
            model: info("ds", vec![ThinkingLevel::Off, ThinkingLevel::High]),
            thinking: ThinkingLevel::High,
            context: None,
            total: TokenUsage::default(),
        })
    }
}

#[test]
fn session_header_heads_each_opened_session() {
    let mut shots = Vec::new();
    for width in [100, 50] {
        let mut app = app_with(
            width,
            Options {
                banner: true,
                cwd: "/home/u/work".into(),
                home: Some("/home/u".into()),
                ..options()
            },
        );
        let mut history = turn(TurnStatus::Completed);
        history.items = vec![Item::Known(KnownItem::UserMessage {
            id: ItemId("u".into()),
            content: input("hello"),
        })];
        app.event(event(started(vec![history])));
        assert!(
            app.take_history().is_empty(),
            "history waits for the header"
        );
        app.event(updated("t", 0));
        shots.push(text(&app.take_history()).join("\n"));
    }
    insta::assert_snapshot!("session_header", shots.join("\n\n"));

    let mut app = app_with(
        60,
        Options {
            banner: true,
            ..options()
        },
    );
    app.event(event(started(vec![])));
    app.event(updated("t", 0));
    app.take_history();
    typed(&mut app, "/new");
    app.key(key(KeyCode::Enter));
    app.take_switch();
    app.event(thread("t2", 1));
    app.event(updated("t2", 1));
    assert!(
        text(&app.take_history())[0].starts_with("╭"),
        "/new prints it again"
    );
    typed(&mut app, "/reload");
    app.key(key(KeyCode::Enter));
    app.take_switch();
    app.event(thread("t2", 2));
    app.event(updated("t2", 2));
    assert!(
        !text(&app.take_history())
            .iter()
            .any(|row| row.starts_with("╭")),
        "/reload keeps the session and its header"
    );

    // Anything but the session report releases a waiting header with what is known.
    let mut app = app_with(
        60,
        Options {
            banner: true,
            ..options()
        },
    );
    app.event(event(started(vec![])));
    app.event(event(EventKind::Error {
        message: "boom".into(),
    }));
    let rows = text(&app.take_history());
    assert!(rows.iter().any(|r| r.contains("no model")));
    assert!(rows.last().is_some_and(|r| r.contains("boom")));
}

#[test]
fn resume_layouts() {
    let mut shots = Vec::new();
    for (width, rows, dense) in [(100, 16, false), (100, 12, true), (40, 9, false)] {
        let listed = sessions();
        let mut app = ready(width);
        typed(&mut app, "/resume");
        app.key(key(KeyCode::Enter));
        let (generation, _) = reads(&mut app);
        app.loaded(
            generation,
            Loaded::List {
                all: false,
                sessions: Ok(listed),
            },
        );
        if dense {
            app.key(with(KeyCode::Char('o'), KeyModifiers::CONTROL));
        }
        shots.push(format!(
            "{width}x{rows}\n{}",
            page(&mut app, rows).join("\n")
        ));
    }
    insta::assert_snapshot!(shots.join("\n\n"));
}

#[test]
fn resumed_edit_renders_its_persisted_diff() {
    let mut app = app_with(80, options());
    let mut history = turn(TurnStatus::Completed);
    history.items = vec![
        Item::Known(KnownItem::ToolCall {
            id: ItemId("edit".into()),
            name: "edit".into(),
            arguments: json!({"path":"sample.rs"}),
        }),
        Item::Known(KnownItem::ToolResult {
            id: ItemId("result".into()),
            call_id: ItemId("edit".into()),
            is_error: false,
            output: json!({"content":[{"kind":"text","text":"Successfully replaced 1 block(s)."}], "details":{"diff":"- 1 let old = 1;\n+ 1 let new = 2;"}}),
        }),
    ];
    app.event(event(started(vec![history])));
    let lines = text(&app.take_history()).join("\n");
    assert!(lines.contains("let old = 1;"), "{lines}");
    assert!(lines.contains("let new = 2;"), "{lines}");
}

fn finish_search(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.searching() {
        assert!(Instant::now() < deadline, "file matcher did not settle");
        app.search_step();
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn reopening_mentions_reuses_index_until_a_turn_completes() {
    let mut app = ready(80);
    typed(&mut app, "@note");
    let (_, generation, cancel) = app.take_index_request().unwrap();
    app.set_index_result(generation, Index::from_paths(["notes.rs".into()]));
    finish_search(&mut app);
    app.key(key(KeyCode::Esc));
    assert!(!cancel.is_cancelled());
    typed(&mut app, " @notes");
    finish_search(&mut app);
    assert!(app.take_index_request().is_none());
    assert!(screen(&mut app).contains("notes.rs"));
    app.event(event(EventKind::TurnCompleted {
        turn: turn(TurnStatus::Completed),
    }));
    assert!(cancel.is_cancelled());
    let (_, next_generation, _) = app.take_index_request().unwrap();
    assert!(next_generation > generation);
}
