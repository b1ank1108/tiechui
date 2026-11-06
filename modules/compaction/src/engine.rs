use crate::{messages::*, state::*};
use std::collections::{BTreeMap, BTreeSet};
use tiechui_plugin_api::{model::*, *};

/// Tools whose historical calls carry the summaries they created.
pub fn carrier(tool: &str) -> bool {
    matches!(tool, "compact_history" | "compress")
}

#[derive(Clone)]
pub struct Prepared {
    pub projection: Projection,
    pub history: Projection,
    pub state: State,
    /// Part id to the first reason that keeps it visible.
    pub protected: BTreeMap<String, &'static str>,
}
pub fn matches(name: &str, pattern: &str) -> bool {
    pattern
        .strip_suffix('*')
        .map_or(name == pattern, |prefix| name.starts_with(prefix))
}
pub fn prepare(
    ctx: &ProjectionContext,
    config: &CompactionConfig,
    cancel: &CancellationToken,
) -> Result<Prepared, ToolError> {
    let projection = project(&ctx.messages, cancel)?;
    let history = project(&ctx.history, cancel)?;
    let mut state = State::read(&ctx.branch.entries)?;
    state.assign(&history.parts)?;
    state.sync(&projection.parts);
    let covered = state.covered();
    let visible: Vec<_> = projection
        .parts
        .iter()
        .filter(|p| !covered.contains(&p.id) && !p.recovery())
        .collect();
    let mut protected = BTreeMap::new();
    if let Some(part) = visible.iter().rev().find(|p| p.role == Role::User) {
        protected.insert(part.id.clone(), "latest_user_request");
    }
    for part in visible.iter().rev().take(config.preserve_recent_messages) {
        protected.entry(part.id.clone()).or_insert("recent");
    }
    let mut size = 0;
    for part in visible.iter().rev() {
        if size >= config.preserve_recent_tokens {
            break;
        }
        protected.entry(part.id.clone()).or_insert("recent");
        size += part.tokens();
    }
    let owners: Vec<_> = visible
        .iter()
        .map(|p| p.owner)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for owner in owners.iter().rev().take(config.preserve_recent_messages) {
        for p in projection.parts.iter().filter(|p| p.owner == *owner) {
            protected.entry(p.id.clone()).or_insert("recent");
        }
    }
    let protected_owners: BTreeSet<_> = projection
        .parts
        .iter()
        .filter(|p| protected.contains_key(&p.id))
        .map(|p| p.owner)
        .collect();
    for part in &projection.parts {
        let reason = if protected_owners.contains(&part.owner) {
            "recent"
        } else if part.opaque {
            "opaque_content"
        } else if !state.refs.contains_key(&part.id) {
            "transient"
        } else if carrier(&part.tool) || matches!(part.tool.as_str(), "acp_rule" | "skill") {
            "pinned_tool"
        } else if config
            .protected_tools
            .iter()
            .any(|pattern| matches(&part.tool, pattern))
        {
            "protected_tool"
        } else {
            continue;
        };
        protected.entry(part.id.clone()).or_insert(reason);
    }
    let mut allowed: BTreeSet<_> = projection
        .parts
        .iter()
        .filter(|p| !covered.contains(&p.id) && !protected.contains_key(&p.id))
        .map(|p| p.id.clone())
        .collect();
    integrity(&projection.parts, &mut allowed);
    for p in &projection.parts {
        if !allowed.contains(&p.id) && !covered.contains(&p.id) {
            protected.entry(p.id.clone()).or_insert("tool_pair");
        }
    }
    Ok(Prepared {
        projection,
        history,
        state,
        protected,
    })
}

fn summary_message(block: &Block) -> ProjectedMessage {
    ProjectedMessage {
        source: None,
        message: Message {
            role: Role::User,
            content: vec![plain(format!(
                "Historical summary; use read_history for source records.\n[Compressed conversation section]\n{} ({})\n{}",
                block.id,
                block.topic.as_deref().unwrap_or("Summary"),
                block.summary
            ))],
            opaque: None,
        },
    }
}

