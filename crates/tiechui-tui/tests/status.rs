mod support;

use std::cell::RefCell;
use std::path::Path;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::style::Color;
use ratatui_core::text::{Line, Span};
use tiechui_protocol::{
    ApprovalId, ApprovalRequest, CompactionReason, Event, EventKind, ItemId, KnownItem, ModelInfo,
    SessionEpoch, ThinkingLevel, Thread, ThreadId, TokenUsage, Turn, TurnId, TurnStatus,
};
use tiechui_tui::palette::ColorLevel;
use tiechui_tui::status::{
    self, Git, GitFields, Item, PermissionMode, Phase, Snapshot, StatusLineConfig, battery,
    compact_tokens,
};
use tiechui_tui::theme::Theme;
use tiechui_tui::{App, Options};

use support::dump;

fn dark() -> Theme {
    Theme::fixture(ColorLevel::TrueColor, false)
}

fn model(id: &str, reasoning: bool) -> ModelInfo {
    ModelInfo {
        provider: "p".into(),
        id: id.into(),
        context_window: 200_000,
        reasoning,
        thinking_levels: vec![ThinkingLevel::Off],
    }
}

fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> TokenUsage {
    TokenUsage {
        input,
        output,
        cache_read,
        cache_write,
        ..Default::default()
    }
}

#[test]
fn cost_is_optional_and_reasoning_does_not_inflate_totals() {
    let mut fixture = Fixture::default();
    assert_eq!(
        status::text(Item::EstimatedThreadCost, &fixture.snapshot()),
        None
    );
    let before = status::text(Item::UsedTokens, &fixture.snapshot());
    fixture.total.reasoning = 10;
    fixture.total.cache_write_1h = 2;
    fixture.total.cost_nano_usd = Some(12_340_000);
    assert_eq!(status::text(Item::UsedTokens, &fixture.snapshot()), before);
    assert_eq!(
        status::text(Item::EstimatedThreadCost, &fixture.snapshot()).as_deref(),
        Some("~$0.01")
    );
    fixture.total.cost_nano_usd = Some(0);
    assert_eq!(
        status::text(Item::EstimatedThreadCost, &fixture.snapshot()).as_deref(),
        Some("~$0.00")
    );
}

/// Context usage that is exactly `used` percent of a 200k window.
fn used(percent: u64) -> TokenUsage {
    usage(percent * 2000, 0, 0, 0)
}

struct Fixture {
    model: Option<ModelInfo>,
    thinking: ThinkingLevel,
    context: Option<TokenUsage>,
    total: TokenUsage,
    compacted: bool,
    phase: Phase,
    permission: PermissionMode,
    git: Git,
}

impl Default for Fixture {
    fn default() -> Self {
        Self {
            model: Some(model("a model high", true)),
            thinking: ThinkingLevel::High,
            context: Some(used(25)),
            total: usage(20, 20, 80, 0),
            compacted: false,
            phase: Phase::Ready,
            permission: PermissionMode::Rules,
            git: Git {
                project: Some("tiechui".into()),
                branch: Some("main".into()),
                changes: Some("+2 -1".into()),
            },
        }
    }
}

impl Fixture {
    fn snapshot(&self) -> Snapshot<'_> {
        Snapshot {
            cwd: Path::new("/home/me/project"),
            home: Some(Path::new("/home/me")),
            hostname: Some("host"),
            model: self.model.as_ref(),
            thinking: self.thinking,
            context: self.context.as_ref(),
            total: &self.total,
            compacted: self.compacted,
            phase: self.phase,
            permission: self.permission,
            thread: Some("0190-thread"),
            git: &self.git,
        }
    }
}

fn config(items: &[Item], icons: &[Item]) -> StatusLineConfig {
    StatusLineConfig {
        items: items.to_vec(),
        use_theme_colors: true,
        icons: icons.to_vec(),
    }
}

