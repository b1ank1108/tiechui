//! Read-only context panels contain display data, never provider replay payloads.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::TokenUsage;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContextView {
    #[default]
    Overview,
    System,
    Tools,
    Injections,
    Messages,
    Cache,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextItem {
    pub label: String,
    pub category: String,
    pub source: String,
    pub tokens: u64,
    pub text: String,
    pub group: String,
    pub json: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacheResponse {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub compression: u64,
    pub input: u64,
    pub cached: u64,
    pub fresh: u64,
    pub unattributed: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacheDiagnostics {
    #[serde(default, skip_serializing_if = "is_zero")]
    pub folds: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reclaimed: u64,
    /// Folds that capacity pressure applied during a cache cooldown.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub forced: u64,
    pub responses: Vec<CacheResponse>,
    pub omitted: usize,
    pub unavailable: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContextSnapshot {
    pub model: String,
    /// Zero means unknown, as in ModelInfo.
    pub window: u64,
    pub input_tokens: u64,
    pub provider_usage: Option<TokenUsage>,
    pub total: u64,
    pub system: Vec<ContextItem>,
    pub tools: Vec<ContextItem>,
    pub injections: Vec<ContextItem>,
    pub messages: Vec<ContextItem>,
    pub cache: CacheDiagnostics,
    pub notices: Vec<String>,
}

// Also available to non-terminal command clients without exposing prompt bodies.
pub fn overview(snapshot: &ContextSnapshot) -> Vec<String> {
    let mut lines = vec![
        format!("Model: {}", snapshot.model),
        format!(
            "Context window: {}",
            if snapshot.window == 0 {
                "unknown".into()
            } else {
                snapshot.window.to_string()
            }
        ),
        format!("Session context estimate: {} tokens", snapshot.input_tokens),
        format!(
            "Remaining (session estimate): {}",
            if snapshot.window == 0 {
                "unknown".into()
            } else {
                snapshot
                    .window
                    .saturating_sub(snapshot.input_tokens)
                    .to_string()
            }
        ),
        format!("Category estimate: ~{} tokens", snapshot.total),
    ];
    if let Some(usage) = snapshot.provider_usage {
        lines.push(format!(
            "Last provider response: {} input · {} output · {} cache reads · {} cache writes",
            usage.input + usage.cache_read + usage.cache_write,
            usage.output,
            usage.cache_read,
            usage.cache_write
        ));
    } else {
        lines.push("Last provider response: unavailable".into());
    }
    let mut groups: Vec<(String, u64)> = vec![];
    for item in snapshot
        .system
        .iter()
        .chain(&snapshot.tools)
        .chain(&snapshot.messages)
    {
        if let Some((_, count)) = groups.iter_mut().find(|(name, _)| name == &item.category) {
            *count += item.tokens;
        } else {
            groups.push((item.category.clone(), item.tokens));
        }
    }
    lines.push(String::new());
    lines.extend(
        groups
            .into_iter()
            .map(|(name, count)| format!("{name}: ~{count} tokens")),
    );
    lines.push(String::new());
    lines.extend(snapshot.notices.clone());
    lines
}

pub fn cache_lines(cache: &CacheDiagnostics) -> Vec<String> {
    let input: u64 = cache.responses.iter().map(|r| r.input).sum();
    let cached: u64 = cache.responses.iter().map(|r| r.cached).sum();
    let mut lines = vec![format!("Provider cache hit rate: {}", if input == 0 { "unknown".into() } else { format!("{:.1}%", cached as f64 * 100.0 / input as f64) }),
        format!("Reported input: {input} · cache reads: {cached}"), format!("Cache misses: {}", input - cached),
        format!("Estimated fresh input: {}", cache.responses.iter().map(|r| r.fresh).sum::<u64>()),
        format!("Unattributed misses: {}", cache.responses.iter().map(|r| r.unattributed).sum::<u64>()),
        format!("Usage window: {} responses on the current branch; {} earlier responses omitted", cache.responses.len(), cache.omitted),
        format!("{} assistant record(s) with unavailable usage", cache.unavailable),
        "Fresh-input attribution is an estimate, not a bill or causal proof. Unattributed includes cold starts, TTL/eviction, prompt/tool changes and provider rewrites; it does not prove a TTL expiry.".into(),
        "Model or system changes and unavailable usage reset the baseline. No extra model requests are made.".into()];
    if cache.folds > 0 {
        lines.insert(
            4,
            format!(
                "Estimated compression-associated misses: {}",
                cache.responses.iter().map(|r| r.compression).sum::<u64>()
            ),
        );
        let mut events = format!(
            "{} recorded compression event(s) · ~{} net context tokens reclaimed",
            cache.folds, cache.reclaimed
        );
        if cache.forced > 0 {
            events.push_str(&format!(
                " · {} over cache cooldown for capacity",
                cache.forced
            ));
        }
        lines.push(events);
        lines.push("Compression attribution covers at most four cold responses after a fold; estimates are not causal proof or billing savings.".into());
    }
    lines
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}