pub fn view(
    projection: &Projection,
    state: &State,
    cancel: &CancellationToken,
) -> Result<Vec<ProjectedMessage>, ToolError> {
    if state.blocks.is_empty() {
        return Ok(projection
            .originals
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, message)| ProjectedMessage {
                source: Some(i),
                message,
            })
            .collect());
    }
    let mut folded = state.covered();
    if let Some(first) = projection.parts.iter().find(|p| p.role == Role::User) {
        folded.remove(&first.id);
    }
    let mut rewrites = BTreeMap::new();
    let mut orphaned = projection.parts.iter().rev().filter(|p| {
        carrier(&p.tool) && !p.result && !state.blocks.iter().any(|b| b.call == p.call)
    });
    let recent_orphans: BTreeSet<_> = orphaned.by_ref().take(2).map(|p| p.call.clone()).collect();
    for p in projection
        .parts
        .iter()
        .filter(|p| carrier(&p.tool) && !p.result)
    {
        let blocks: Vec<_> = state
            .blocks
            .iter()
            .filter(|b| b.call == p.call && b.active)
            .collect();
        if blocks.is_empty() && !recent_orphans.contains(&p.call) {
            for part in projection.parts.iter().filter(|v| v.call == p.call) {
                folded.insert(part.id.clone());
            }
        } else if !signed(&projection.originals[p.owner]) {
            let mut args: serde_json::Value =
                serde_json::from_str(&p.text).map_err(|e| ToolError(e.to_string()))?;
            let key = if p.tool == "compress" {
                "content"
            } else {
                "ranges"
            };
            if let Some(items) = args.get_mut(key).and_then(|v| v.as_array_mut()) {
                let kept: Vec<_> = items
                    .iter()
                    .filter(|v| {
                        blocks.iter().any(|b| match &b.candidate {
                            Some(id) => v["candidate_id"] == *id,
                            None => v["startId"] == b.start && v["endId"] == b.end,
                        })
                    })
                    .cloned()
                    .collect();
                if !kept.is_empty() && kept.len() < items.len() {
                    *items = kept;
                    rewrites.insert(p.id.clone(), args);
                }
            }
        }
    }
    let live_calls: BTreeSet<_> = projection
        .parts
        .iter()
        .filter(|p| !p.result && !p.call.is_empty() && !folded.contains(&p.id))
        .map(|p| p.call.as_str())
        .collect();
    let mut summaries = BTreeMap::<usize, Vec<ProjectedMessage>>::new();
    for block in state
        .blocks
        .iter()
        .filter(|b| b.active && !live_calls.contains(b.call.as_str()))
    {
        let owner = projection
            .parts
            .iter()
            .filter(|p| block.effective.contains(&p.id))
            .map(|p| p.owner)
            .min()
            .unwrap_or(0);
        // Place a synthetic summary after a surviving parallel burst, never inside it.
        let mut anchor = owner;
        if let Some(original) = projection.originals.get(owner) {
            for call in original.content.iter().filter_map(|p| {
                if let Content::ToolCall(c) = p {
                    Some(c)
                } else {
                    None
                }
            }) {
                if live_calls.contains(call.id.as_str())
                    && let Some(result) = projection
                        .parts
                        .iter()
                        .find(|p| p.result && p.call == call.id)
                {
                    anchor = anchor.max(result.owner + 1);
                }
            }
        }
        summaries
            .entry(anchor)
            .or_default()
            .push(summary_message(block));
    }
    let mut result = vec![];
    for (owner, original) in projection.originals.iter().enumerate() {
        check(cancel)?;
        if let Some(messages) = summaries.remove(&owner) {
            result.extend(messages);
        }
        let parts: Vec<_> = projection
            .parts
            .iter()
            .filter(|p| p.owner == owner)
            .collect();
        let kept: Vec<_> = parts.iter().filter(|p| !folded.contains(&p.id)).collect();
        if kept.is_empty() {
            continue;
        }
        let mut message = original.clone();
        if kept.len() != parts.len() && signed(original) {
            return Err(ToolError(
                "Compaction would split a signed assistant message".into(),
            ));
        }
        if original.role == Role::Assistant
            && (kept.len() != parts.len() || kept.iter().any(|p| rewrites.contains_key(&p.id)))
        {
            message.content = original
                .content
                .iter()
                .enumerate()
                .filter_map(|(slot, content)| {
                    let p = kept.iter().find(|p| p.slots.contains(&slot));
                    if let Some(p) = p {
                        if let (Content::ToolCall(call), Some(args)) =
                            (content, rewrites.get(&p.id))
                        {
                            let mut call = call.clone();
                            call.arguments = args.clone();
                            return Some(Content::ToolCall(call));
                        }
                        Some(content.clone())
                    } else if matches!(content, Content::Image { .. }) {
                        Some(content.clone())
                    } else {
                        None
                    }
                })
                .collect();
        }
        result.push(ProjectedMessage {
            source: Some(owner),
            message,
        });
    }
    result.extend(summaries.into_values().flatten());
    let mut valid = folded.clone();
    integrity(&projection.parts, &mut valid);
    if valid != folded {
        return Err(ToolError(
            "Compaction would split reasoning or a tool call/result pair".into(),
        ));
    }
    for p in &projection.parts {
        if p.opaque && folded.contains(&p.id) {
            return Err(ToolError("Compaction would remove opaque content".into()));
        }
    }
    Ok(result)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    /// Part ids in request order.
    Messages(Vec<String>),
    /// Active block ids in request order.
    Summaries(Vec<String>),
}
#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: String,
    pub source: Source,
    pub start: String,
    pub end: String,
    pub first: String,
    pub last: String,
    /// Request position of the first source part; later candidates keep a longer cached prefix.
    pub position: usize,
    pub tokens: u64,
}
impl Candidate {
    fn new(p: &Prepared, source: Source, position: usize, tokens: u64) -> Self {
        let (kind, ids) = match &source {
            Source::Messages(ids) => ("messages", ids),
            Source::Summaries(ids) => ("summaries", ids),
        };
        let id = format!(
            "c_{}",
            &hash(format!("{kind}\n{}", ids.join("\n")).as_bytes())[..8]
        );
        let (start, end, first, last) = match &source {
            Source::Messages(ids) => {
                let part = |id: &String| p.projection.parts.iter().find(|v| &v.id == id);
                let named: Vec<_> = ids
                    .iter()
                    .filter_map(part)
                    .filter(|v| !v.text.trim().is_empty())
                    .collect();
                (
                    ids.first()
                        .and_then(part)
                        .map_or("", |v| p.state.reference(v)),
                    ids.last()
                        .and_then(part)
                        .map_or("", |v| p.state.reference(v)),
                    named.first().map(|v| excerpt(v)).unwrap_or_default(),
                    named.last().map(|v| excerpt(v)).unwrap_or_default(),
                )
            }
            Source::Summaries(ids) => {
                let title = |id: Option<&String>| {
                    id.and_then(|id| p.state.block(id))
                        .map_or(String::new(), |b| {
                            format!("{}: {}", b.id, b.topic.as_deref().unwrap_or("Summary"))
                        })
                };
                (
                    ids.first().map_or("", String::as_str),
                    ids.last().map_or("", String::as_str),
                    title(ids.first()),
                    title(ids.last()),
                )
            }
        };
        Self {
            id,
            start: start.into(),
            end: end.into(),
            first,
            last,
            source,
            position,
            tokens,
        }
    }
}
fn excerpt(part: &Part) -> String {
    let label = match (part.role, part.result, part.tool.is_empty()) {
        (Role::Tool, _, false) | (_, true, false) => format!("tool {}", part.tool),
        (Role::Assistant, false, false) => format!("assistant calls {}", part.tool),
        (Role::User, ..) => "user".into(),
        (Role::Tool, ..) => "tool".into(),
        _ => "assistant".into(),
    };
    let text: String = part
        .text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect();
    format!("{label}: {text}")
}
fn chars(parts: &[Part], indices: &[usize]) -> usize {
    indices
        .iter()
        .map(|i| parts[*i].text.encode_utf16().count())
        .sum()
}
/// Splits a structurally safe source set into contiguous runs.
fn safe_runs(parts: &[Part], indices: &[usize]) -> Vec<Vec<usize>> {
    let mut set: BTreeSet<_> = indices.iter().map(|i| parts[*i].id.clone()).collect();
    integrity(parts, &mut set);
    let mut runs: Vec<Vec<usize>> = vec![];
    for &i in indices.iter().filter(|i| set.contains(&parts[**i].id)) {
        match runs.last_mut() {
            Some(run) if run.last() == Some(&(i - 1)) => run.push(i),
            _ => runs.push(vec![i]),
        }
    }
    runs
}
fn whole(parts: &[Part], indices: &[usize]) -> bool {
    safe_runs(parts, indices).first().map(Vec::len) == Some(indices.len())
}