fn plain(spans: &[Span<'_>]) -> String {
    spans.iter().map(|s| s.content.as_ref()).collect()
}

fn line(config: &StatusLineConfig, fixture: &Fixture, width: usize, theme: &Theme) -> String {
    plain(&status::line(config, &fixture.snapshot(), width, theme).spans)
}

fn style_of(spans: &[Span<'_>], text: &str) -> ratatui_core::style::Style {
    spans
        .iter()
        .find(|s| s.content.contains(text))
        .map(|s| s.style)
        .unwrap_or_else(|| panic!("{text:?} missing from {:?}", plain(spans)))
}

#[test]
fn icon_mode_mixes_with_text_and_reads_only_real_reasoning() {
    let selected = [
        Item::ModelWithReasoning,
        Item::CurrentDir,
        Item::ContextRemaining,
        Item::ContextUsed,
        Item::ContextWindowSize,
        Item::TotalInputTokens,
        Item::TotalOutputTokens,
        Item::CacheHitRate,
    ];
    let icons: Vec<Item> = selected
        .into_iter()
        .filter(|i| *i != Item::CurrentDir)
        .collect();
    let fixture = Fixture::default();
    let theme = dark();
    let text = plain(&status::spans(
        &config(&selected, &icons),
        &fixture.snapshot(),
        &theme,
    ));
    assert!(text.starts_with(&format!(
        "{} a model high (high) · ~/project · ",
        status::glyph(Item::Model)
    )));
    for value in ["75%", "25%", "200k", "100", "20", "80%"] {
        assert!(text.contains(value), "{value} in {text}");
    }
    for label in [" left", " used", " cache", "100 in", "20 out"] {
        assert!(!text.contains(label), "{label} in {text}");
    }
    assert_eq!(
        plain(&status::spans(
            &config(&selected, &[]),
            &fixture.snapshot(),
            &theme
        )),
        "a model high (high) · ~/project · 75% left · 25% used / 200k · 100 in · 20 out · 80% cache"
    );
    let flat = Fixture {
        model: Some(model("a model high", false)),
        ..Fixture::default()
    };
    let text = plain(&status::spans(
        &config(&selected, &icons),
        &flat.snapshot(),
        &theme,
    ));
    assert!(text.starts_with(&format!("{} a model high ·", status::glyph(Item::Model))));
    let reasoning = config(&[Item::Reasoning], &[]);
    assert_eq!(line(&reasoning, &flat, 80, &theme), "  none");
}

#[test]
fn context_battery_has_true_empty_and_full_and_warning_boundaries() {
    let theme = dark();
    let icons = config(&[Item::ContextRemaining], &[Item::ContextRemaining]);
    let texts = config(&[Item::ContextRemaining], &[]);
    let usage_color = style_of(
        &status::spans(&texts, &Fixture::default().snapshot(), &theme),
        "left",
    );
    for remaining in [0, 1, 8, 10, 11, 20, 21, 25, 26, 50, 51, 75, 76, 99, 100] {
        let fixture = Fixture {
            context: Some(used(100 - remaining)),
            ..Fixture::default()
        };
        let spans = status::spans(&icons, &fixture.snapshot(), &theme);
        let marker = format!("{remaining}%");
        assert_eq!(
            plain(&spans),
            format!("{} {marker}", battery(Some(remaining)))
        );
        if remaining > 0 && remaining < 100 {
            assert_ne!(battery(Some(remaining)), battery(Some(100)));
        }
        let expected = match remaining {
            0..=10 => theme.error,
            11..=20 => theme.warning,
            _ => usage_color,
        };
        assert_eq!(style_of(&spans, &marker), expected, "{remaining}");
        assert_eq!(
            plain(&status::spans(&texts, &fixture.snapshot(), &theme)),
            format!("{marker} left")
        );
        let mono = StatusLineConfig {
            use_theme_colors: false,
            ..icons.clone()
        };
        let spans = status::spans(&mono, &fixture.snapshot(), &theme);
        assert_eq!(style_of(&spans, &marker), theme.dim);
    }
    assert_eq!(battery(Some(0)), battery(None));
    assert_eq!(battery(Some(76)), battery(Some(99)));
    assert_eq!(battery(Some(51)), battery(Some(75)));
    assert_ne!(battery(Some(25)), battery(Some(26)));
    for unknown in [
        Fixture {
            context: None,
            ..Fixture::default()
        },
        Fixture {
            model: Some(ModelInfo {
                context_window: 0,
                ..model("m", true)
            }),
            ..Fixture::default()
        },
    ] {
        let snapshot = unknown.snapshot();
        assert_eq!(
            plain(&status::spans(&icons, &snapshot, &theme)),
            format!("{} --%", battery(None))
        );
        assert!(status::spans(&texts, &snapshot, &theme).is_empty());
    }
}

#[test]
fn running_states_have_distinct_glyphs_and_missing_values_leave_no_separator() {
    let theme = dark();
    let items = [Item::RunState, Item::GitBranch, Item::CacheHitRate];
    let line_config = config(&items, &items);
    let mut glyphs = std::collections::HashSet::new();
    for phase in [
        Phase::Ready,
        Phase::Thinking,
        Phase::Working,
        Phase::Waiting,
        Phase::Compacting,
        Phase::Error,
        Phase::Cancelled,
    ] {
        let fixture = Fixture {
            phase,
            git: Git::default(),
            total: TokenUsage::default(),
            ..Fixture::default()
        };
        let spans = status::spans(&line_config, &fixture.snapshot(), &theme);
        let text = plain(&spans);
        assert!(!text.contains(" · "), "{text}");
        glyphs.insert(text.chars().next());
        let alert = match phase {
            Phase::Error => Some(theme.error),
            Phase::Waiting => Some(theme.warning),
            _ => None,
        };
        if let Some(alert) = alert {
            assert_eq!(spans[0].style, alert);
        }
    }
    assert_eq!(glyphs.len(), 7);
}

#[test]
fn context_is_full_before_any_request_and_compacted_until_measured() {
    let theme = dark();
    let texts = config(&[Item::ContextRemaining], &[]);
    let icons = config(&[Item::ContextRemaining], &[Item::ContextRemaining]);
    let fresh = Fixture {
        context: None,
        total: TokenUsage::default(),
        ..Fixture::default()
    };
    assert_eq!(
        plain(&status::spans(&texts, &fresh.snapshot(), &theme)),
        "100% left"
    );
    assert_eq!(
        plain(&status::spans(&icons, &fresh.snapshot(), &theme)),
        format!("{} 100%", battery(Some(100)))
    );
    let compacted = Fixture {
        context: None,
        compacted: true,
        ..Fixture::default()
    };
    assert_eq!(
        plain(&status::spans(&texts, &compacted.snapshot(), &theme)),
        "◇ compacted"
    );
    assert_eq!(
        plain(&status::spans(&icons, &compacted.snapshot(), &theme)),
        "\u{eac5} compacted"
    );
}

#[test]
fn cache_rate_is_token_weighted_over_all_input_and_absent_without_input() {
    let rate = |total: TokenUsage| {
        let fixture = Fixture {
            total,
            ..Fixture::default()
        };
        status::text(Item::CacheHitRate, &fixture.snapshot())
    };
    assert_eq!(rate(usage(0, 5000, 0, 0)), None);
    assert_eq!(rate(usage(100, 5000, 0, 0)).as_deref(), Some("0% cache"));
    assert_eq!(rate(usage(0, 5000, 0, 100)).as_deref(), Some("0% cache"));
    assert_eq!(rate(usage(0, 5000, 100, 0)).as_deref(), Some("100% cache"));
    assert_eq!(
        rate(usage(100, 5000, 800, 100)).as_deref(),
        Some("80% cache")
    );
    // Three requests, then a cold and warm pair.
    assert_eq!(
        rate(usage(300, 60, 150, 30)).as_deref(),
        Some("31.3% cache")
    );
    assert_eq!(
        rate(usage(190, 2001, 800, 10)).as_deref(),
        Some("80% cache")
    );
    assert_eq!(rate(usage(100, 1, 800, 0)).as_deref(), Some("88.9% cache"));
    let fixture = Fixture {
        total: usage(300, 60, 150, 30),
        ..Fixture::default()
    };
    let snapshot = fixture.snapshot();
    let text = |item| status::text(item, &snapshot);
    assert_eq!(text(Item::TotalInputTokens).as_deref(), Some("480 in"));
    assert_eq!(text(Item::TotalOutputTokens).as_deref(), Some("60 out"));
    assert_eq!(text(Item::UsedTokens).as_deref(), Some("540 used"));
    let empty = Fixture {
        total: TokenUsage::default(),
        ..Fixture::default()
    };
    assert_eq!(status::text(Item::UsedTokens, &empty.snapshot()), None);
    assert_eq!(
        status::text(Item::TotalInputTokens, &empty.snapshot()).as_deref(),
        Some("0 in")
    );
}

#[test]
fn tokens_compact_to_one_decimal_without_a_trailing_zero() {
    for (value, expected) in [
        (0, "0"),
        (999, "999"),
        (1_000, "1k"),
        (1_450, "1.5k"),
        (200_000, "200k"),
        (1_000_000, "1M"),
        (1_250_000, "1.3M"),
        (1_048_576, "1M"),
    ] {
        assert_eq!(compact_tokens(value), expected);
    }
}

#[test]
fn unavailable_items_drop_out_and_colors_and_monochrome_stay_distinct() {
    let theme = dark();
    let items = config(&[Item::GitBranch, Item::Model, Item::BranchChanges], &[]);
    let fixture = Fixture {
        git: Git {
            branch: Some("main".into()),
            ..Git::default()
        },
        model: Some(model("model", false)),
        ..Fixture::default()
    };
    let spans = status::spans(&items, &fixture.snapshot(), &theme);
    assert_eq!(plain(&spans), "main · model");
    let (branch, name) = (style_of(&spans, "main"), style_of(&spans, "model"));
    assert!(matches!(branch.fg, Some(Color::Rgb(..))));
    assert_ne!(branch, name);
    assert_eq!(style_of(&spans, " · "), theme.dim);
    let mono = StatusLineConfig {
        use_theme_colors: false,
        ..items.clone()
    };
    let spans = status::spans(&mono, &fixture.snapshot(), &theme);
    assert!(spans.iter().all(|s| s.style == theme.dim));
    let ansi = Theme::fixture(ColorLevel::Ansi16, false);
    let spans = status::spans(&items, &fixture.snapshot(), &ansi);
    assert_eq!(style_of(&spans, "main").fg, Some(Color::Indexed(6)));
    assert_ne!(style_of(&spans, "model"), style_of(&spans, "main"));
    let none = Theme::fixture(ColorLevel::Unknown, false);
    let spans = status::spans(&items, &fixture.snapshot(), &none);
    assert!(spans.iter().all(|s| s.style == none.dim));
}

#[test]
fn an_empty_line_falls_back_to_the_shortcut_and_context_hint() {
    let theme = dark();
    let fixture = Fixture::default();
    let hint = line(&config(&[], &[]), &fixture, 60, &theme);
    assert!(hint.starts_with("  ? for shortcuts"));
    assert!(hint.ends_with("75% context left"));
    assert_eq!(wrap_width(&hint), 58);
    let nothing = Fixture {
        git: Git::default(),
        ..Fixture::default()
    };
    assert_eq!(
        line(&config(&[Item::GitBranch], &[]), &nothing, 60, &theme),
        hint
    );
    assert_eq!(
        line(&config(&[], &[]), &fixture, 20, &theme),
        "  ? for shortcuts"
    );
    for width in [1, 10, 20, 40, 80] {
        let wide = config(&Item::ALL, &[]);
        let rendered = status::line(&wide, &fixture.snapshot(), width, &theme);
        assert!(tiechui_tui::wrap::line_width(&rendered) <= width);
    }
}

fn wrap_width(text: &str) -> usize {
    tiechui_tui::wrap::width(text)
}

/// With styles, so softened theme colors and alert tones show in the snapshot.
#[test]
fn snapshots_cover_default_all_items_narrow_and_low_context() {
    let theme = dark();
    let mut out = Vec::new();
    let mut section = |title: &str, lines: Vec<Line<'static>>| {
        out.push(format!("== {title}\n{}", dump(&lines)));
    };
    let default = StatusLineConfig::default();
    let fixture = Fixture::default();
    section(
        "default",
        vec![status::line(&default, &fixture.snapshot(), 100, &theme)],
    );
    section(
        "default icons",
        vec![status::line(
            &config(&default.items, &default.items),
            &fixture.snapshot(),
            100,
            &theme,
        )],
    );
    let all = config(&Item::ALL, &[]);
    let every: Vec<Line<'static>> = Item::ALL
        .into_iter()
        .flat_map(|item| {
            [false, true].map(|icon| {
                let mut spans = vec![Span::raw(format!("{:<20}", item.id()))];
                spans.extend(
                    status::segment(item, &fixture.snapshot(), &theme, true, icon)
                        .unwrap_or_else(|| vec![Span::raw("(none)")]),
                );
                Line::from(spans)
            })
        })
        .collect();
    section("all items", every);
    section(
        "all items narrow",
        vec![status::line(&all, &fixture.snapshot(), 40, &theme)],
    );
    for (title, remaining, phase) in [
        ("low context warning", 15, Phase::Working),
        ("low context error", 5, Phase::Error),
    ] {
        let low = Fixture {
            context: Some(used(100 - remaining)),
            phase,
            ..Fixture::default()
        };
        let items = [Item::ContextRemaining, Item::RunState, Item::ContextUsed];
        section(
            title,
            vec![
                status::line(&config(&items, &items), &low.snapshot(), 80, &theme),
                status::line(&config(&items, &[]), &low.snapshot(), 80, &theme),
            ],
        );
    }
    insta::assert_snapshot!(out.join("\n\n"));
}

#[tokio::test]
async fn git_runs_only_what_the_selection_needs() {
    let calls = RefCell::new(Vec::new());
    let run = |args: Vec<String>| {
        let line = args.join(" ");
        calls.borrow_mut().push(line.clone());
        async move {
            match args[0].as_str() {
                "rev-parse" if args[1] == "--show-toplevel" => Some("/parent/project".into()),
                "rev-parse" => Some("feature/x".into()),
                "symbolic-ref" => None,
                "diff" if args[2] == "origin/main...HEAD" => None,
                "diff" => Some("2\t1\tsrc/a.rs\n-\t-\timage.png\n3\t0\tb.rs".into()),
                _ => None,
            }
        }
    };
    let none = status::collect(GitFields::default(), run).await;
    assert_eq!(none, Git::default());
    assert!(calls.borrow().is_empty());
    let fields = GitFields {
        project: true,
        ..GitFields::default()
    };
    let git = status::collect(fields, run).await;
    assert_eq!(git.project.as_deref(), Some("project"));
    assert_eq!(*calls.borrow(), ["rev-parse --show-toplevel"]);
    calls.borrow_mut().clear();
    let all = GitFields::of(&[Item::GitBranch, Item::BranchChanges]);
    let git = status::collect(all, run).await;
    assert_eq!(git.branch.as_deref(), Some("feature/x"));
    assert_eq!(git.changes.as_deref(), Some("+5 -1"));
    assert_eq!(
        *calls.borrow(),
        [
            "rev-parse --abbrev-ref HEAD",
            "symbolic-ref --quiet --short refs/remotes/origin/HEAD",
            "diff --numstat origin/main...HEAD --",
            "diff --numstat origin/master...HEAD --",
        ]
    );
    let clean = status::collect(all, |args: Vec<String>| async move {
        match args[0].as_str() {
            "symbolic-ref" => Some("origin/trunk".into()),
            "diff" if args[2] == "origin/trunk...HEAD" => Some(String::new()),
            _ => None,
        }
    })
    .await;
    assert_eq!(clean.changes.as_deref(), Some("No changes"));
    assert_eq!(clean.branch, None);
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
        id: TurnId("turn".into()),
        status,
        items: Vec::new(),
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn app(status_line: StatusLineConfig) -> App {
    let mut app = App::new(
        dark(),
        80,
        Options {
            cwd: "/isolated".into(),
            status_line,
            ..Options::default()
        },
    );
    app.resize(80, 40);
    app.event(event(EventKind::ThreadStarted {
        replay: false,
        thread: Thread {
            id: ThreadId("t".into()),
            epoch: SessionEpoch(0),
            turns: Vec::new(),
        },
    }));
    app.event(event(EventKind::SessionUpdated {
        model: model("STEP_MODEL", true),
        thinking: ThinkingLevel::High,
        context: Some(used(25)),
        total: usage(20, 20, 80, 0),
    }));
    app
}

fn live(app: &mut App) -> Vec<String> {
    app.live().0.iter().map(|l| plain(&l.spans)).collect()
}

/// An 80-column status line with `left` and `right` at the margins.
fn split(left: &str, right: &str) -> String {
    let gap = 76 - wrap_width(left) - wrap_width(right);
    format!("  {left}{}{right}", " ".repeat(gap))
}

fn command(app: &mut App, text: &str) {
    app.paste(text);
    app.key(key(KeyCode::Enter));
}

#[test]
fn the_line_sits_under_the_composer_and_hides_while_waiting() {
    let mut app = app(StatusLineConfig::default());
    let rows = live(&mut app);
    let last = rows.len() - 1;
    assert!(rows[last - 1].starts_with('─'));
    assert_eq!(
        rows[last],
        split("STEP_MODEL (high) · /isolated", "75% left / 200k")
    );
    app.event(event(EventKind::TurnStarted {
        turn: turn(TurnStatus::InProgress),
    }));
    app.event(event(EventKind::ApprovalRequested {
        turn_id: Some(TurnId("turn".into())),
        approval_id: ApprovalId("a".into()),
        request: ApprovalRequest::Elicitation {
            server: "docs".into(),
            request: serde_json::json!({"message": "run rm?"}),
        },
    }));
    assert!(!live(&mut app).iter().any(|r| r.contains("STEP_MODEL")));
    app.event(event(EventKind::TurnCompleted {
        turn: turn(TurnStatus::Failed),
    }));
    app.event(event(EventKind::TokenUsageUpdated {
        turn_id: TurnId("turn".into()),
        last: used(90),
        total: usage(1, 1, 0, 0),
    }));
    assert!(
        live(&mut app)
            .last()
            .is_some_and(|r| r.contains("10% left"))
    );
}

#[test]
fn permission_mode_shows_only_outside_rules_and_follows_the_session() {
    let theme = dark();
    let items = config(
        &[Item::Model, Item::PermissionMode],
        &[Item::PermissionMode],
    );
    let mode = |permission| {
        let fixture = Fixture {
            permission,
            ..Fixture::default()
        };
        let spans = status::line(&items, &fixture.snapshot(), 80, &theme).spans;
        (plain(&spans), spans)
    };
    assert_eq!(mode(PermissionMode::Rules).0, "  a model high");
    let (auto, spans) = mode(PermissionMode::Auto);
    assert!(auto.starts_with("  a model high ") && auto.ends_with(" ⏵⏵ auto"));
    assert_eq!(style_of(&spans, "auto"), theme.warning);
    let (_, spans) = mode(PermissionMode::Yolo);
    assert_eq!(
        style_of(&spans, "yolo"),
        theme
            .error
            .add_modifier(ratatui_core::style::Modifier::BOLD)
    );
    let mut app = app(StatusLineConfig::default());
    app.event(event(EventKind::Extension {
        name: tiechui_protocol::PERMISSION_MODE_EVENT.into(),
        data: serde_json::json!("auto"),
    }));
    assert_eq!(
        live(&mut app).last().unwrap(),
        &split("STEP_MODEL (high) · /isolated", "⏵⏵ auto · 75% left / 200k")
    );
}

#[test]
fn compaction_shows_in_the_run_state_and_the_context_until_the_next_usage() {
    let mut app = app(config(&[Item::RunState, Item::ContextRemaining], &[]));
    let state = |app: &mut App| live(app).last().cloned().unwrap_or_default();
    app.event(event(EventKind::TurnStarted {
        turn: turn(TurnStatus::InProgress),
    }));
    app.event(event(EventKind::CompactionStarted {
        reason: CompactionReason::Manual,
    }));
    assert_eq!(state(&mut app), split("", "Compacting · 75% left"));
    app.event(event(EventKind::ItemCompleted {
        turn_id: TurnId("turn".into()),
        item: tiechui_protocol::Item::Known(KnownItem::Compaction {
            id: ItemId("checkpoint".into()),
            summary: "Goal".into(),
            tokens_before: 9500,
            degraded: false,
        }),
    }));
    app.event(event(EventKind::CompactionEnded {
        reason: CompactionReason::Manual,
        aborted: false,
        error: None,
        will_retry: false,
        paused: None,
    }));
    app.event(event(EventKind::SessionUpdated {
        model: model("STEP_MODEL", true),
        thinking: ThinkingLevel::High,
        context: None,
        total: usage(20, 20, 80, 0),
    }));
    assert_eq!(state(&mut app), split("", "Thinking · ◇ compacted"));
    app.event(event(EventKind::TokenUsageUpdated {
        turn_id: TurnId("turn".into()),
        last: used(10),
        total: usage(40, 40, 80, 0),
    }));
    assert_eq!(state(&mut app), split("", "Thinking · 90% left"));
}

#[test]
fn git_is_requested_once_per_selection_and_again_after_a_turn() {
    let mut app = app(config(&[Item::GitBranch, Item::Model], &[]));
    let (cwd, fields) = app.take_git_request().unwrap();
    assert_eq!(cwd, Path::new("/isolated"));
    assert_eq!(fields, GitFields::of(&[Item::GitBranch]));
    // One request at a time.
    assert_eq!(app.take_git_request(), None);
    app.set_git(
        fields,
        Git {
            branch: Some("fixture-branch".into()),
            ..Git::default()
        },
    );
    assert_eq!(app.take_git_request(), None);
    assert!(
        live(&mut app)
            .iter()
            .any(|r| r == "  fixture-branch · STEP_MODEL")
    );
    app.event(event(EventKind::TurnStarted {
        turn: turn(TurnStatus::InProgress),
    }));
    app.event(event(EventKind::TurnCompleted {
        turn: turn(TurnStatus::Completed),
    }));
    assert_eq!(app.take_git_request().map(|r| r.1), Some(fields));
    let mut plain_app = app_without_git();
    assert_eq!(plain_app.take_git_request(), None);
}

fn app_without_git() -> App {
    app(config(&[Item::Model], &[]))
}

#[test]
fn picker_toggles_reorders_previews_filters_saves_and_cancels() {
    let mut app = app(StatusLineConfig::default());
    command(&mut app, "/statusline");
    let rows = live(&mut app);
    assert!(rows.iter().any(|r| r.contains("Configure Status Line")));
    assert!(
        rows.iter()
            .any(|r| *r == split("STEP_MODEL (high) · /isolated", "75% left / 200k"))
    );
    assert!(
        !rows.iter().any(|r| r.starts_with('─')),
        "the picker replaces the composer"
    );
    app.key(key(KeyCode::Char(' '))); // theme colors off
    app.key(key(KeyCode::Down)); // first selected item
    app.key(key(KeyCode::Right)); // behind current-dir
    assert!(
        live(&mut app)
            .iter()
            .any(|r| *r == split("/isolated · STEP_MODEL (high)", "75% left / 200k"))
    );
    for c in "cache".chars() {
        app.key(key(KeyCode::Char(c)));
    }
    let rows = live(&mut app);
    assert!(
        rows.iter().any(|r| r.contains("[ ] cache-hit-rate")),
        "{rows:#?}"
    );
    app.key(key(KeyCode::Char(' ')));
    app.key(key(KeyCode::Tab));
    app.key(key(KeyCode::Right)); // a filtered list cannot reorder
    let rows = live(&mut app);
    assert!(rows.iter().any(|r| r.contains("[x] cache-hit-rate")));
    assert!(rows.iter().any(|r| r.ends_with(&format!(
        "75% left / 200k · {} 80%",
        status::glyph(Item::CacheHitRate)
    ))));
    for width in [10, 22, 36, 50, 80, 120] {
        app.resize(width, 40);
        assert!(
            app.live()
                .0
                .iter()
                .all(|l| tiechui_tui::wrap::line_width(l) <= width)
        );
    }
    app.resize(80, 40);
    app.key(key(KeyCode::Enter));
    let saved = app.take_save().unwrap();
    assert_eq!(
        saved,
        StatusLineConfig {
            items: vec![
                Item::CurrentDir,
                Item::ModelWithReasoning,
                Item::GitBranch,
                Item::PermissionMode,
                Item::ContextRemaining,
                Item::ContextWindowSize,
                Item::CacheHitRate,
            ],
            use_theme_colors: false,
            icons: vec![Item::CacheHitRate],
        }
    );
    let rows = live(&mut app);
    assert!(rows.iter().any(|r| r.starts_with('─')));
    assert!(rows.last().is_some_and(|r| r.ends_with(" 80%")));

    command(&mut app, "/statusline");
    app.key(key(KeyCode::Char(' ')));
    for c in "nomatch".chars() {
        app.key(key(KeyCode::Char(c)));
    }
    assert!(
        live(&mut app)
            .iter()
            .any(|r| r.trim_end() == "  No matching items")
    );
    app.key(key(KeyCode::Esc));
    assert_eq!(app.take_save(), None);
    assert!(
        live(&mut app)
            .last()
            .is_some_and(|r| r.starts_with("  /isolated"))
    );
}

#[test]
fn picker_layout_fits_short_and_narrow_terminals() {
    let mut app = app(config(&[Item::Model], &[]));
    command(&mut app, "/statusline");
    app.key(key(KeyCode::Down));
    for height in [10, 14, 20, 24, 30] {
        for width in [10, 36, 50, 80, 120] {
            app.resize(width, height);
            let (lines, cursor) = app.live();
            // The screen shows the bottom rows; the search field must be among them.
            let skip = lines.len().saturating_sub(height);
            let lines = &lines[skip..];
            assert!(
                cursor.is_some_and(|(row, _)| row >= skip),
                "{width}x{height}"
            );
            assert!(
                lines
                    .iter()
                    .all(|l| tiechui_tui::wrap::line_width(l) <= width)
            );
            assert!(cursor.is_some_and(|(_, column)| column < width));
            let rows: Vec<String> = lines.iter().map(|l| plain(&l.spans)).collect();
            if width >= 50 {
                assert!(rows.iter().any(|r| r.contains("enter")), "{rows:#?}");
            }
            if height >= 14 && width >= 50 {
                let prompt = rows.iter().position(|r| r.contains("› Message")).unwrap();
                assert_eq!(rows[prompt - 1].trim(), "");
                assert_eq!(rows[prompt + 1].trim(), "");
                assert!(rows[prompt + 2].contains("STEP_MODEL"));
            }
        }
    }
    app.resize(100, 30);
    insta::assert_snapshot!(dump(&app.live().0));
}
