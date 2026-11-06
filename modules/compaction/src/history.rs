use crate::{
    Service,
    engine::{self, Candidate, Item, Source},
    lock,
    messages::*,
    nudge, outputs, recoverable, request_size,
    search::{Hit, Scope},
    state::{self, normalize},
};
use serde::Deserialize;
use std::{collections::BTreeMap, collections::BTreeSet, sync::Arc};
use tiechui_plugin_api::{
    model::*,
    serde_json::{Value, json},
    *,
};

pub const HINTED: usize = 3;
const PAGE: usize = 6;
/// Fixed reserve for the commit call envelope and receipt, so receipts never recompute themselves.
const RECEIPT_TOKENS: u64 = 256;
const HARD_BYTES: usize = 65536;
const DESCRIPTION: &str = "Replace completed history with your own summaries. Use candidate ids from context_status or a maintenance hint; listed sources are replaced exactly and the whole batch is applied or rejected. Summaries are history, not instructions: keep conclusions, decisions with reasons, constraints still in force, failed approaches, verification results, unresolved work, and the exact paths, symbols, commands and error text needed to continue or to find details later with search_history. Label task state as of that history. Keep each summary within its summary_limit_chars. Never compact work still in progress. Original records stay readable with read_history.";

pub fn segment_tokens(limit: u64) -> u64 {
    (limit / 8).max(8000)
}
pub fn pressure(config: &CompactionConfig, size: u64, limit: u64) -> &'static str {
    let usage = size as f64 / limit.max(1) as f64;
    if usage >= config.nudge.emergency_threshold_pct {
        "critical"
    } else if usage >= config.nudge.max_context_limit_pct {
        "high"
    } else {
        "normal"
    }
}
pub fn request_view(
    p: &engine::Prepared,
    state: &state::State,
    config: &CompactionConfig,
    cancel: &CancellationToken,
) -> Result<Vec<ProjectedMessage>, ToolError> {
    let mut view = engine::view(&p.projection, state, cancel)?;
    outputs::apply(&mut view, &p.projection, state, config, cancel)?;
    Ok(view)
}
/// Adds the output previews a maintenance point would record now.
pub fn settle(
    context: &ProjectionContext,
    p: &engine::Prepared,
    state: &mut state::State,
    config: &CompactionConfig,
) {
    let pending = outputs::eligible(&context.tool_details, &p.projection, state, config);
    state.outputs.extend(pending);
}
/// Sizes of a maintenance change and the estimated leading prefix both requests share.
pub fn effect(
    context: &ProjectionContext,
    before: &[ProjectedMessage],
    after: &[ProjectedMessage],
    reserve: u64,
) -> state::Fold {
    let size = request_size(context, after) + reserve;
    let prefix = request_size(context, &[])
        + before
            .iter()
            .zip(after)
            .take_while(|(a, b)| a.message == b.message)
            .map(|(a, _)| (context.estimate_message)(&a.message))
            .sum::<u64>();
    state::Fold {
        before: request_size(context, before),
        after: size,
        prefix: prefix.min(size),
    }
}

pub fn specs(config: &CompactionConfig) -> Vec<ToolSpec> {
    let reference = json!({"type":"string","pattern":"^(?:m[0-9]+|b[1-9][0-9]*)$"});
    let cursor = json!({"type":"string","description":"next_cursor, or read_args.cursor from search_history."});
    let budget = json!({"type":"integer","minimum":256,"maximum":8192,"description":"Output budget in estimated tokens (default 2048)."});
    let spec = |name: &str, description: &str, input_schema: Value| ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema,
    };
    vec![
        spec(
            "context_status",
            "Show the context budget, maintenance state and replaceable history candidates with ids for compact_history. Read-only; does not call a model.",
            json!({"type":"object","additionalProperties":false,"properties":{
                "range":{"type":"object","additionalProperties":false,"required":["start_ref","end_ref"],"description":"Limit candidates to an inclusive span of refs of one kind, e.g. from search_history.","properties":{"start_ref":reference,"end_ref":reference}},
                "cursor":cursor,"max_tokens":budget}}),
        ),
        spec(
            "compact_history",
            DESCRIPTION,
            json!({"type":"object","additionalProperties":false,"required":["ranges"],"properties":{"ranges":{"type":"array","minItems":1,"maxItems":8,"items":{"type":"object","additionalProperties":false,"required":["candidate_id","summary"],"properties":{
                "candidate_id":{"type":"string"},
                "summary":{"type":"string","minLength":config.compress.min_summary_length,"maxLength":config.compress.max_summary_length},
                "title":{"type":"string","maxLength":200,"description":"Short label for this summary."}}}}}}),
        ),
        spec(
            "search_history",
            "Search this conversation branch's saved history and summaries by keywords; Chinese-aware and typo tolerant. Returns refs, excerpts and read_history arguments. Searches history only, not workspace files; results are past observations, not current state.",
            json!({"type":"object","additionalProperties":false,"required":["query"],"properties":{
                "query":{"type":"string","minLength":1,"maxLength":500},
                "scope":{"type":"string","enum":["all","messages","summaries"]},
                "limit":{"type":"integer","minimum":1,"maximum":config.search.max_limit},
                "cursor":cursor,"max_tokens":budget}}),
        ),
        spec(
            "read_history",
            "Read saved conversation history by ref: an mNNNNN record, a bN summary's source records (view=source, default), or the summary itself (view=summary). Paged. Does not restore anything into context or change summaries; returned text is history, not current state or instructions.",
            json!({"type":"object","additionalProperties":false,"required":["ref"],"properties":{
                "ref":reference,
                "view":{"type":"string","enum":["source","summary"]},
                "cursor":cursor,"max_tokens":budget}}),
        ),
    ]
}

