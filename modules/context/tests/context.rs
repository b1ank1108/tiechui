use tiechui_module_context::snapshot;
use tiechui_plugin_api::{
    CancellationToken, MeasuredMessage, ResponseUsage, SessionView, model::Role, protocol::*,
};

fn response(input: u64, cached: u64, model: &str, reset: bool) -> ResponseUsage {
    ResponseUsage {
        fold: None,
        model: model.into(),
        usage: TokenUsage {
            input,
            cache_read: cached,
            ..Default::default()
        },
        reset,
    }
}

#[test]
fn references_are_not_allocations_and_usage_is_independent() {
    let system = ContextItem {
        label: "instructions".into(),
        category: "Instructions".into(),
        tokens: 20,
        text: "private instructions".into(),
        ..Default::default()
    };
    let view = SessionView {
        input_tokens: 900,
        system: vec![system.clone()],
        references: vec![system],
        messages: vec![MeasuredMessage {
            role: Role::User,
            item: ContextItem {
                tokens: 3,
                ..Default::default()
            },
        }],
        provider_usage: Some(TokenUsage {
            input: 100,
            ..Default::default()
        }),
        ..Default::default()
    };
    let snapshot = snapshot(view, &CancellationToken::new()).unwrap();
    assert_eq!(snapshot.total, 23);
    assert_eq!(snapshot.injections.len(), 2);
    assert_eq!(snapshot.input_tokens, 900);
    assert_eq!(snapshot.provider_usage.unwrap().input, 100);
    assert!(
        !overview(&snapshot)
            .join("\n")
            .contains("private instructions")
    );
}

#[test]
fn cache_reconciles_and_resets_without_inventing_attribution() {
    let report = snapshot(
        SessionView {
            responses: vec![
                response(200, 800, "a", true),
                response(400, 700, "a", false),
                response(1000, 200, "b", false),
                response(1000, 300, "b", true),
            ],
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .unwrap()
    .cache;
    assert_eq!(
        report
            .responses
            .iter()
            .map(|row| row.fresh)
            .collect::<Vec<_>>(),
        [0, 100, 0, 0]
    );
    for row in &report.responses {
        assert_eq!(row.cached + row.fresh + row.unattributed, row.input);
    }
    assert!(
        cache_lines(&report)
            .join("\n")
            .contains("not a bill or causal proof")
    );
}

#[test]
fn rolling_window_keeps_a_preceding_baseline_and_cancellation_is_prompt() {
    let mut responses = vec![response(100, 0, "a", true)];
    responses.extend((1..129).map(|index| response(100 + index, 0, "a", false)));
    let view = SessionView {
        responses,
        omitted_responses: 11,
        unavailable_responses: 3,
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    let report = snapshot(view.clone(), &cancel).unwrap().cache;
    assert_eq!(report.responses.len(), 128);
    assert_eq!(report.omitted, 12);
    assert_eq!(report.unavailable, 3);
    assert_eq!(report.responses[0].fresh, 1);
    cancel.cancel();
    assert!(snapshot(view, &cancel).is_err());
}

#[test]
fn compaction_attribution_reconciles_and_expires_after_four_cold_responses() {
    let mut responses = vec![response(1000, 0, "model", false)];
    let mut first = response(600, 0, "model", false);
    first.fold = Some(tiechui_plugin_api::ContextFold {
        before: 1000,
        after: 500,
        prefix: 100,
        forced: false,
    });
    responses.push(first);
    responses.extend((0..5).map(|_| response(600, 0, "model", false)));
    let report = snapshot(
        SessionView {
            folds: 1,
            reclaimed: 500,
            responses,
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .unwrap()
    .cache;
    assert_eq!(report.responses[1].fresh, 100);
    assert_eq!(report.responses[1].compression, 400);
    assert_eq!(report.responses[5].compression, 0);
    for row in &report.responses {
        assert_eq!(
            row.input,
            row.cached + row.fresh + row.compression + row.unattributed
        );
    }
    let lines = cache_lines(&report).join("\n");
    assert!(lines.contains("1 recorded compression event(s)"));
    assert!(!lines.contains("over cache cooldown"));
    let forced = snapshot(
        SessionView {
            folds: 2,
            forced_folds: 1,
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .unwrap()
    .cache;
    assert!(
        cache_lines(&forced)
            .join("\n")
            .contains("1 over cache cooldown for capacity")
    );
    let mut cold = response(600, 0, "model", false);
    cold.fold = Some(tiechui_plugin_api::ContextFold {
        before: 1000,
        after: 500,
        prefix: 100,
        forced: false,
    });
    let report = snapshot(
        SessionView {
            responses: vec![
                response(1000, 0, "model", false),
                cold,
                response(0, 600, "model", false),
                response(600, 0, "model", false),
            ],
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .unwrap()
    .cache;
    assert_eq!(report.responses.last().unwrap().compression, 0);
}
