use super::*;
use engine::{Candidate, Item, Source, candidates, commit, prepare, records, view};
use messages::{project, signed, tokens};
use state::{ENTRY, State};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![plain(text)],
        opaque: None,
    }
}
fn assistant(content: Vec<Content>) -> Message {
    Message {
        role: Role::Assistant,
        content,
        opaque: None,
    }
}
fn tool(call: &str, text: &str) -> Message {
    Message {
        role: Role::Tool,
        content: vec![Content::ToolResult(ToolResult {
            call_id: call.into(),
            content: vec![plain(text)],
            is_error: false,
        })],
        opaque: None,
    }
}
fn call(id: &str, name: &str, args: Value) -> Content {
    Content::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args,
        opaque: None,
    })
}
fn message_size(message: &Message) -> u64 {
    fn content(parts: &[Content]) -> u64 {
        parts
            .iter()
            .map(|p| match p {
                Content::Text { text, .. } | Content::Thinking { text, .. } => tokens(text),
                Content::ToolCall(c) => tokens(&c.arguments.to_string()) + tokens(&c.name),
                Content::ToolResult(r) => content(&r.content),
                Content::Image { .. } => 1600,
            })
            .sum()
    }
    4 + content(&message.content)
}
fn history() -> Vec<Message> {
    vec![
        message(Role::User, "Earlier task: review completed work"),
        assistant(vec![
            Content::Thinking {
                text: "PRIVATE_REASONING".into(),
                redacted: false,
                opaque: Some(OpaqueData("signature".into())),
            },
            plain("Review two completed reports"),
            call("a", "read", json!({"path":"a.txt"})),
            call("b", "read", json!({"path":"b.txt"})),
        ]),
        tool("a", &"OLD_REPORT_A\n".repeat(1800)),
        tool("b", &"OLD_REPORT_B\n".repeat(1800)),
        message(Role::Assistant, "Completed earlier work"),
        message(Role::User, &"CURRENT_EVIDENCE ".repeat(1500)),
        message(Role::Assistant, "Current task continues"),
        message(Role::User, "Keep current constraints"),
        message(Role::Assistant, "I will preserve the constraints"),
        message(Role::User, "LATEST_REQUEST"),
    ]
}
fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: String::new(),
        input_schema: json!({}),
    }
}
fn context(messages: Vec<Message>) -> ProjectionContext {
    ProjectionContext {
        tool_details: BTreeMap::new(),
        branch: BranchView {
            session_id: "fixture".into(),
            leaf: "leaf".into(),
            cwd: PathBuf::from("/fixture"),
            parent_session: None,
            entries: vec![],
        },
        history: messages.clone(),
        messages,
        tools: ["read", "read_history", "compact_history"]
            .into_iter()
            .map(spec)
            .collect(),
        system: "Static system prompt".into(),
        window: 200000,
        estimate_message: message_size,
        estimate_tools: |tools| tools.iter().map(|t| tokens(&t.description) + 8).sum(),
        cache_cold: false,
    }
}
fn summary() -> String {
    "Completed review of two reports. Both outputs were examined; preserve the user's current constraints and continue the latest task. Original report text remains available through read_history.".into()
}
fn config() -> CompactionConfig {
    CompactionConfig::default()
}
fn all(p: &engine::Prepared) -> Vec<Candidate> {
    candidates(p, config().compress.min_compress_range, 25000, None)
}
/// The candidate containing a part whose text includes `needle`.
fn holding(p: &engine::Prepared, list: &[Candidate], needle: &str) -> Candidate {
    list.iter()
        .find(|c| match &c.source {
            Source::Messages(ids) => p
                .projection
                .parts
                .iter()
                .any(|v| ids.contains(&v.id) && v.text.contains(needle)),
            Source::Summaries(_) => false,
        })
        .cloned()
        .unwrap_or_else(|| panic!("no candidate holds {needle}"))
}
fn fold(p: &engine::Prepared, candidate: &Candidate, text: &str, id: &str) -> State {
    commit(
        p,
        &[Item {
            candidate: &candidate.id,
            source: &candidate.source,
            summary: text,
            title: None,
        }],
        id,
    )
}
fn persist(ctx: &mut ProjectionContext, state: &State) {
    ctx.branch.entries.push(protocol::BranchEntry {
        id: format!("state-{}", ctx.branch.entries.len()),
        parent_id: None,
        timestamp: 0,
        content: None,
        custom: Some((ENTRY.into(), json!(state))),
    });
}
fn output(p: &engine::Prepared) -> Vec<Message> {
    view(&p.projection, &p.state, &CancellationToken::new())
        .unwrap()
        .into_iter()
        .map(|m| m.message)
        .collect()
}
fn service(config: CompactionConfig) -> Arc<Service> {
    Arc::new(Service {
        config,
        snapshot: Mutex::new(None),
        cadence: Mutex::new(nudge::Cadence::default()),
        search: Mutex::new(search::HistorySearch::default()),
        queued: Mutex::new(None),
        issued: Mutex::new(VecDeque::new()),
    })
}
type Append = Arc<dyn Fn(tools::CustomSessionEntry) -> Result<(), ToolError> + Send + Sync>;
fn tool_context(ctx: &ProjectionContext, append: Append) -> ToolContext {
    ToolContext {
        invoker: None,
        branch: Arc::new(ctx.branch.entries.clone()),
        parent_session: None,
        cwd: ctx.branch.cwd.clone(),
        cancel: CancellationToken::new(),
        updates: Arc::new(|_| {}),
        append_entry: append,
        epoch: protocol::SessionEpoch(0),
        scope: tools::ExecutionScope {
            thread_id: protocol::ThreadId(ctx.branch.session_id.clone()),
            turn_id: protocol::TurnId("turn".into()),
        },
        origin: Default::default(),
    }
}
fn history_tool(service: Arc<Service>, name: &str) -> history::HistoryTool {
    history::HistoryTool {
        service,
        spec: spec(name),
    }
}
async fn run_with(
    service: &Arc<Service>,
    ctx: &ProjectionContext,
    id: &str,
    name: &str,
    args: Value,
    append: Append,
) -> Result<(Value, bool), ToolError> {
    let output = history_tool(service.clone(), name)
        .run(
            ToolCall {
                id: id.into(),
                name: name.into(),
                arguments: args,
                opaque: None,
            },
            tool_context(ctx, append),
        )
        .await?;
    let text = messages::text(&output.content);
    Ok((serde_json::from_str(&text).unwrap(), output.is_error))
}
async fn run(
    service: &Arc<Service>,
    ctx: &ProjectionContext,
    name: &str,
    args: Value,
) -> (Value, bool) {
    run_with(service, ctx, "call", name, args, Arc::new(|_| Ok(())))
        .await
        .unwrap()
}
async fn snapshot(service: &Arc<Service>, ctx: &ProjectionContext) {
    service
        .project(ctx.clone(), CancellationToken::new())
        .await
        .unwrap();
}
fn compact_args(id: &Value, text: &str) -> Value {
    json!({"ranges":[{"candidate_id": id, "summary": text}]})
}