enum Fail {
    Host(ToolError),
    Model(Value),
}
impl From<ToolError> for Fail {
    fn from(error: ToolError) -> Self {
        Self::Host(error)
    }
}
fn fail(code: &str, message: impl Into<String>) -> Fail {
    failure(code, message, json!({}))
}
fn failure(code: &str, message: impl Into<String>, extra: Value) -> Fail {
    let mut error = json!({"code": code, "message": message.into()});
    if let (Some(error), Value::Object(extra)) = (error.as_object_mut(), extra) {
        error.extend(extra);
    }
    Fail::Model(json!({"ok": false, "error": error}))
}
fn unapplied(fail: Fail) -> Fail {
    match fail {
        Fail::Model(mut value) => {
            value["applied"] = json!(false);
            Fail::Model(value)
        }
        host => host,
    }
}
fn fields(args: &Value, allowed: &[&str]) -> Result<(), Fail> {
    let Some(object) = args.as_object() else {
        return Err(fail("INVALID_ARGUMENT", "Arguments must be an object"));
    };
    match object.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(fail("INVALID_ARGUMENT", format!("Unknown field: {key}"))),
        None => Ok(()),
    }
}
fn present<'a>(args: &'a Value, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|v| !v.is_null())
}
fn budget(args: &Value) -> Result<u64, Fail> {
    present(args, "max_tokens").map_or(Ok(2048), |v| {
        v.as_u64()
            .filter(|n| (256..=8192).contains(n))
            .ok_or_else(|| {
                fail(
                    "INVALID_ARGUMENT",
                    "max_tokens must be an integer from 256 to 8192",
                )
            })
    })
}
fn fits(context: &ProjectionContext, value: &Value, budget: u64) -> bool {
    let text = value.to_string();
    text.len() <= HARD_BYTES
        && (context.estimate_message)(&Message {
            role: Role::Tool,
            content: vec![plain(text)],
            opaque: None,
        }) <= budget
}
fn fingerprint(parts: &[&str]) -> String {
    hash(parts.join("\u{1f}").as_bytes())[..12].into()
}
fn cursor(fp: &str, position: &[usize]) -> String {
    let mut parts: Vec<_> = position.iter().map(usize::to_string).collect();
    parts.push(fp.into());
    parts.join(".")
}
/// Cursor positions; empty when the call starts from the beginning.
fn resume(args: &Value, fp: &str, len: usize) -> Result<Vec<usize>, Fail> {
    let Some(value) = present(args, "cursor") else {
        return Ok(vec![]);
    };
    let stale = || {
        fail(
            "STALE_CURSOR",
            "The cursor belongs to other content or filters; repeat the request without a cursor.",
        )
    };
    let (position, found) = value
        .as_str()
        .and_then(|s| s.rsplit_once('.'))
        .ok_or_else(stale)?;
    let position: Vec<_> = position
        .split('.')
        .map(str::parse::<usize>)
        .collect::<Result<_, _>>()
        .map_err(|_| stale())?;
    if found != fp || position.len() != len {
        return Err(stale());
    }
    Ok(position)
}
fn role(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
        Role::System => "system",
    }
}
/// Largest summary, in characters, that the shared estimator keeps within `tokens`.
fn summary_chars(context: &ProjectionContext, tokens: u64, cap: usize) -> usize {
    let size = |n: usize| {
        (context.estimate_message)(&Message {
            role: Role::Assistant,
            content: vec![plain("x".repeat(n))],
            opaque: None,
        })
    };
    let base = size(0);
    let (mut low, mut high) = (0, cap);
    while low < high {
        let middle = (low + high).div_ceil(2);
        if size(middle) - base <= tokens {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    low
}

/// Candidates with exact view-based limits, in request order. Sources that are no longer
/// safe to fold or cannot save enough for a minimum summary are dropped. Recommendations
/// start from the latest candidate, which keeps the longest cached prefix.
pub fn describe(
    context: &ProjectionContext,
    p: &engine::Prepared,
    config: &CompactionConfig,
    before: u64,
    candidates: &[Candidate],
    cancel: &CancellationToken,
) -> Result<Vec<(Candidate, Value)>, ToolError> {
    let mut rows = vec![];
    for candidate in candidates {
        check(cancel)?;
        let mut probe = engine::commit(
            p,
            &[Item {
                candidate: &candidate.id,
                source: &candidate.source,
                summary: "",
                title: None,
            }],
            "",
        );
        settle(context, p, &mut probe, config);
        let after = match request_view(p, &probe, config, cancel) {
            Ok(view) => view,
            Err(error) if cancel.is_cancelled() => return Err(error),
            Err(_) => continue,
        };
        let reclaim = before.saturating_sub(request_size(context, &after));
        let chars = summary_chars(
            context,
            reclaim.saturating_sub(RECEIPT_TOKENS + config.compress.min_net_savings_tokens.max(1)),
            config.compress.max_summary_length,
        );
        if chars >= config.compress.min_summary_length {
            rows.push((candidate.clone(), reclaim, chars));
        }
    }
    let limit = config.range_window(context.window);
    let target = before
        .saturating_sub((limit as f64 * config.nudge.max_context_limit_pct) as u64)
        .max(limit / 10);
    let mut recommended = BTreeSet::new();
    let mut total = 0;
    for (candidate, reclaim, _) in rows.iter().rev() {
        if total >= target {
            break;
        }
        recommended.insert(candidate.id.clone());
        total += reclaim;
    }
    Ok(rows
        .into_iter()
        .map(|(candidate, reclaim, chars)| {
            let mut value = json!({
                "id": candidate.id,
                "first": candidate.first,
                "last": candidate.last,
                "source_tokens_estimate": reclaim,
                "summary_limit_chars": chars,
                "recommended": recommended.contains(&candidate.id),
            });
            match &candidate.source {
                Source::Messages(_) => {
                    value["kind"] = json!("messages");
                    value["start_ref"] = json!(candidate.start);
                    value["end_ref"] = json!(candidate.end);
                }
                Source::Summaries(ids) => {
                    value["kind"] = json!("summaries");
                    value["source_refs"] = json!(ids);
                }
            }
            (candidate, value)
        })
        .collect())
}

pub struct HistoryTool {
    pub service: Arc<Service>,
    pub spec: ToolSpec,
}
#[async_trait]
impl Tool for HistoryTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn source(&self) -> Option<String> {
        Some("compaction".into())
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        if self.spec.name == "compact_history" {
            Concurrency::Exclusive
        } else {
            Concurrency::Shared
        }
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let tool = HistoryTool {
            service: self.service.clone(),
            spec: self.spec.clone(),
        };
        let cancel = ctx.cancel.clone();
        let work = tokio::task::spawn_blocking(move || tool.execute(call, ctx));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(ToolError("Operation aborted".into())),
            result = work => result.map_err(|e| ToolError(e.to_string()))?,
        }
    }
}
impl HistoryTool {
    fn execute(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        check(&ctx.cancel)?;
        let service = &self.service;
        let mut context = lock(&service.snapshot)?
            .clone()
            .ok_or_else(|| ToolError("History request context is unavailable".into()))?;
        if context.branch.session_id != ctx.scope.thread_id.0 {
            return Err(ToolError(
                "History context belongs to a previous session".into(),
            ));
        }
        context.branch.entries = (*ctx.branch).clone();
        let batch = format!(
            "{}:{}:{}",
            ctx.scope.thread_id.0,
            ctx.epoch.0,
            ctx.branch.last().map_or("root", |entry| entry.id.as_str())
        );
        let queued = lock(&service.queued)?.clone();
        let same_batch = queued
            .as_ref()
            .is_some_and(|(identity, _)| identity == &batch);
        if let Some((_, state)) = queued.filter(|_| same_batch) {
            context.branch.entries.push(protocol::BranchEntry {
                id: "queued-compaction".into(),
                parent_id: None,
                timestamp: 0,
                content: None,
                custom: Some((state::ENTRY.into(), json!(state))),
            });
        }
        let prepared = engine::prepare(&context, &service.config, &ctx.cancel)?;
        let args = &call.arguments;
        let result = match call.name.as_str() {
            "context_status" => self.status(&context, &prepared, args, &ctx.cancel),
            "compact_history" => self
                .compact(&context, &prepared, &call, &ctx, batch, same_batch)
                .map_err(unapplied),
            "search_history" => self.search(&context, &prepared, args, &ctx.cancel),
            "read_history" => self.read(&context, &prepared, args),
            _ => return Err(ToolError("Unknown history tool".into())),
        };
        match result {
            Ok(value) => Ok(ToolOutput::text(value.to_string())),
            Err(Fail::Model(value)) => Ok(ToolOutput {
                is_error: true,
                ..ToolOutput::text(value.to_string())
            }),
            Err(Fail::Host(error)) => Err(error),
        }
    }