/// Replaceable sources in request order: old user turns first, long turns cut at safe
/// assistant-step boundaries, then runs of adjacent summaries. `bounds` limits positions.
pub fn candidates(
    p: &Prepared,
    min_chars: usize,
    segment_tokens: u64,
    bounds: Option<(usize, usize)>,
) -> Vec<Candidate> {
    let parts = &p.projection.parts;
    let covered = p.state.covered();
    let inside = |i: usize| bounds.is_none_or(|(a, b)| (a..=b).contains(&i));
    let mut runs: Vec<Vec<usize>> = vec![vec![]];
    for (i, part) in parts.iter().enumerate() {
        if p.protected.contains_key(&part.id) || covered.contains(&part.id) || !inside(i) {
            runs.push(vec![]);
        } else if let Some(run) = runs.last_mut() {
            run.push(i);
        }
    }
    let mut result = vec![];
    for run in runs.into_iter().filter(|r| !r.is_empty()) {
        let mut segments: Vec<Vec<usize>> = vec![];
        let mut current: Vec<usize> = vec![];
        let mut size = 0;
        let flush = |current: &mut Vec<usize>, segments: &mut Vec<Vec<usize>>, turn_end: bool| {
            if current.is_empty() {
                return;
            }
            let starts_turn = parts[current[0]].role == Role::User;
            // A short turn tail joins the previous segment of the same turn.
            if turn_end
                && chars(parts, current) < min_chars
                && let Some(previous) = segments.last_mut()
                && !starts_turn
            {
                previous.append(current);
                return;
            }
            segments.push(std::mem::take(current));
        };
        for &i in &run {
            let part = &parts[i];
            if current
                .last()
                .is_some_and(|&j| parts[j].owner != part.owner)
            {
                if part.role == Role::User {
                    flush(&mut current, &mut segments, true);
                    size = 0;
                } else if size >= segment_tokens && whole(parts, &current) {
                    flush(&mut current, &mut segments, false);
                    size = 0;
                }
            }
            current.push(i);
            size += part.tokens();
        }
        flush(&mut current, &mut segments, true);
        for segment in segments {
            for indices in safe_runs(parts, &segment) {
                if chars(parts, &indices) >= min_chars {
                    result.push(Candidate::new(
                        p,
                        Source::Messages(indices.iter().map(|i| parts[*i].id.clone()).collect()),
                        indices[0],
                        indices.iter().map(|i| parts[*i].tokens()).sum(),
                    ));
                }
            }
        }
    }
    let mut blocks: Vec<_> = p
        .state
        .blocks
        .iter()
        .filter(|b| b.active)
        .filter_map(|b| {
            let positions: Vec<_> = parts
                .iter()
                .enumerate()
                .filter(|(_, v)| b.effective.contains(&v.id))
                .map(|(i, _)| i)
                .collect();
            Some((*positions.first()?, *positions.last()?, b))
        })
        .filter(|(a, b, _)| inside(*a) && inside(*b))
        .collect();
    blocks.sort_by_key(|v| v.0);
    let mut run: Vec<(usize, usize, &Block)> = vec![];
    let mut emit = |run: &mut Vec<(usize, usize, &Block)>| {
        if run.len() >= 2 {
            result.push(Candidate::new(
                p,
                Source::Summaries(run.iter().map(|v| v.2.id.clone()).collect()),
                run[0].0,
                run.iter().map(|v| tokens(&v.2.summary)).sum(),
            ));
        }
        run.clear();
    };
    for block in blocks {
        if let Some(previous) = run.last()
            && parts
                .get(previous.1 + 1..block.0)
                .is_some_and(|gap| gap.iter().any(|v| !covered.contains(&v.id)))
        {
            emit(&mut run);
        }
        run.push(block);
    }
    emit(&mut run);
    result.sort_by_key(|c| c.position);
    result
}

