//! Native checkpoint preparation shared with compaction contributors.

use crate::{
    CancellationToken, LifecycleContext, ToolError, async_trait,
    model::{Message, Usage},
    protocol::CompactionReason,
    serde_json::Value,
};

#[derive(Clone, Debug)]
pub struct CompactionPreparation {
    pub first_kept_entry_id: String,
    pub messages: Vec<Message>,
    pub turn_prefix: Vec<Message>,
    pub previous_summary: Option<String>,
    pub tokens_before: u64,
    pub read_files: Vec<String>,
    pub modified_files: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct CompactionResult {
    pub summary: String,
    pub first_kept_entry_id: String,
    pub tokens_before: u64,
    pub usage: Option<Usage>,
    pub details: Value,
}

pub struct CompactionContext {
    pub lifecycle: LifecycleContext,
    pub branch: crate::BranchView,
    pub preparation: CompactionPreparation,
    pub instructions: Option<String>,
    pub reason: CompactionReason,
    pub will_retry: bool,
}

#[derive(Default)]
pub enum CompactionOutcome {
    #[default]
    Continue,
    Cancel,
    Summary(CompactionResult),
}

/// Context a checkpoint would lose, supplied again after its summary and persisted with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredContext {
    /// The contributor, so a later checkpoint can carry its items forward.
    pub source: String,
    /// Identity such as a path; a later item with the same key is dropped.
    pub key: String,
    /// Version of `text`, such as a content digest.
    pub version: String,
    pub text: String,
}

pub struct RestoreContext {
    pub lifecycle: LifecycleContext,
    /// Messages the summary replaces, including any a degraded summary omitted.
    pub summarized: Vec<Message>,
    /// Messages kept verbatim after the summary; their content needs no restoring.
    pub retained: Vec<Message>,
    /// Items restored by the checkpoint being replaced; they are dropped unless restored again.
    pub previous: Vec<RestoredContext>,
    /// Items above this estimate are dropped; contributors trim to fit.
    pub item_tokens: u64,
    /// Items are kept in contribution order until this estimate is spent.
    pub total_tokens: u64,
    /// Shared host estimate.
    pub estimate: fn(&str) -> u64,
}

#[async_trait]
pub trait Compactor: Send + Sync {
    /// Runs once per logical compaction, before any summary request or retry.
    async fn before_compact(
        &self,
        _context: &CompactionContext,
        _cancel: CancellationToken,
    ) -> Result<CompactionOutcome, ToolError> {
        Ok(CompactionOutcome::Continue)
    }

    /// Runs after the summary and before commit; return the most important items first.
    /// An error fails the checkpoint without committing it.
    async fn restore(
        &self,
        _context: &RestoreContext,
        _cancel: CancellationToken,
    ) -> Result<Vec<RestoredContext>, ToolError> {
        Ok(Vec::new())
    }
}
