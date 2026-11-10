mod support;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_core::text::Line;
use tiechui_protocol::*;
use tiechui_tui::{context::ContextPanel, palette::ColorLevel, theme::Theme, wrap};

fn fixture() -> ContextSnapshot {
    let item = |label: &str, category: &str, tokens, text: &str| ContextItem {
        label: label.into(),
        category: category.into(),
        tokens,
        text: text.into(),
        source: "/project/AGENTS.md · included in System".into(),
        ..Default::default()
    };
    ContextSnapshot {
        model: "fixture/model".into(),
        window: 200_000,
        input_tokens: 24_000,
        total: 10_510,
        provider_usage: Some(TokenUsage {
            input: 1000,
            output: 100,
            cache_read: 20_000,
            ..Default::default()
        }),
        system: vec![
            item(
                "System prompt",
                "System prompt",
                3000,
                "PRIVATE_SYSTEM_BODY",
            ),
            item(
                "project_context",
                "Instructions",
                500,
                "Read existing files before editing.",
            ),
            item("skills", "Skills", 300, "Available skill catalog"),
        ],
        tools: vec![ContextItem {
            json: true,
            group: "Built-in tools".into(),
            ..item(
                "read",
                "Tool definitions",
                200,
                "{\n  \"name\": \"read\",\n  \"input_schema\": {\"type\": \"object\"}\n}",
            )
        }],
        injections: vec![
            item(
                "/project/AGENTS.md",
                "Instruction file",
                500,
                "Referenced instructions",
            ),
            item(
                "Request-only system #1",
                "Request-only",
                3800,
                "PRIVATE_REQUEST",
            ),
        ],
        messages: vec![
            item("user #1", "User / injected messages", 10, "USER_CONTENT"),
            item(
                "assistant #2",
                "Assistant",
                1500,
                "reason\n[thinking signature omitted]\n[image: image/png; data omitted]",
            ),
            item("toolResult #3", "Tool results", 5000, "tool output"),
        ],
        cache: CacheDiagnostics {
            responses: vec![CacheResponse {
                compression: 0,
                input: 21000,
                cached: 20000,
                fresh: 0,
                unattributed: 1000,
            }],
            ..Default::default()
        },
        notices: vec![
            "Category sizes are estimates; provider serialization can differ.".into(),
            "Request differences exclude later provider rewrites.".into(),
        ],
    }
}
fn key(panel: &mut ContextPanel, code: KeyCode) -> bool {
    panel.key(KeyEvent::new(code, KeyModifiers::NONE))
}
fn text(lines: &[Line<'_>]) -> String {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
fn render(panel: &mut ContextPanel, width: usize, rows: usize) -> String {
    text(&panel.render(width, rows, &Theme::fixture(ColorLevel::TrueColor, false)))
}

#[test]
fn snapshots_per_view_at_both_widths_and_palettes() {
    for view in [
        ContextView::Overview,
        ContextView::System,
        ContextView::Tools,
        ContextView::Injections,
        ContextView::Messages,
        ContextView::Cache,
    ] {
        let mut snapshots = vec![];
        for light in [false, true] {
            for width in [40, 80] {
                let theme = Theme::fixture(ColorLevel::TrueColor, light);
                let mut panel = ContextPanel::new(fixture(), view);
                snapshots.push(format!(
                    "{width} columns / {}\n{}",
                    if light { "light" } else { "dark" },
                    support::dump(&panel.render(width, 24, &theme))
                ));
                if view == ContextView::Tools {
                    key(&mut panel, KeyCode::Enter);
                    snapshots.push(format!(
                        "Definition / {width} columns\n{}",
                        support::dump(&panel.render(width, 24, &theme))
                    ));
                }
            }
        }
        insta::assert_snapshot!(format!("context_{view:?}"), snapshots.join("\n\n"));
    }
}

#[test]
fn layered_navigation_search_order_and_zoom_do_not_expose_unopened_bodies() {
    let mut panel = ContextPanel::new(fixture(), ContextView::Overview);
    let full = render(&mut panel, 80, 30);
    assert!(full.contains("Full window"));
    assert!(!full.contains("PRIVATE_"));
    key(&mut panel, KeyCode::Char('z'));
    let zoom = render(&mut panel, 80, 30);
    assert!(zoom.contains("Fit 200k"));
    assert!(full.contains("1.5%") && zoom.contains("1.5%"));
    key(&mut panel, KeyCode::Enter);
    assert!(render(&mut panel, 80, 30).contains("PRIVATE_SYSTEM_BODY"));
    assert!(key(&mut panel, KeyCode::Esc));
    assert!(render(&mut panel, 80, 30).contains("Fit 200k"));
    key(&mut panel, KeyCode::Char('c'));
    assert!(render(&mut panel, 80, 30).contains("Context · Cache"));
    key(&mut panel, KeyCode::Esc);
    key(&mut panel, KeyCode::Left);
    let messages = render(&mut panel, 80, 30);
    assert!(messages.find("user #1").unwrap() < messages.find("assistant #2").unwrap());
    assert!(!messages.contains("USER_CONTENT"));
    key(&mut panel, KeyCode::Char('/'));
    panel.paste("assistant");
    key(&mut panel, KeyCode::Enter);
    let found = render(&mut panel, 40, 24);
    assert!(found.contains("assistant #2"));
    assert!(!found.contains("user #1"));
    key(&mut panel, KeyCode::Enter);
    assert!(render(&mut panel, 80, 30).contains("thinking signature omitted"));
    assert!(key(&mut panel, KeyCode::Esc));
    assert!(render(&mut panel, 40, 24).contains("Search: assistant"));
    assert!(!key(&mut panel, KeyCode::Esc));
}

#[test]
fn pages_keep_bounds_and_exit_hints_on_resize_and_ignore_key_releases() {
    for view in [
        ContextView::Overview,
        ContextView::System,
        ContextView::Tools,
        ContextView::Injections,
        ContextView::Messages,
        ContextView::Cache,
    ] {
        let mut panel = ContextPanel::new(fixture(), view);
        let mut release = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert!(panel.key(release));
        for width in [1, 4, 16, 40, 52, 80] {
            for rows in [1, 2, 3, 8, 12, 24] {
                let screen =
                    panel.render(width, rows, &Theme::fixture(ColorLevel::TrueColor, false));
                assert_eq!(screen.len(), rows, "{view:?} {width}x{rows}");
                assert!(screen.iter().all(|line| wrap::line_width(line) <= width));
                let footer = text(&screen[rows - 1..]);
                assert!(
                    width < 16 || footer.contains("esc"),
                    "{view:?} {width}x{rows}"
                );
            }
        }
    }
}

#[test]
fn unknown_window_overflow_plain_json_and_scrollable_sources() {
    let mut snapshot = fixture();
    snapshot.window = 0;
    let mut panel = ContextPanel::new(snapshot.clone(), ContextView::Overview);
    let output = render(&mut panel, 80, 30);
    assert!(!output.contains("Full window") && !output.contains("Free (estimate)"));
    snapshot.window = 10;
    let mut panel = ContextPanel::new(snapshot.clone(), ContextView::Overview);
    assert!(render(&mut panel, 80, 30).contains("exceeds window"));
    snapshot.messages[0].text = "{\"literal\":true}".into();
    snapshot.messages[0].source = "long source ".repeat(100) + "SOURCE_END";
    let mut panel = ContextPanel::new(snapshot, ContextView::Messages);
    key(&mut panel, KeyCode::Enter);
    let screen = panel.render(40, 12, &Theme::fixture(ColorLevel::TrueColor, false));
    assert!(text(&screen).contains("{\"literal\":true}"));
    assert!(
        screen
            .iter()
            .flat_map(|line| &line.spans)
            .filter(|span| span.content.contains("literal"))
            .all(|span| span.style.fg.is_none())
    );
    key(&mut panel, KeyCode::End);
    assert!(render(&mut panel, 40, 12).contains("SOURCE_END"));
}

#[test]
fn mouse_selects_entries_and_matrix_cells_without_opening_on_a_single_click() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let click = |column, row| MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let mut panel = ContextPanel::new(fixture(), ContextView::Messages);
    let screen = panel.render(80, 24, &Theme::fixture(ColorLevel::TrueColor, false));
    let row = screen
        .iter()
        .position(|line| text(std::slice::from_ref(line)).contains("assistant #2"))
        .unwrap() as u16;
    panel.mouse(click(6, row));
    assert!(!render(&mut panel, 80, 24).contains("thinking signature omitted"));
    panel.mouse(click(6, row));
    assert!(render(&mut panel, 80, 24).contains("thinking signature omitted"));
    let mut panel = ContextPanel::new(fixture(), ContextView::Overview);
    let screen = panel.render(80, 24, &Theme::fixture(ColorLevel::TrueColor, false));
    let row = screen
        .iter()
        .position(|line| text(std::slice::from_ref(line)).starts_with("  ■"))
        .unwrap() as u16;
    panel.mouse(click(2, row));
    panel.mouse(click(2, row));
    assert!(render(&mut panel, 80, 24).contains("PRIVATE_SYSTEM_BODY"));
}

#[test]
fn context_control_paging_matches_page_keys() {
    let mut snapshot = fixture();
    snapshot.messages[0].text = (0..100).map(|n| format!("line {n}\n")).collect();
    let mut control = ContextPanel::new(snapshot.clone(), ContextView::Messages);
    let mut page = ContextPanel::new(snapshot, ContextView::Messages);
    for panel in [&mut control, &mut page] {
        key(panel, KeyCode::Enter);
        render(panel, 40, 16);
    }
    control.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    key(&mut page, KeyCode::PageDown);
    assert_eq!(render(&mut control, 40, 16), render(&mut page, 40, 16));
}

#[test]
fn compaction_cache_rows_at_both_widths_and_palettes() {
    let mut snapshots = vec![];
    for light in [false, true] {
        for width in [40, 80] {
            let mut fixture = fixture();
            fixture.cache.folds = 2;
            fixture.cache.reclaimed = 12000;
            fixture.cache.responses[0].compression = 800;
            fixture.cache.responses[0].unattributed = 200;
            let theme = Theme::fixture(ColorLevel::TrueColor, light);
            let mut panel = ContextPanel::new(fixture, ContextView::Cache);
            snapshots.push(format!(
                "{width} columns / {}\n{}",
                if light { "light" } else { "dark" },
                support::dump(&panel.render(width, 30, &theme))
            ));
        }
    }
    insta::assert_snapshot!("context_compaction_cache", snapshots.join("\n\n"));
}