/// Why a candidate can no longer be replaced exactly as listed.
pub fn changed(p: &Prepared, source: &Source) -> Option<&'static str> {
    match source {
        Source::Messages(ids) => {
            let covered = p.state.covered();
            let mut indices = vec![];
            for id in ids {
                let Some(i) = p.projection.parts.iter().position(|v| &v.id == id) else {
                    return Some("source_missing");
                };
                if covered.contains(id) {
                    return Some("already_compacted");
                }
                if p.protected.contains_key(id) {
                    return Some("now_protected");
                }
                indices.push(i);
            }
            (!whole(&p.projection.parts, &indices)).then_some("splits_tool_pair")
        }
        Source::Summaries(ids) => ids.iter().find_map(|id| match p.state.block(id) {
            Some(b) if b.active => None,
            Some(_) if p.state.blocks.iter().any(|o| o.children.contains(id)) => {
                Some("already_compacted")
            }
            _ => Some("source_missing"),
        }),
    }
}

pub struct Item<'a> {
    pub candidate: &'a str,
    pub source: &'a Source,
    pub summary: &'a str,
    pub title: Option<&'a str>,
}
/// Appends one block per item in a single state; sources must already be validated and disjoint.
pub fn commit(p: &Prepared, items: &[Item], call: &str) -> State {
    let mut state = p.state.clone();
    state.version = VERSION;
    state.fold = None;
    let parts = &p.projection.parts;
    for item in items {
        let ordered = |ids: &BTreeSet<&String>| -> Vec<String> {
            parts
                .iter()
                .filter(|v| ids.contains(&v.id))
                .map(|v| v.id.clone())
                .collect()
        };
        let (direct, children): (Vec<String>, Vec<String>) = match item.source {
            Source::Messages(ids) => (ids.clone(), vec![]),
            Source::Summaries(ids) => (vec![], ids.clone()),
        };
        let child_blocks: Vec<_> = children.iter().filter_map(|id| state.block(id)).collect();
        let effective = ordered(
            &direct
                .iter()
                .chain(child_blocks.iter().flat_map(|b| &b.effective))
                .collect(),
        );
        let (start, end) = match item.source {
            Source::Messages(_) => {
                let reference = |id: Option<&String>| {
                    id.and_then(|id| parts.iter().find(|v| &v.id == id))
                        .map_or(String::new(), |v| state.reference(v).into())
                };
                (reference(direct.first()), reference(direct.last()))
            }
            Source::Summaries(_) => (
                children.first().cloned().unwrap_or_default(),
                children.last().cloned().unwrap_or_default(),
            ),
        };
        let block = Block {
            id: format!("b{}", state.next_block),
            tier: child_blocks
                .iter()
                .map(|b| b.tier + 1)
                .max()
                .unwrap_or(1)
                .min(3),
            summary: item.summary.into(),
            topic: item.title.map(Into::into),
            tokens: parts
                .iter()
                .filter(|v| direct.contains(&v.id))
                .map(Part::tokens)
                .sum::<u64>()
                + child_blocks.iter().map(|b| tokens(&b.summary)).sum::<u64>(),
            direct,
            effective,
            children,
            call: call.into(),
            candidate: Some(item.candidate.into()),
            start,
            end,
            active: true,
        };
        for old in &mut state.blocks {
            if block.children.contains(&old.id) {
                old.active = false;
            }
        }
        state.blocks.push(block);
        state.next_block += 1;
    }
    state
}