#[test]
fn identity_refs_occurrences_transient_context_and_opaque_bytes() {
    let cancel = CancellationToken::new();
    let mut messages = history();
    messages.insert(0, message(Role::System, "SYSTEM"));
    messages.push(messages[0].clone());
    let ctx = context(messages.clone());
    let p = prepare(&ctx, &config(), &cancel).unwrap();
    assert_eq!(
        serde_json::to_vec(&output(&p)).unwrap(),
        serde_json::to_vec(&messages).unwrap()
    );
    let mut moved = ctx.clone();
    moved.messages.remove(0);
    moved.messages.push(message(Role::User, "transient"));
    let p2 = prepare(&moved, &config(), &cancel).unwrap();
    assert_eq!(p.state.refs, p2.state.refs);
    assert!(
        p2.protected
            .contains_key(&p2.projection.parts.last().unwrap().id)
    );
    let identical = project(
        &[message(Role::User, "same"), message(Role::User, "same")],
        &cancel,
    )
    .unwrap();
    assert_ne!(identical.parts[0].id, identical.parts[1].id);
}
#[test]
fn compaction_preserves_parallel_pairs_raw_records_reload_rewind_and_fork() {
    let cancel = CancellationToken::new();
    let mut ctx = context(history());
    let p = prepare(&ctx, &config(), &cancel).unwrap();
    let candidate = holding(&p, &all(&p), "OLD_REPORT_A");
    assert!(engine::changed(&p, &candidate.source).is_none());
    let state = fold(&p, &candidate, &summary(), "compaction");
    persist(&mut ctx, &state);
    let restored = prepare(&ctx, &config(), &cancel).unwrap();
    let result = output(&restored);
    let bytes = serde_json::to_string(&result).unwrap();
    assert!(!bytes.contains("OLD_REPORT"));
    assert!(bytes.contains("LATEST_REQUEST"));
    assert!(bytes.contains("Earlier task"), "first user request stays");
    assert_eq!(bytes.matches(&summary()).count(), 1);
    assert_eq!(ctx.history, history());
    ctx.branch.session_id = "fork-remapped".into();
    assert_eq!(output(&prepare(&ctx, &config(), &cancel).unwrap()), result);
    ctx.branch.entries.clear();
    assert_eq!(
        output(&prepare(&ctx, &config(), &cancel).unwrap()),
        history()
    );
    persist(&mut ctx, &state);
    ctx.messages = ctx.messages[3..].to_vec();
    let partial = prepare(&ctx, &config(), &cancel).unwrap();
    assert_eq!(
        engine::changed(&partial, &Source::Summaries(vec!["b1".into()])),
        Some("source_missing")
    );
    assert_eq!(
        engine::changed(&partial, &candidate.source),
        Some("source_missing")
    );
    assert_eq!(output(&partial), ctx.messages);
}
#[test]
fn candidates_follow_turns_split_long_runs_safely_and_keep_stable_ids() {
    let cancel = CancellationToken::new();
    let mut turn = vec![message(Role::User, "Long autonomous task")];
    for i in 0..6 {
        turn.push(assistant(vec![
            Content::Thinking {
                text: format!("step {i}"),
                redacted: false,
                opaque: None,
            },
            call(&format!("s{i}"), "bash", json!({"step": i})),
        ]));
        turn.push(tool(
            &format!("s{i}"),
            &format!("STEP_{i} {}", "output ".repeat(1500)),
        ));
    }
    turn.push(message(Role::User, "LATEST"));
    let ctx = context(turn);
    let p = prepare(&ctx, &config(), &cancel).unwrap();
    let list = candidates(&p, 5000, 4000, None);
    assert!(list.len() >= 2, "{list:?}");
    for c in &list {
        let Source::Messages(ids) = &c.source else {
            panic!("raw history expected")
        };
        let mut set: std::collections::BTreeSet<_> = ids.iter().cloned().collect();
        messages::integrity(&p.projection.parts, &mut set);
        assert_eq!(set.len(), ids.len(), "candidate splits a pair");
        assert!(!c.first.is_empty() && !c.last.is_empty());
    }
    assert!(list[0].first.starts_with("user: Long autonomous task"));
    assert!(list.last().unwrap().last.starts_with("tool bash: STEP_"));
    let again = candidates(
        &prepare(&ctx, &config(), &cancel).unwrap(),
        5000,
        4000,
        None,
    );
    assert_eq!(
        list.iter().map(|c| &c.id).collect::<Vec<_>>(),
        again.iter().map(|c| &c.id).collect::<Vec<_>>()
    );
    let (a, _) = engine::span(&p, &list[1].start).unwrap();
    let (_, b) = engine::span(&p, &list[1].end).unwrap();
    let narrowed = candidates(&p, 5000, 4000, Some((a, b)));
    assert_eq!(narrowed.len(), 1);
    assert_eq!(narrowed[0].id, list[1].id);
}
#[test]
fn signed_partial_envelopes_are_never_offered_and_opaque_results_withdraw_pairs() {
    let cancel = CancellationToken::new();
    let mut ctx = context(history());
    if let Content::Text { opaque, .. } = &mut ctx.messages[1].content[1] {
        *opaque = Some(OpaqueData("signature".into()));
    }
    ctx.history = ctx.messages.clone();
    assert!(signed(&ctx.messages[1]));
    let p = prepare(&ctx, &config(), &cancel).unwrap();
    let owner: Vec<_> = p
        .projection
        .parts
        .iter()
        .filter(|v| v.owner == 1)
        .map(|v| v.id.clone())
        .collect();
    let call_a = p
        .projection
        .parts
        .iter()
        .position(|v| v.call == "a")
        .unwrap();
    let narrowed = candidates(&p, 0, 25000, Some((call_a, p.projection.parts.len() - 1)));
    let before = request_size(&ctx, &view(&p.projection, &p.state, &cancel).unwrap());
    let described = history::describe(&ctx, &p, &config(), before, &narrowed, &cancel).unwrap();
    for (c, _) in described {
        let Source::Messages(ids) = &c.source else {
            continue;
        };
        let inside = owner.iter().filter(|id| ids.contains(id)).count();
        assert!(
            inside == 0 || inside == owner.len(),
            "partial signed message offered"
        );
    }
    let mut ctx = context(history());
    if let Content::ToolResult(r) = &mut ctx.messages[2].content[0] {
        r.content.push(Content::Image {
            mime_type: "image/png".into(),
            data: "opaque".into(),
        });
    }
    ctx.history = ctx.messages.clone();
    let p = prepare(&ctx, &config(), &cancel).unwrap();
    assert!(
        p.projection
            .parts
            .iter()
            .filter(|p| p.call == "a")
            .all(|part| p.protected.contains_key(&part.id))
    );
}
#[tokio::test]
async fn protection_recent_zero_latest_intent_and_low_savings() {
    let mut config = CompactionConfig {
        preserve_recent_messages: 0,
        preserve_recent_tokens: 0,
        ..Default::default()
    };
    let ctx = context(history());
    let p = prepare(&ctx, &config, &CancellationToken::new()).unwrap();
    assert_eq!(
        p.protected.get(&p.projection.parts.last().unwrap().id),
        Some(&"latest_user_request")
    );
    config.protected_tools = vec!["read*".into()];
    let p = prepare(&ctx, &config, &CancellationToken::new()).unwrap();
    assert!(
        p.projection
            .parts
            .iter()
            .filter(|v| v.tool == "read")
            .all(|v| p.protected.get(&v.id) == Some(&"protected_tool"))
    );
    config.protected_tools.clear();
    config.compress.min_net_savings_tokens = 100000;
    let service = service(config);
    snapshot(&service, &ctx).await;
    let p = prepare(&ctx, &service.config, &CancellationToken::new()).unwrap();
    let id = holding(&p, &all(&p), "OLD_REPORT_A").id;
    let (result, error) = run_with(
        &service,
        &ctx,
        "c",
        "compact_history",
        compact_args(&json!(id), &summary()),
        Arc::new(|_| panic!("low-savings state must not be queued")),
    )
    .await
    .unwrap();
    assert!(error);
    assert_eq!(result["applied"], false);
    assert_eq!(result["error"]["code"], "INSUFFICIENT_SAVINGS");
    assert_eq!(result["error"]["minimum_tokens"], 100000);
    assert!(result["error"]["items"][0]["summary_limit_chars"].is_number());
}
#[tokio::test]
async fn summary_merges_restore_source_graph_and_carry_the_summary_once() {
    let cancel = CancellationToken::new();
    let config = config();
    let mut ctx = context(
        std::iter::once(message(Role::User, "Initial task context"))
            .chain(
                (0..5)
                    .map(|i| message(Role::User, &format!("STAGE_{i} {}", "data ".repeat(2500))))
                    .chain(history().into_iter().skip(5)),
            )
            .collect(),
    );
    for i in 0..5 {
        let p = prepare(&ctx, &config, &cancel).unwrap();
        let candidate = holding(&p, &all(&p), &format!("STAGE_{i}"));
        let state = fold(
            &p,
            &candidate,
            &"Historical findings and exact constraints for the completed stage. ".repeat(30),
            &format!("c{i}"),
        );
        assert_eq!(state.blocks.len(), i + 1);
        persist(&mut ctx, &state);
    }
    let p = prepare(&ctx, &config, &cancel).unwrap();
    let merge = all(&p)
        .into_iter()
        .find(|c| matches!(c.source, Source::Summaries(_)))
        .unwrap();
    assert_eq!(
        merge.source,
        Source::Summaries((1..=5).map(|i| format!("b{i}")).collect())
    );
    let state = fold(&p, &merge, &summary(), "merged");
    assert_eq!(state.blocks[5].tier, 2);
    assert_eq!(state.blocks[5].children.len(), 5);
    persist(&mut ctx, &state);
    ctx.messages.push(assistant(vec![call(
        "merged",
        "compact_history",
        compact_args(&json!(merge.id), &summary()),
    )]));
    ctx.messages.push(tool("merged", "{\"ok\":true}"));
    ctx.history = ctx.messages.clone();
    let p = prepare(&ctx, &config, &cancel).unwrap();
    assert_eq!(
        serde_json::to_string(&output(&p))
            .unwrap()
            .matches(&summary())
            .count(),
        1
    );
    let source: String = records(&p, "b6", false)
        .unwrap()
        .into_iter()
        .map(|r| r.text)
        .collect();
    assert!(source.contains("STAGE_4") && source.contains("STAGE_0"));
    let own = records(&p, "b6", true).unwrap();
    assert_eq!(own.len(), 1);
    assert_eq!(own[0].text, summary());
    assert_eq!(own[0].source_refs.as_ref().unwrap().len(), 5);
    assert_eq!(records(&p, "m00002", true).err(), Some("view_unsupported"));
}
#[tokio::test]
async fn storage_failure_cancellation_corruption_and_read_pause_never_publish() {
    let service = service(config());
    let mut ctx = context(history());
    assert!(
        service
            .project(ctx.clone(), CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
    let (status, _) = run(&service, &ctx, "context_status", json!({})).await;
    let id = status["candidates"][0]["id"].clone();
    let error = run_with(
        &service,
        &ctx,
        "c",
        "compact_history",
        compact_args(&id, &summary()),
        Arc::new(|_| Err(ToolError("save failed".into()))),
    )
    .await
    .unwrap_err();
    assert!(error.0.contains("save failed"));
    assert!(
        service
            .project(ctx.clone(), CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
    let toolctx = tool_context(&ctx, Arc::new(|_| panic!("cancelled write")));
    toolctx.cancel.cancel();
    assert!(
        history_tool(service.clone(), "compact_history")
            .run(
                ToolCall {
                    id: "c".into(),
                    name: "compact_history".into(),
                    arguments: compact_args(&id, &summary()),
                    opaque: None,
                },
                toolctx
            )
            .await
            .is_err()
    );
    let p = prepare(&ctx, &service.config, &CancellationToken::new()).unwrap();
    persist(&mut ctx, &fold(&p, &all(&p)[0], &summary(), "c"));
    ctx.tools.retain(|t| t.name != "read_history");
    assert!(
        service
            .project(ctx.clone(), CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
    let (status, _) = run(&service, &ctx, "context_status", json!({})).await;
    assert_eq!(status["maintenance"]["state"], "paused");
    let (result, error) = run(
        &service,
        &ctx,
        "compact_history",
        compact_args(&id, &summary()),
    )
    .await;
    assert!(error);
    assert_eq!(result["error"]["code"], "RECOVERY_UNAVAILABLE");
    ctx.tools = context(vec![]).tools;
    assert!(
        service
            .project(ctx.clone(), CancellationToken::new())
            .await
            .unwrap()
            .is_some()
    );
    ctx.branch.entries.last_mut().unwrap().custom = Some((ENTRY.into(), json!({"version":1})));
    assert!(
        service
            .project(ctx, CancellationToken::new())
            .await
            .unwrap_err()
            .0
            .contains("Invalid saved")
    );
}
#[tokio::test]
async fn status_reports_budget_pages_and_rejects_unknown_fields() {
    let mut config = config();
    config.compress.min_compress_range = 0;
    let service = service(config);
    let mut messages = vec![message(Role::User, "Initial")];
    for i in 0..9 {
        messages.push(message(
            Role::User,
            &format!("TURN_{i} {}", "words ".repeat(3000)),
        ));
        messages.push(message(Role::Assistant, &format!("Done {i}")));
    }
    messages.push(message(Role::User, "LATEST"));
    let ctx = context(messages);
    snapshot(&service, &ctx).await;
    let (status, error) = run(&service, &ctx, "context_status", json!({})).await;
    assert!(!error);
    assert_eq!(status["budget"]["model_window_tokens"], 200000);
    assert_eq!(status["budget"]["window_source"], "model");
    assert_eq!(status["budget"]["pressure"], "normal");
    assert_eq!(status["maintenance"]["state"], "ready");
    assert_eq!(status["candidates"].as_array().unwrap().len(), 6);
    assert_eq!(status["has_more"], true);
    assert!(status["protected"]["count_by_reason"]["latest_user_request"] == 1);
    let next = status["next_cursor"].clone();
    let (page, _) = run(&service, &ctx, "context_status", json!({"cursor": next})).await;
    assert!(!page["candidates"].as_array().unwrap().is_empty());
    assert_ne!(page["candidates"][0]["id"], status["candidates"][0]["id"]);
    let (stale, error) = run(
        &service,
        &ctx,
        "context_status",
        json!({"cursor": next, "range": {"start_ref": "m00001", "end_ref": "m00004"}}),
    )
    .await;
    assert!(error);
    assert_eq!(stale["error"]["code"], "STALE_CURSOR");
    for args in [
        json!({"snapshot": "x"}),
        json!({"max_tokens": 100}),
        json!({"range": {"start_ref": "m00004", "end_ref": "m00002"}}),
        json!({"range": {"start_ref": "m00002", "end_ref": "b1"}}),
    ] {
        let (result, error) = run(&service, &ctx, "context_status", args).await;
        assert!(error);
        assert_eq!(result["error"]["code"], "INVALID_ARGUMENT", "{result}");
    }
    let (small, _) = run(&service, &ctx, "context_status", json!({"max_tokens": 256})).await;
    assert!(small["candidates"].as_array().unwrap().len() < 6);
}
#[tokio::test]
async fn compact_history_is_atomic_idempotent_and_survives_unrelated_steps() {
    let mut config = config();
    config.compress.min_compress_range = 0;
    config.output.enabled = false;
    let service = service(config);
    let mut messages = vec![message(Role::User, "Initial")];
    for i in 0..4 {
        messages.push(message(
            Role::User,
            &format!("TURN_{i} {}", "words ".repeat(3000)),
        ));
        messages.push(message(Role::Assistant, &format!("Done {i}")));
    }
    messages.push(message(Role::User, "LATEST"));
    let mut ctx = context(messages);
    snapshot(&service, &ctx).await;
    let (status, _) = run(&service, &ctx, "context_status", json!({})).await;
    let ids: Vec<_> = status["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].clone())
        .collect();
    assert!(ids.len() >= 2);
    let (result, error) = run(
        &service,
        &ctx,
        "compact_history",
        json!({"ranges":[{"candidate_id": ids[0], "summary": summary()}, {"candidate_id": "c_unknown", "summary": summary()}]}),
    )
    .await;
    assert!(error);
    assert_eq!(result["applied"], false);
    assert_eq!(result["error"]["code"], "CANDIDATE_CHANGED");
    assert_eq!(result["error"]["items"][0]["candidate_id"], "c_unknown");
    let (result, _) = run(
        &service,
        &ctx,
        "compact_history",
        json!({"ranges":[{"candidate_id": ids[0], "summary": "short"}]}),
    )
    .await;
    assert_eq!(result["error"]["code"], "INVALID_ARGUMENT");
    assert_eq!(result["error"]["items"][0]["reason"], "summary_length");
    // A business step after status keeps the listed sources valid.
    ctx.messages
        .push(message(Role::Assistant, "Unrelated progress"));
    ctx.history = ctx.messages.clone();
    snapshot(&service, &ctx).await;
    let saved = Arc::new(Mutex::new(vec![]));
    let sink = saved.clone();
    let (receipt, error) = run_with(
        &service,
        &ctx,
        "commit",
        "compact_history",
        json!({"ranges":[{"candidate_id": ids[0], "summary": summary(), "title": "First turn"}, {"candidate_id": ids[1], "summary": summary()}]}),
        Arc::new(move |entry| {
            sink.lock().unwrap().push(entry.data);
            Ok(())
        }),
    )
    .await
    .unwrap();
    assert!(!error, "{receipt}");
    assert_eq!(receipt["blocks"].as_array().unwrap().len(), 2);
    assert_eq!(receipt["blocks"][0]["candidate_id"], ids[0]);
    let effect = &receipt["effect"];
    assert_eq!(
        effect["net_saved_tokens_estimate"],
        effect["before_tokens_estimate"].as_u64().unwrap()
            - effect["after_tokens_estimate"].as_u64().unwrap()
    );
    assert!(effect["common_prefix_tokens_estimate"].as_u64().unwrap() > 0);
    assert_eq!(saved.lock().unwrap().len(), 1, "one state for the batch");
    let state: State = serde_json::from_value(saved.lock().unwrap()[0].clone()).unwrap();
    persist(&mut ctx, &state);
    let (retry, _) = run_with(
        &service,
        &ctx,
        "commit",
        "compact_history",
        json!({"ranges":[{"candidate_id": ids[0], "summary": summary()}]}),
        Arc::new(|_| panic!("a retried commit must not append again")),
    )
    .await
    .unwrap();
    assert_eq!(retry["duplicate"], true);
    assert_eq!(receipt["maintenance"]["state"], "cooldown");
    let (deferred, _) = run(
        &service,
        &ctx,
        "compact_history",
        compact_args(&json!("c_any"), &summary()),
    )
    .await;
    assert_eq!(deferred["error"]["code"], "MAINTENANCE_DEFERRED");
    assert_eq!(deferred["error"]["remaining_business_requests"], 3);
    let (status, _) = run(&service, &ctx, "context_status", json!({})).await;
    assert_eq!(status["maintenance"]["state"], "cooldown");
    assert!(status["candidates"].as_array().unwrap().is_empty());
    ctx.messages.push(message(Role::User, "Next request"));
    ctx.history = ctx.messages.clone();
    snapshot(&service, &ctx).await;
    let (again, _) = run(
        &service,
        &ctx,
        "compact_history",
        compact_args(&ids[0], &summary()),
    )
    .await;
    assert_eq!(again["error"]["code"], "CANDIDATE_CHANGED");
    assert_eq!(again["error"]["items"][0]["reason"], "already_compacted");
}
#[test]
fn cooldown_counts_business_responses_and_ends_at_user_input() {
    let mut state = State {
        maintenance: Some(state::Maintenance {
            at: 1,
            forced: false,
        }),
        ..Default::default()
    };
    let maintenance = assistant(vec![call("s", "context_status", json!({}))]);
    let business = assistant(vec![call("b", "bash", json!({}))]);
    let mut history = vec![message(Role::User, "task"), maintenance.clone()];
    assert_eq!(nudge::cooling(&state, &history), Some(3));
    history.extend([tool("s", "{}"), business.clone(), tool("b", "ok")]);
    assert_eq!(nudge::cooling(&state, &history), Some(2));
    history.extend([business.clone(), message(Role::Assistant, "done")]);
    assert_eq!(nudge::cooling(&state, &history), None);
    history.truncate(3);
    history.push(message(Role::User, "new input"));
    assert_eq!(nudge::cooling(&state, &history), None);
    state.maintenance = None;
    assert_eq!(nudge::cooling(&state, &history[..2]), None);
}
#[tokio::test]
async fn request_time_output_previews_are_recorded_once_and_respect_cooldown() {
    let mut config = config();
    config.nudge.max_context_limit_pct = 0.99;
    config.nudge.emergency_threshold_pct = 1.0;
    let service = service(config);
    let large = |id: &str, marker: &str| {
        vec![
            assistant(vec![call(id, "bash", json!({}))]),
            tool(id, &format!("{marker}\n").repeat(3000)),
        ]
    };
    let mut ctx = context(
        [message(Role::User, "first")]
            .into_iter()
            .chain(large("x", "FIRST_OUTPUT"))
            .chain([message(Role::User, "second")])
            .collect(),
    );
    let projected = service
        .project(ctx.clone(), CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(projected.entries.len(), 1);
    let state: State = serde_json::from_value(projected.entries[0].1.clone()).unwrap();
    assert_eq!(state.outputs.len(), 1);
    assert!(state.fold.is_some());
    assert!(!state.maintenance.as_ref().unwrap().forced);
    let text = serde_json::to_string(&projected.messages[2].message).unwrap();
    assert!(text.contains("read_history"));
    persist(&mut ctx, &state);
    let again = service
        .project(ctx.clone(), CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert!(
        again.entries.is_empty(),
        "recorded previews are not recorded again"
    );
    assert_eq!(again.messages[2].message, projected.messages[2].message);
    // A new user turn ends the cooldown; outputs from the previous turn become eligible together.
    ctx.messages.extend(large("y", "SECOND_OUTPUT"));
    ctx.messages.push(message(Role::User, "third"));
    ctx.history = ctx.messages.clone();
    let next = service
        .project(ctx.clone(), CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    let state: State = serde_json::from_value(next.entries[0].1.clone()).unwrap();
    assert_eq!(state.outputs.len(), 2);
    persist(&mut ctx, &state);
    // While cooling, pending previews wait unless the provider cache is cold or capacity presses.
    let mut cooling = context(ctx.history[..4].to_vec());
    let state = State {
        refs: state.refs.clone(),
        maintenance: Some(state::Maintenance {
            at: cooling.history.len(),
            forced: false,
        }),
        ..Default::default()
    };
    persist(&mut cooling, &state);
    let held = service
        .project(cooling.clone(), CancellationToken::new())
        .await
        .unwrap();
    assert!(held.is_none(), "cooling holds previews back");
    let mut cold = cooling.clone();
    cold.cache_cold = true;
    let cleared = service
        .project(cold, CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    let state: State = serde_json::from_value(cleared.entries[0].1.clone()).unwrap();
    assert_eq!(state.outputs.len(), 1);
    assert!(!state.maintenance.unwrap().forced);
    cooling.window = 4000;
    let pressed = service
        .project(cooling, CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    let state: State = serde_json::from_value(pressed.entries[0].1.clone()).unwrap();
    assert!(state.maintenance.unwrap().forced);
}
#[tokio::test]
async fn capacity_pressure_overrides_cooldown_and_is_marked_forced() {
    let mut config = CompactionConfig {
        preserve_recent_messages: 0,
        preserve_recent_tokens: 0,
        ..Default::default()
    };
    config.compress.min_compress_range = 0;
    config.output.enabled = false;
    let service = service(config);
    let mut messages = vec![message(Role::User, "Initial")];
    for i in 0..4 {
        messages.push(message(
            Role::User,
            &format!("TURN_{i} {}", "words ".repeat(3000)),
        ));
        messages.push(message(Role::Assistant, &format!("Done {i}")));
    }
    messages.push(message(Role::User, "LATEST"));
    let mut ctx = context(messages);
    snapshot(&service, &ctx).await;
    let (status, _) = run(&service, &ctx, "context_status", json!({})).await;
    let ids: Vec<_> = status["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].clone())
        .collect();
    let saved = Arc::new(Mutex::new(vec![]));
    let sink = saved.clone();
    run_with(
        &service,
        &ctx,
        "first",
        "compact_history",
        compact_args(&ids[0], &summary()),
        Arc::new(move |entry| {
            sink.lock().unwrap().push(entry.data);
            Ok(())
        }),
    )
    .await
    .unwrap();
    let state: State = serde_json::from_value(saved.lock().unwrap()[0].clone()).unwrap();
    persist(&mut ctx, &state);
    ctx.window = 12000;
    snapshot(&service, &ctx).await;
    let sink = saved.clone();
    let (receipt, error) = run_with(
        &service,
        &ctx,
        "second",
        "compact_history",
        compact_args(&ids[1], &summary()),
        Arc::new(move |entry| {
            sink.lock().unwrap().push(entry.data);
            Ok(())
        }),
    )
    .await
    .unwrap();
    assert!(!error, "{receipt}");
    let state: State = serde_json::from_value(saved.lock().unwrap()[1].clone()).unwrap();
    assert!(state.maintenance.as_ref().unwrap().forced);
    let fold = service.fold(ENTRY, &json!(state)).unwrap();
    assert!(fold.forced);
}
#[tokio::test]
async fn search_reports_positions_view_state_and_read_args_that_resume_reading() {
    let mut config = CompactionConfig {
        preserve_recent_messages: 0,
        preserve_recent_tokens: 0,
        ..Default::default()
    };
    config.compress.min_compress_range = 0;
    config.output.enabled = false;
    let service = service(config);
    let ctx = context(vec![
        message(Role::User, "Initial"),
        message(
            Role::User,
            &format!(
                "{}修复登录验证失败，保留用户权限边界。{}",
                "前置日志".repeat(800),
                "后续".repeat(800)
            ),
        ),
        message(
            Role::Assistant,
            &format!("unrelated {}", "noise ".repeat(1000)),
        ),
        message(
            Role::Assistant,
            "Authentication middleware validates requests.",
        ),
        message(
            Role::Assistant,
            "Authentication middleware validates requests.",
        ),
        message(Role::User, "Current request"),
    ]);
    snapshot(&service, &ctx).await;
    for query in ["登录验证", "authentcation", "authentication"] {
        let (result, error) = run(
            &service,
            &ctx,
            "search_history",
            json!({"query": query, "limit": 2}),
        )
        .await;
        assert!(!error);
        assert!(!result["results"].as_array().unwrap().is_empty(), "{query}");
        assert!(result.to_string().len() < 2000);
    }
    let (result, _) = run(
        &service,
        &ctx,
        "search_history",
        json!({"query": "middleware"}),
    )
    .await;
    assert_eq!(
        result["results"].as_array().unwrap().len(),
        1,
        "identical records merge"
    );
    let (result, _) = run(
        &service,
        &ctx,
        "search_history",
        json!({"query": "登录验证"}),
    )
    .await;
    let hit = &result["results"][0];
    assert_eq!(hit["ref"], "m00002");
    assert_eq!(hit["role"], "user");
    assert_eq!(hit["in_view"], true);
    let (page, error) = run(&service, &ctx, "read_history", hit["read_args"].clone()).await;
    assert!(!error, "{page}");
    let start = page["records"][0]["span"]["start"].as_u64().unwrap();
    assert!(start > 0);
    assert!(
        page["records"][0]["text"]
            .as_str()
            .unwrap()
            .contains("登录验证")
    );
    let p = prepare(&ctx, &service.config, &CancellationToken::new()).unwrap();
    let mut folded = ctx.clone();
    persist(
        &mut folded,
        &fold(&p, &holding(&p, &all(&p), "登录验证"), &summary(), "c"),
    );
    snapshot(&service, &folded).await;
    let (result, _) = run(
        &service,
        &folded,
        "search_history",
        json!({"query": "登录验证", "scope": "messages"}),
    )
    .await;
    assert_eq!(result["results"][0]["in_view"], false);
    assert_eq!(result["results"][0]["covered_by"], "b1");
    let (result, _) = run(
        &service,
        &folded,
        "search_history",
        json!({"query": "reports", "scope": "summaries"}),
    )
    .await;
    assert_eq!(result["results"][0]["kind"], "summary");
    assert_eq!(result["results"][0]["read_args"]["view"], "summary");
    let (bad, error) = run(&service, &folded, "search_history", json!({"query": " "})).await;
    assert!(error);
    assert_eq!(bad["error"]["code"], "INVALID_ARGUMENT");
    let sibling = context(vec![message(Role::User, "Sibling")]);
    snapshot(&service, &sibling).await;
    let (result, _) = run(
        &service,
        &sibling,
        "search_history",
        json!({"query": "登录验证"}),
    )
    .await;
    assert!(result["results"].as_array().unwrap().is_empty());
}
#[tokio::test]
async fn read_history_pages_without_gaps_or_split_characters() {
    let mut config = config();
    config.output.enabled = false;
    let service = service(config);
    let line: String = (0..6000)
        .map(|i| if i % 3 == 0 { '界' } else { 'a' })
        .collect();
    let ctx = context(vec![
        message(Role::User, "Initial"),
        assistant(vec![call("x", "bash", json!({"cmd": "dump"}))]),
        tool("x", &line),
        message(Role::User, "LATEST"),
    ]);
    snapshot(&service, &ctx).await;
    let (first, error) = run(
        &service,
        &ctx,
        "read_history",
        json!({"ref": "m00003", "max_tokens": 512}),
    )
    .await;
    assert!(!error, "{first}");
    assert_eq!(first["records"][0]["call_ref"], "m00002");
    assert_eq!(first["records"][0]["tool"], "bash");
    let mut text = String::new();
    let mut page = first;
    let mut previous_end = 0;
    loop {
        let record = &page["records"][0];
        assert_eq!(record["span"]["start"].as_u64().unwrap(), previous_end);
        assert_eq!(record["span"]["total"], 6000);
        previous_end = record["span"]["end"].as_u64().unwrap();
        text.push_str(record["text"].as_str().unwrap());
        if page["has_more"] == false {
            assert!(page["next_cursor"].is_null());
            break;
        }
        page = run(
            &service,
            &ctx,
            "read_history",
            json!({"ref": "m00003", "cursor": page["next_cursor"], "max_tokens": 512}),
        )
        .await
        .0;
    }
    assert_eq!(text, line);
    let (stale, _) = run(
        &service,
        &ctx,
        "read_history",
        json!({"ref": "m00002", "cursor": "0.5.ffffffffffff"}),
    )
    .await;
    assert_eq!(stale["error"]["code"], "STALE_CURSOR");
    for (args, code) in [
        (
            json!({"ref": "m00002", "view": "summary"}),
            "INVALID_ARGUMENT",
        ),
        (json!({"ref": "m09999"}), "UNKNOWN_REF"),
        (json!({"ref": "b7"}), "UNKNOWN_REF"),
        (json!({"ref": "m00002", "full": true}), "INVALID_ARGUMENT"),
    ] {
        let (result, error) = run(&service, &ctx, "read_history", args).await;
        assert!(error);
        assert_eq!(result["error"]["code"], code);
    }
}
#[test]
fn output_references_keep_current_turn_read_errors_signatures_and_raw_history() {
    let cancel = CancellationToken::new();
    let config = config();
    let messages = vec![
        message(Role::User, "old"),
        assistant(vec![call("x", "bash", json!({}))]),
        tool("x", &"large evidence\n".repeat(3000)),
        message(Role::User, "current"),
    ];
    let reference = |messages: &[Message]| {
        let p = prepare(&context(messages.to_vec()), &config, &cancel).unwrap();
        let mut state = p.state.clone();
        state.outputs = outputs::eligible(&BTreeMap::new(), &p.projection, &state, &config);
        let mut result = view(&p.projection, &state, &cancel).unwrap();
        outputs::apply(&mut result, &p.projection, &state, &config, &cancel).unwrap();
        result
    };
    let result = reference(&messages);
    let output = serde_json::to_string(&result[2].message).unwrap();
    assert!(
        output.contains("read_history {\\\"ref\\\":\\\"m00003\\\"}"),
        "{output}"
    );
    assert!(output.len() < 2000);
    let again = messages[..3].to_vec();
    assert_eq!(reference(&again)[2].message, again[2]);
    for exemption in [
        "read",
        "read_history",
        "error",
        "signature",
        "image",
        "protected",
    ] {
        let mut exempt = messages.clone();
        if let Content::ToolCall(c) = &mut exempt[1].content[0]
            && exemption.starts_with("read")
        {
            c.name = exemption.into();
        }
        if let Content::ToolResult(r) = &mut exempt[2].content[0] {
            match exemption {
                "error" => r.is_error = true,
                "signature" => {
                    if let Content::Text { opaque, .. } = &mut r.content[0] {
                        *opaque = Some(OpaqueData("signature".into()));
                    }
                }
                "image" => r.content.push(Content::Image {
                    mime_type: "image/png".into(),
                    data: "opaque".into(),
                }),
                _ => {}
            }
        }
        let mut config = config.clone();
        if exemption == "protected" {
            config.protected_tools = vec!["bash".into()];
        }
        let p = prepare(&context(exempt.clone()), &config, &cancel).unwrap();
        assert!(
            outputs::eligible(&BTreeMap::new(), &p.projection, &p.state, &config).is_empty(),
            "{exemption}"
        );
    }
}
#[tokio::test]
async fn nudges_offer_recommended_candidates_at_the_tail_only() {
    let mut config = CompactionConfig {
        preserve_recent_messages: 0,
        preserve_recent_tokens: 0,
        ..Default::default()
    };
    config.compress.min_compress_range = 0;
    config.output.enabled = false;
    config.nudge.min_pressure_benefit_tokens = PressureBenefit::Tokens(1);
    let mut ctx = context(history());
    ctx.window = 30000;
    let p = prepare(&ctx, &config, &CancellationToken::new()).unwrap();
    let mut cadence = nudge::Cadence::default();
    assert_eq!(
        cadence.hint(&p, &all(&p), &config, ctx.window, 29999),
        Some(false)
    );
    let service = service(config);
    let view = service
        .project(ctx.clone(), CancellationToken::new())
        .await
        .unwrap()
        .unwrap()
        .messages;
    assert_eq!(
        &view[..view.len() - 1]
            .iter()
            .map(|m| m.message.clone())
            .collect::<Vec<_>>()[..],
        &ctx.messages[..]
    );
    let hint = messages::text(&view.last().unwrap().message.content);
    assert!(hint.starts_with("Context maintenance hint"));
    let offered: Value = serde_json::from_str(hint.split_once('\n').unwrap().1).unwrap();
    let id = offered["candidates"][0]["id"].clone();
    assert!(offered["candidates"].as_array().unwrap().len() <= history::HINTED);
    let (receipt, error) = run(
        &service,
        &ctx,
        "compact_history",
        compact_args(&id, &summary()),
    )
    .await;
    assert!(!error, "{receipt}");
    assert_eq!(CompactionConfig::default().range_window(1000000), 300000);
    assert_eq!(CompactionConfig::default().range_window(0), 32768);
}
#[test]
fn registration_disabled_and_schemas_match_limits() {
    let config = ModuleConfig {
        modules: BTreeMap::from([("compaction".into(), false)]),
        ..Default::default()
    };
    let registry = Registry::from_modules(&[&CompactionModule], &config).unwrap();
    assert!(registry.tools().specs().is_empty());
    assert!(registry.projections().is_empty());
    let registry = Registry::from_modules(&[&CompactionModule], &ModuleConfig::default()).unwrap();
    let names: Vec<_> = registry
        .tools()
        .specs()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names.len(), 4, "{names:?}");
    for name in [
        "context_status",
        "compact_history",
        "search_history",
        "read_history",
    ] {
        assert!(names.contains(&name.to_string()));
    }
    assert_eq!(registry.compactors().len(), 1);
    assert_eq!(
        registry
            .tools()
            .get("compact_history")
            .unwrap()
            .spec()
            .input_schema["properties"]["ranges"]["items"]["properties"]["summary"]["minLength"],
        50
    );
}
#[tokio::test]
async fn output_references_cannot_be_claimed_as_raw_net_savings() {
    let mut ctx = context(history());
    for part in &mut ctx.messages[1].content {
        if let Content::ToolCall(call) = part {
            call.name = "bash".into();
        }
    }
    ctx.history = ctx.messages.clone();
    let service = service(config());
    snapshot(&service, &ctx).await;
    let p = prepare(&ctx, &service.config, &CancellationToken::new()).unwrap();
    let id = holding(&p, &all(&p), "OLD_REPORT_A").id;
    let (result, error) = run_with(
        &service,
        &ctx,
        "c",
        "compact_history",
        compact_args(&json!(id), &summary()),
        Arc::new(|_| panic!("low-savings state must not be queued")),
    )
    .await
    .unwrap();
    assert!(error);
    assert_eq!(result["error"]["code"], "INSUFFICIENT_SAVINGS");
}
#[tokio::test]
async fn queued_tool_batch_state_is_composed_but_never_published_before_persistence() {
    let mut config = CompactionConfig {
        preserve_recent_messages: 0,
        preserve_recent_tokens: 0,
        ..Default::default()
    };
    config.output.enabled = false;
    let ctx = context(vec![
        message(Role::User, "Initial"),
        message(Role::Assistant, &"FIRST_RECORD ".repeat(2000)),
        message(Role::User, "Second"),
        message(Role::Assistant, &"SECOND_RECORD ".repeat(2000)),
        message(Role::User, "Current"),
    ]);
    let service = service(config);
    snapshot(&service, &ctx).await;
    let p = prepare(&ctx, &service.config, &CancellationToken::new()).unwrap();
    let list = all(&p);
    let saved = Arc::new(Mutex::new(vec![]));
    for (i, needle) in ["FIRST_RECORD", "SECOND_RECORD"].into_iter().enumerate() {
        let sink = saved.clone();
        let (receipt, error) = run_with(
            &service,
            &ctx,
            &format!("c{i}"),
            "compact_history",
            compact_args(&json!(holding(&p, &list, needle).id), &summary()),
            Arc::new(move |entry| {
                sink.lock().unwrap().push(entry.data);
                Ok(())
            }),
        )
        .await
        .unwrap();
        assert!(!error, "{receipt}");
    }
    assert_eq!(
        saved.lock().unwrap()[1]["blocks"].as_array().unwrap().len(),
        2
    );
    assert!(p.state.blocks.is_empty());
    assert!(
        service
            .project(ctx, CancellationToken::new())
            .await
            .unwrap()
            .is_none()
    );
}
#[test]
fn nested_merges_retain_original_recovery() {
    let cancel = CancellationToken::new();
    let mut config = CompactionConfig {
        preserve_recent_messages: 0,
        preserve_recent_tokens: 0,
        ..Default::default()
    };
    config.compress.min_net_savings_tokens = 1;
    let mut ctx = context(
        std::iter::once(message(Role::User, "Initial"))
            .chain(
                (0..4)
                    .map(|i| message(Role::User, &format!("RAW_{i} {}", "evidence ".repeat(1500)))),
            )
            .chain([message(Role::User, "Current")])
            .collect(),
    );
    let raw = |ctx: &mut ProjectionContext, i: usize| {
        let p = prepare(ctx, &config, &cancel).unwrap();
        let state = fold(
            &p,
            &holding(&p, &all(&p), &format!("RAW_{i}")),
            &summary(),
            &format!("r{i}"),
        );
        persist(ctx, &state);
    };
    let merge = |ctx: &mut ProjectionContext, tier: u8| {
        let p = prepare(ctx, &config, &cancel).unwrap();
        let merge = all(&p)
            .into_iter()
            .find(|c| matches!(c.source, Source::Summaries(_)))
            .unwrap();
        let state = fold(&p, &merge, &summary(), &format!("t{tier}"));
        assert_eq!(state.blocks.last().unwrap().tier, tier);
        persist(ctx, &state);
    };
    for i in 0..3 {
        raw(&mut ctx, i);
    }
    merge(&mut ctx, 2);
    raw(&mut ctx, 3);
    merge(&mut ctx, 3);
    let p = prepare(&ctx, &config, &cancel).unwrap();
    let source: String = records(&p, "b6", false)
        .unwrap()
        .into_iter()
        .map(|r| r.text)
        .collect();
    assert!(source.contains("RAW_0") && source.contains("RAW_3"));
}
#[test]
fn recovery_results_skip_recent_protection_and_corrupt_graphs_are_rejected() {
    for name in [
        "read_history",
        "search_history",
        "context_status",
        "decompress",
        "search_context",
    ] {
        let ctx = context(vec![
            message(Role::User, "Initial"),
            message(Role::User, "LATEST"),
            assistant(vec![call("r", name, json!({}))]),
            tool("r", &"historical ".repeat(2000)),
        ]);
        let p = prepare(&ctx, &config(), &CancellationToken::new()).unwrap();
        assert!(
            p.projection
                .parts
                .iter()
                .filter(|p| p.call == "r")
                .all(|part| !p.protected.contains_key(&part.id)),
            "{name}"
        );
    }
    let ctx = context(history());
    let p = prepare(&ctx, &config(), &CancellationToken::new()).unwrap();
    let mut state = fold(&p, &all(&p)[0], &summary(), "c");
    state.blocks[0].children.push("b1".into());
    assert!(state.validate().is_err());
}
#[test]
fn legacy_state_and_compress_carriers_remain_readable() {
    let cancel = CancellationToken::new();
    let mut ctx = context(history());
    let p = prepare(&ctx, &config(), &cancel).unwrap();
    let candidate = holding(&p, &all(&p), "OLD_REPORT_A");
    let mut state = fold(&p, &candidate, &summary(), "legacy");
    state.version = 1;
    let block = &mut state.blocks[0];
    block.candidate = None;
    let (start, end) = (block.start.clone(), block.end.clone());
    persist(&mut ctx, &state);
    ctx.messages.push(assistant(vec![call(
        "legacy",
        "compress",
        json!({"content":[{"startId": start, "endId": end, "summary": summary()}]}),
    )]));
    ctx.messages
        .push(tool("legacy", "1 summary block(s) saved"));
    ctx.history = ctx.messages.clone();
    let bytes =
        serde_json::to_string(&output(&prepare(&ctx, &config(), &cancel).unwrap())).unwrap();
    assert_eq!(bytes.matches(&summary()).count(), 1);
    assert!(!bytes.contains("OLD_REPORT"));
}
#[test]
fn reasoning_runs_across_envelopes_withdraw_the_whole_split_turn() {
    let history = vec![
        assistant(vec![Content::Thinking {
            text: "reason".into(),
            redacted: false,
            opaque: None,
        }]),
        assistant(vec![
            plain("text"),
            call("a", "read", json!({})),
            call("b", "read", json!({})),
        ]),
        tool("a", "one"),
        tool("b", "two"),
    ];
    let p = project(&history, &CancellationToken::new()).unwrap();
    let mut folded = p
        .parts
        .iter()
        .filter(|p| p.call != "b")
        .map(|p| p.id.clone())
        .collect();
    messages::integrity(&p.parts, &mut folded);
    assert!(folded.is_empty());
    let mut reverse = p
        .parts
        .iter()
        .filter(|p| p.call == "a")
        .map(|p| p.id.clone())
        .collect();
    messages::integrity(&p.parts, &mut reverse);
    assert_eq!(reverse.len(), 2);
}