    fn bounds(&self, p: &engine::Prepared, range: &Value) -> Result<(usize, usize), Fail> {
        fields(range, &["start_ref", "end_ref"])?;
        let reference = |key| {
            range[key].as_str().and_then(normalize).ok_or_else(|| {
                fail(
                    "INVALID_ARGUMENT",
                    format!("range.{key} must be mNNNNN or bN"),
                )
            })
        };
        let (start, end) = (reference("start_ref")?, reference("end_ref")?);
        if start[..1] != end[..1] {
            return Err(fail(
                "INVALID_ARGUMENT",
                "range refs must both be messages or both be summaries",
            ));
        }
        let span = |reference: &str| {
            engine::span(p, reference).ok_or_else(|| {
                fail(
                    "UNKNOWN_REF",
                    format!("{reference} is not in the current request view; use search_history"),
                )
            })
        };
        let (a, b) = (span(&start)?.0, span(&end)?.1);
        if a > b {
            return Err(fail(
                "INVALID_ARGUMENT",
                "range.start_ref must not follow range.end_ref",
            ));
        }
        Ok((a, b))
    }

    fn status(
        &self,
        context: &ProjectionContext,
        p: &engine::Prepared,
        args: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value, Fail> {
        fields(args, &["range", "cursor", "max_tokens"])?;
        let budget = budget(args)?;
        let config = &self.service.config;
        let limit = config.range_window(context.window);
        let bounds = present(args, "range")
            .map(|range| self.bounds(p, range))
            .transpose()?;
        let mut baseline = p.state.clone();
        settle(context, p, &mut baseline, config);
        let before = request_size(context, &request_view(p, &baseline, config, cancel)?);
        let paused = !recoverable(context);
        let cooling = nudge::cooling(&p.state, &context.history)
            .filter(|_| pressure(config, before, limit) == "normal");
        let all = if paused || cooling.is_some() {
            vec![]
        } else {
            engine::candidates(
                p,
                config.compress.min_compress_range,
                segment_tokens(limit),
                bounds,
            )
        };
        let described = describe(context, p, config, before, &all, cancel)?;
        let ids: Vec<_> = described.iter().map(|(c, _)| c.id.as_str()).collect();
        let fp = fingerprint(&[
            &context.branch.session_id,
            &args["range"].to_string(),
            &ids.join(","),
        ]);
        let offset = resume(args, &fp, 1)?.first().copied().unwrap_or(0);
        let maintenance = if paused {
            json!({"state":"paused","reason":"read_history_unavailable"})
        } else if let Some(remaining) = cooling {
            json!({"state":"cooldown","reason":"recent_maintenance","remaining_business_requests":remaining})
        } else if described.is_empty() {
            json!({"state":"not_needed","reason":"no_eligible_history"})
        } else {
            json!({"state":"ready","reason":"eligible_history"})
        };
        let mut result = json!({
            "ok": true,
            "budget": {
                "input_tokens_estimate": before,
                "model_window_tokens": if context.window == 0 { config.fallback_context_window } else { context.window },
                "window_source": if context.window == 0 { "fallback" } else { "model" },
                "reserve_tokens": config.reserve_tokens,
                "scheduling_limit_tokens": limit,
                "pressure": pressure(config, before, limit),
            },
            "maintenance": maintenance,
            "active_summary_count": p.state.blocks.iter().filter(|b| b.active).count(),
            "candidates": [],
            "protected": protected(p, bounds),
            "has_more": offset < described.len(),
            "next_cursor": cursor(&fp, &[described.len()]),
        });
        if !fits(context, &result, budget) {
            return Err(fail(
                "INVALID_ARGUMENT",
                "max_tokens is too small for a status result",
            ));
        }
        let mut next = offset;
        let mut shown = vec![];
        for (candidate, value) in described.iter().skip(offset).take(PAGE) {
            push(&mut result["candidates"], value.clone());
            if !fits(context, &result, budget) {
                pop(&mut result["candidates"]);
                break;
            }
            shown.push(candidate.clone());
            next += 1;
        }
        if shown.is_empty() && offset < described.len() {
            return Err(fail(
                "INVALID_ARGUMENT",
                "max_tokens is too small for one candidate",
            ));
        }
        result["has_more"] = json!(next < described.len());
        result["next_cursor"] = if next < described.len() {
            json!(cursor(&fp, &[next]))
        } else {
            Value::Null
        };
        self.service.issue(&shown)?;
        Ok(result)
    }

    fn compact(
        &self,
        context: &ProjectionContext,
        p: &engine::Prepared,
        call: &ToolCall,
        ctx: &ToolContext,
        batch: String,
        same_batch: bool,
    ) -> Result<Value, Fail> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Range {
            candidate_id: String,
            summary: String,
            title: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Arguments {
            ranges: Vec<Range>,
        }
        let config = &self.service.config;
        let cancel = &ctx.cancel;
        let args: Arguments = serde_json::from_value(call.arguments.clone())
            .map_err(|e| fail("INVALID_ARGUMENT", e.to_string()))?;
        if !recoverable(context) {
            return Err(fail(
                "RECOVERY_UNAVAILABLE",
                "read_history is unavailable, so compacted history could not be recovered; original history stays visible.",
            ));
        }
        let existing: Vec<_> = p
            .state
            .blocks
            .iter()
            .filter(|b| b.call == call.id)
            .collect();
        if !existing.is_empty() {
            let mut receipt = receipt(&existing);
            receipt["duplicate"] = json!(true);
            return Ok(receipt);
        }
        let limit = config.range_window(context.window);
        let mut baseline = p.state.clone();
        settle(context, p, &mut baseline, config);
        let before_view = request_view(p, &baseline, config, cancel)?;
        let before = request_size(context, &before_view);
        // Commits in one tool batch reach the same next request, so only later batches cool down.
        let cooling = nudge::cooling(&p.state, &context.history).filter(|_| !same_batch);
        if let Some(remaining) = cooling
            && pressure(config, before, limit) == "normal"
        {
            return Err(failure(
                "MAINTENANCE_DEFERRED",
                "History was just maintained; compacting again now would rewrite the cached prefix twice. Continue the task.",
                json!({"remaining_business_requests": remaining}),
            ));
        }
        if args.ranges.is_empty() || args.ranges.len() > 8 {
            return Err(fail("INVALID_ARGUMENT", "Provide 1–8 ranges"));
        }
        let (min, max) = (
            config.compress.min_summary_length,
            config.compress.max_summary_length,
        );
        let mut invalid = vec![];
        let mut seen = BTreeSet::new();
        for range in &args.ranges {
            let length = range.summary.trim().chars().count();
            let reason = if !seen.insert(&range.candidate_id) {
                "duplicate_candidate"
            } else if !(min..=max).contains(&length) {
                "summary_length"
            } else if range
                .title
                .as_ref()
                .is_some_and(|t| t.chars().count() > 200)
            {
                "title_length"
            } else {
                continue;
            };
            invalid.push(json!({"candidate_id": range.candidate_id, "reason": reason}));
        }
        if !invalid.is_empty() {
            return Err(failure(
                "INVALID_ARGUMENT",
                format!(
                    "Summaries must be {min}–{max} characters and titles at most 200; nothing was applied."
                ),
                json!({"items": invalid}),
            ));
        }
        let issued = lock(&self.service.issued)?.clone();
        let fresh = engine::candidates(
            p,
            config.compress.min_compress_range,
            segment_tokens(limit),
            None,
        );
        let mut resolved = vec![];
        let mut changed = vec![];
        for range in &args.ranges {
            let found = issued
                .iter()
                .rev()
                .chain(&fresh)
                .find(|c| c.id == range.candidate_id);
            let reason = match found {
                None => "unknown_candidate",
                Some(candidate) => match engine::changed(p, &candidate.source) {
                    Some(reason) => reason,
                    None => {
                        resolved.push((candidate.clone(), range));
                        continue;
                    }
                },
            };
            changed.push((
                found,
                json!({"candidate_id": range.candidate_id, "reason": reason}),
            ));
        }
        if !changed.is_empty() {
            let arguments = match changed.as_slice() {
                [(Some(c), _)] if matches!(c.source, Source::Messages(_)) => {
                    json!({"range": {"start_ref": c.start, "end_ref": c.end}})
                }
                _ => json!({}),
            };
            return Err(failure(
                "CANDIDATE_CHANGED",
                format!(
                    "{} of {} candidates changed; nothing was applied.",
                    changed.len(),
                    args.ranges.len()
                ),
                json!({
                    "items": changed.into_iter().map(|(_, v)| v).collect::<Vec<_>>(),
                    "next": {"tool": "context_status", "arguments": arguments},
                }),
            ));
        }
        let mut sources = BTreeSet::new();
        let overlapping: Vec<_> = resolved
            .iter()
            .filter(|(c, _)| {
                let (Source::Messages(ids) | Source::Summaries(ids)) = &c.source;
                !ids.iter().all(|id| sources.insert(id.clone()))
            })
            .map(|(_, r)| json!({"candidate_id": r.candidate_id}))
            .collect();
        if !overlapping.is_empty() {
            return Err(failure(
                "OVERLAPPING_RANGES",
                "Candidates in one batch must not share sources; nothing was applied.",
                json!({"items": overlapping}),
            ));
        }
        let items: Vec<_> = resolved
            .iter()
            .map(|(c, r)| Item {
                candidate: &c.id,
                source: &c.source,
                summary: &r.summary,
                title: r.title.as_deref(),
            })
            .collect();
        let mut state = engine::commit(p, &items, &call.id);
        settle(context, p, &mut state, config);
        state.maintenance = Some(state::Maintenance {
            at: context.history.len(),
            forced: cooling.is_some(),
        });
        let after_view = match request_view(p, &state, config, cancel) {
            Ok(view) => view,
            Err(error) if cancel.is_cancelled() => return Err(error.into()),
            Err(error) => return Err(fail("CANDIDATE_CHANGED", error.0)),
        };
        let fold = effect(context, &before_view, &after_view, RECEIPT_TOKENS);
        let after = fold.after;
        let minimum = config.compress.min_net_savings_tokens.max(1);
        if before < after + minimum {
            let candidates: Vec<_> = resolved.iter().map(|(c, _)| c.clone()).collect();
            let limits: BTreeMap<_, _> = describe(context, p, config, before, &candidates, cancel)?
                .into_iter()
                .map(|(c, v)| (c.id, v["summary_limit_chars"].clone()))
                .collect();
            return Err(failure(
                "INSUFFICIENT_SAVINGS",
                "The batch would not save enough context after summaries and the receipt; nothing was applied.",
                json!({
                    "net_saved_tokens_estimate": before as i64 - after as i64,
                    "minimum_tokens": minimum,
                    "items": resolved.iter().map(|(c, _)| json!({
                        "candidate_id": c.id,
                        "summary_limit_chars": limits.get(&c.id).cloned().unwrap_or(json!(0)),
                    })).collect::<Vec<_>>(),
                }),
            ));
        }
        let prefix = fold.prefix;
        state.fold = Some(fold);
        check(cancel)?;
        (ctx.append_entry)(tools::CustomSessionEntry {
            kind: state::ENTRY.into(),
            data: json!(state),
        })?;
        let created: Vec<_> = state.blocks.iter().filter(|b| b.call == call.id).collect();
        let mut receipt = receipt(&created);
        receipt["effect"] = json!({
            "before_tokens_estimate": before,
            "after_tokens_estimate": after,
            "net_saved_tokens_estimate": before - after,
            "common_prefix_tokens_estimate": prefix,
            "rewritten_suffix_tokens_estimate": after - prefix,
        });
        receipt["maintenance"] =
            json!({"state": "cooldown", "remaining_business_requests": nudge::COOLDOWN});
        *lock(&self.service.queued)? = Some((batch, state.clone()));
        Ok(receipt)
    }

    fn search(
        &self,
        context: &ProjectionContext,
        p: &engine::Prepared,
        args: &Value,
        cancel: &CancellationToken,
    ) -> Result<Value, Fail> {
        fields(args, &["query", "scope", "limit", "cursor", "max_tokens"])?;
        let budget = budget(args)?;
        let config = &self.service.config;
        let query = args["query"]
            .as_str()
            .filter(|q| !q.trim().is_empty() && q.chars().count() <= 500)
            .ok_or_else(|| {
                fail(
                    "INVALID_ARGUMENT",
                    "query must be non-blank text of at most 500 characters",
                )
            })?;
        let (scope, scope_name) = match present(args, "scope").map(Value::as_str) {
            None | Some(Some("all")) => (Scope::All, "all"),
            Some(Some("messages")) => (Scope::Messages, "messages"),
            Some(Some("summaries")) => (Scope::Summaries, "summaries"),
            _ => {
                return Err(fail(
                    "INVALID_ARGUMENT",
                    "scope must be all, messages or summaries",
                ));
            }
        };
        let limit = present(args, "limit").map_or(Ok(config.search.default_limit), |v| {
            v.as_u64()
                .map(|n| n as usize)
                .filter(|n| (1..=config.search.max_limit).contains(n))
                .ok_or_else(|| {
                    fail(
                        "INVALID_ARGUMENT",
                        format!("limit must be from 1 to {}", config.search.max_limit),
                    )
                })
        })?;
        let hits = lock(&self.service.search)?.search(p, query, scope, cancel)?;
        let refs: Vec<_> = hits.iter().map(|h| h.reference.as_str()).collect();
        let fp = fingerprint(&[
            &context.branch.session_id,
            query,
            scope_name,
            &limit.to_string(),
            &refs.join(","),
        ]);
        let offset = resume(args, &fp, 1)?.first().copied().unwrap_or(0);
        let mut result = json!({
            "ok": true,
            "results": [],
            "has_more": true,
            "next_cursor": cursor(&fp, &[hits.len()]),
        });
        let mut next = offset;
        for hit in hits.iter().skip(offset).take(limit) {
            check(cancel)?;
            push(&mut result["results"], self.hit(context, p, hit));
            if !fits(context, &result, budget) {
                pop(&mut result["results"]);
                break;
            }
            next += 1;
        }
        if next == offset && offset < hits.len() {
            return Err(fail(
                "INVALID_ARGUMENT",
                "max_tokens is too small for one result",
            ));
        }
        result["has_more"] = json!(next < hits.len());
        result["next_cursor"] = if next < hits.len() {
            json!(cursor(&fp, &[next]))
        } else {
            Value::Null
        };
        Ok(result)
    }

    fn hit(&self, context: &ProjectionContext, p: &engine::Prepared, hit: &Hit) -> Value {
        let active = |ids: &[String]| {
            p.state
                .blocks
                .iter()
                .find(|b| b.active && ids.iter().all(|id| b.effective.contains(id)))
                .map(|b| b.id.clone())
        };
        let (in_view, covered_by) = if hit.summary {
            let block = p.state.block(&hit.reference);
            let in_view = block.is_some_and(|b| b.active);
            let cover = block
                .filter(|_| !in_view)
                .and_then(|b| active(&b.effective));
            (in_view, cover)
        } else {
            let id = p
                .history
                .parts
                .iter()
                .find(|v| p.state.reference(v) == hit.reference)
                .map(|v| v.id.clone())
                .unwrap_or_default();
            let cover = active(std::slice::from_ref(&id));
            let visible = p.projection.parts.iter().any(|v| v.id == id);
            (visible && cover.is_none(), cover)
        };
        let view = if hit.summary { "summary" } else { "source" };
        let records = engine::records(p, &hit.reference, hit.summary).unwrap_or_default();
        let mut value = json!({
            "ref": hit.reference,
            "kind": if hit.summary { "summary" } else { "message" },
            "in_view": in_view,
            "excerpt": hit.excerpt,
            "read_args": {
                "ref": hit.reference,
                "view": view,
                "cursor": cursor(
                    &read_fingerprint(context, &hit.reference, view, &records),
                    &[0, hit.position.saturating_sub(80)],
                ),
            },
        });
        if !hit.summary {
            value["role"] = json!(role(hit.role));
        }
        if !hit.tool.is_empty() {
            value["tool"] = json!(hit.tool);
        }
        if let Some(block) = covered_by {
            value["covered_by"] = json!(block);
        }
        value
    }

    fn read(
        &self,
        context: &ProjectionContext,
        p: &engine::Prepared,
        args: &Value,
    ) -> Result<Value, Fail> {
        fields(args, &["ref", "view", "cursor", "max_tokens"])?;
        let budget = budget(args)?;
        let reference = args["ref"]
            .as_str()
            .and_then(normalize)
            .ok_or_else(|| fail("INVALID_ARGUMENT", "ref must be mNNNNN or bN"))?;
        let view = match present(args, "view").map(Value::as_str) {
            None | Some(Some("source")) => "source",
            Some(Some("summary")) => "summary",
            _ => return Err(fail("INVALID_ARGUMENT", "view must be source or summary")),
        };
        let records =
            engine::records(p, &reference, view == "summary").map_err(|reason| match reason {
                "unknown_ref" => fail(
                    "UNKNOWN_REF",
                    format!("{reference} is not on this branch; use search_history"),
                ),
                "source_unavailable" => fail(
                    "SOURCE_UNAVAILABLE",
                    format!("{reference} has sources that are not saved on this branch"),
                ),
                _ => fail("INVALID_ARGUMENT", "view=summary requires a bN ref"),
            })?;
        let fp = read_fingerprint(context, &reference, view, &records);
        let (mut index, mut offset) = match resume(args, &fp, 2)?.as_slice() {
            [record, offset] => (*record, *offset),
            _ => (0, 0),
        };
        if index > records.len()
            || records
                .get(index)
                .is_some_and(|r| offset > r.text.chars().count())
        {
            return Err(fail(
                "STALE_CURSOR",
                "The cursor is outside this record; read again without a cursor.",
            ));
        }
        let mut result = json!({
            "ok": true,
            "ref": reference,
            "view": view,
            "historical": true,
            "records": [],
            "has_more": true,
            "next_cursor": cursor(&fp, &[usize::MAX, usize::MAX]),
        });
        let mut added = false;
        while let Some(record) = records.get(index) {
            let chars: Vec<char> = record.text.chars().collect();
            let total = chars.len();
            let entry = |n: usize| record_json(record, offset, offset + n, total, &chars);
            let (mut low, mut high) = (0, total - offset);
            while low < high {
                let middle = (low + high).div_ceil(2);
                push(&mut result["records"], entry(middle));
                let fit = fits(context, &result, budget);
                pop(&mut result["records"]);
                if fit {
                    low = middle;
                } else {
                    high = middle - 1;
                }
            }
            push(&mut result["records"], entry(low));
            if (low == 0 && total > offset) || !fits(context, &result, budget) {
                pop(&mut result["records"]);
                break;
            }
            added = true;
            if offset + low < total {
                offset += low;
                break;
            }
            index += 1;
            offset = 0;
        }
        if !added && index < records.len() {
            return Err(fail(
                "INVALID_ARGUMENT",
                "max_tokens is too small for one record",
            ));
        }
        result["has_more"] = json!(index < records.len());
        result["next_cursor"] = if index < records.len() {
            json!(cursor(&fp, &[index, offset]))
        } else {
            Value::Null
        };
        Ok(result)
    }
}

fn push(array: &mut Value, value: Value) {
    if let Some(array) = array.as_array_mut() {
        array.push(value);
    }
}
fn pop(array: &mut Value) {
    if let Some(array) = array.as_array_mut() {
        array.pop();
    }
}
fn read_fingerprint(
    context: &ProjectionContext,
    reference: &str,
    view: &str,
    records: &[engine::Record],
) -> String {
    let content: Vec<_> = records
        .iter()
        .map(|r| format!("{}:{}", r.reference, hash(r.text.as_bytes())))
        .collect();
    fingerprint(&[
        &context.branch.session_id,
        reference,
        view,
        &content.join(","),
    ])
}
fn record_json(
    record: &engine::Record,
    start: usize,
    end: usize,
    total: usize,
    chars: &[char],
) -> Value {
    let mut value = json!({
        "ref": record.reference,
        "span": {"start": start, "end": end, "total": total},
        "text": chars[start..end].iter().collect::<String>(),
    });
    if let Some(role) = record.role {
        value["role"] = json!(self::role(role));
    }
    if let Some(tool) = &record.tool {
        value["tool"] = json!(tool);
    }
    if let Some(call) = &record.call {
        value["call_ref"] = json!(call);
    }
    if record.opaque {
        value["attachments_omitted"] = json!(true);
    }
    if let Some(sources) = &record.source_refs {
        value["kind"] = json!("summary");
        value["source_refs"] = json!(sources);
    }
    value
}
fn receipt(blocks: &[&state::Block]) -> Value {
    json!({
        "ok": true,
        "applied": true,
        "blocks": blocks.iter().map(|b| {
            let mut value = json!({"candidate_id": b.candidate, "ref": b.id});
            if b.children.is_empty() {
                value["start_ref"] = json!(b.start);
                value["end_ref"] = json!(b.end);
            } else {
                value["source_refs"] = json!(b.children);
            }
            value
        }).collect::<Vec<_>>(),
    })
}

/// Protected parts relevant to the filter: counts by reason and the earliest example of each.
fn protected(p: &engine::Prepared, bounds: Option<(usize, usize)>) -> Value {
    let covered = p.state.covered();
    let mut counts = BTreeMap::<&str, usize>::new();
    let mut examples = vec![];
    for (i, part) in p.projection.parts.iter().enumerate() {
        if bounds.is_some_and(|(a, b)| !(a..=b).contains(&i)) || covered.contains(&part.id) {
            continue;
        }
        let Some(reason) = p.protected.get(&part.id) else {
            continue;
        };
        let count = counts.entry(reason).or_default();
        if *count == 0 && *reason != "transient" {
            examples.push(json!({"ref": p.state.reference(part), "reason": reason}));
        }
        *count += 1;
    }
    json!({"count_by_reason": counts, "examples": examples})
}