/// First and last request positions of an mNNNNN ref or of a bN block's sources.
pub fn span(p: &Prepared, reference: &str) -> Option<(usize, usize)> {
    let parts = &p.projection.parts;
    if reference.starts_with('m') {
        let i = parts
            .iter()
            .position(|v| p.state.reference(v) == reference)?;
        Some((i, i))
    } else {
        let block = p.state.block(reference)?;
        let first = parts.iter().position(|v| block.effective.contains(&v.id))?;
        let last = parts
            .iter()
            .rposition(|v| block.effective.contains(&v.id))?;
        Some((first, last))
    }
}

pub struct Record {
    pub reference: String,
    pub role: Option<Role>,
    pub tool: Option<String>,
    pub call: Option<String>,
    pub text: String,
    pub opaque: bool,
    /// Present only for a summary record.
    pub source_refs: Option<Vec<String>>,
}
/// Saved text for a ref: a message part, a block's raw sources, or the block summary itself.
pub fn records(p: &Prepared, reference: &str, summary: bool) -> Result<Vec<Record>, &'static str> {
    let message = |part: &Part| Record {
        reference: p.state.reference(part).into(),
        role: Some(part.role),
        tool: (!part.tool.is_empty()).then(|| part.tool.clone()),
        call: part
            .result
            .then(|| {
                p.history
                    .parts
                    .iter()
                    .find(|v| !v.result && v.call == part.call)
                    .map(|v| p.state.reference(v).to_string())
            })
            .flatten(),
        text: part.text.clone(),
        opaque: part.opaque,
        source_refs: None,
    };
    if reference.starts_with('m') {
        if summary {
            return Err("view_unsupported");
        }
        return p
            .history
            .parts
            .iter()
            .find(|m| p.state.reference(m) == reference)
            .map(|m| vec![message(m)])
            .ok_or("unknown_ref");
    }
    let block = p.state.block(reference).ok_or("unknown_ref")?;
    if summary {
        return Ok(vec![Record {
            reference: block.id.clone(),
            role: None,
            tool: None,
            call: None,
            text: block.summary.clone(),
            opaque: false,
            source_refs: Some(if block.children.is_empty() {
                vec![block.start.clone(), block.end.clone()]
            } else {
                block.children.clone()
            }),
        }]);
    }
    if !block
        .effective
        .iter()
        .all(|id| p.history.parts.iter().any(|m| &m.id == id))
    {
        return Err("source_unavailable");
    }
    Ok(p.history
        .parts
        .iter()
        .filter(|m| block.effective.contains(&m.id) && !m.text.is_empty())
        .map(message)
        .collect())
}
