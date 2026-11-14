mod prepare;

use super::*;
use tiechui_plugin_api::{
    CompactionConfig, CompactionContext, CompactionOutcome, CompactionPreparation,
    CompactionResult, RestoreContext, RestoredContext, ToolError,
};

/// Reductions of one operation's summary input; each omits at least one more round.
const MAX_SHRINKS: u32 = 2;
/// Consecutive automatic failures on one source state before automatic compaction pauses.
const MAX_FAILURES: u32 = 3;

pub(super) fn should_compact(tokens: u64, window: u64, config: &CompactionConfig) -> bool {
    config.enabled
        && window > 0
        && (config.reserve_tokens > window || tokens > window.saturating_sub(config.reserve_tokens))
}

fn text_tokens(text: &str) -> u64 {
    tiechui_providers::estimate::messages([&Message {
        role: Role::User,
        content: vec![text_content(text.to_owned())],
        opaque: None,
    }])
}

/// Pauses automatic compaction until the latest model-visible entry changes.
#[derive(Default)]
pub(super) struct Breaker {
    source: Option<String>,
    failures: u32,
    stalled: bool,
}

impl Breaker {
    fn paused(&self, source: &Option<String>) -> bool {
        self.source == *source && (self.failures >= MAX_FAILURES || self.stalled)
    }

    /// Returns whether this failure pauses automatic compaction.
    fn fail(&mut self, source: Option<String>) -> bool {
        if self.source != source {
            *self = Self {
                source,
                ..Self::default()
            };
        }
        self.failures += 1;
        self.failures == MAX_FAILURES
    }
}

/// One logical compaction; retries and shrinks share its request budget.
struct Operation {
    reason: CompactionReason,
    retries: u32,
    shrinks: u32,
    usage: Usage,
    omitted: Vec<serde_json::Value>,
}

impl SessionActor {
    fn compaction_settings(&self) -> CompactionConfig {
        self.config
            .compaction
            .for_model(&format!("{}/{}", self.model.provider, self.model.model.0))
    }

    fn context_tokens(&self) -> Result<u64, String> {
        let store = self.store()?;
        if let Some(tokens) = estimate::anchored(store, &self.excluded)? {
            return Ok(tokens);
        }
        // Without usage, estimate the whole next request: system prompt and tools included.
        let mut messages = store.build_context_excluding(&self.excluded);
        messages.splice(0..0, self.system_message());
        Ok(tiechui_providers::estimate::input(
            &messages,
            &self.visible_tools(),
        ))
    }

    /// The system prompt and tools, which no checkpoint can shrink.
    fn prefix_tokens(&self) -> u64 {
        tiechui_providers::estimate::input(self.system_message().as_slice(), &self.visible_tools())
    }

    /// Maintenance entries such as usage, recovery and checkpoints leave the source unchanged.
    fn compaction_source(&self) -> Result<Option<String>, String> {
        let store = self.store()?;
        Ok(store
            .path_to_root(store.current_leaf().unwrap_or(&store.header().id))
            .map_err(|error| error.to_string())?
            .into_iter()
            .rev()
            .find(|entry| {
                entry.kind.context_message().is_some() && !self.excluded.contains(&entry.id)
            })
            .map(|entry| entry.id.clone()))
    }

    fn progress(&self, operation: &Operation, phase: CompactionPhase) {
        self.emit(EventKind::CompactionProgress {
            reason: operation.reason,
            phase,
            retries: operation.retries,
            shrinks: operation.shrinks,
        });
    }

    pub(super) async fn auto_compact(&mut self) -> Result<(), String> {
        if self.cancel.is_cancelled() || !self.config.compaction.enabled {
            return Ok(());
        }
        let window = self.model_info(&self.model).context_window;
        let settings = self.compaction_settings();
        // When the prefix alone crosses the threshold every checkpoint would stall; only
        // overflow recovery and `/compact` remain.
        if should_compact(self.context_tokens()?, window, &settings)
            && !should_compact(self.prefix_tokens(), window, &settings)
        {
            self.compact(CompactionReason::Threshold, None, false)
                .await?;
        }
        Ok(())
    }

    pub(super) async fn compact(
        &mut self,
        reason: CompactionReason,
        instructions: Option<String>,
        will_retry: bool,
    ) -> Result<bool, String> {
        let source = self.compaction_source()?;
        if reason == CompactionReason::Manual {
            self.breaker = Breaker::default();
        } else if self.breaker.paused(&source) {
            return Ok(false);
        }
        let prepared = prepare::prepare(
            self.store()?,
            &self.excluded,
            self.compaction_settings().keep_recent_tokens,
            self.context_tokens()?,
        );
        if prepared.is_none() && reason != CompactionReason::Manual {
            return Ok(false);
        }
        self.compacting = true;
        self.emit(EventKind::CompactionStarted { reason });
        let mut operation = Operation {
            reason,
            retries: 0,
            shrinks: 0,
            usage: Usage::default(),
            omitted: vec![],
        };
        let result = if let Some(prepared) = prepared {
            self.compact_inner(prepared, &mut operation, instructions, will_retry)
                .await
        } else if self.store()?.entries().iter().any(|entry| {
            Some(entry.id.as_str()) == self.store.as_ref().and_then(|store| store.current_leaf())
                && matches!(entry.kind, EntryKind::Compaction { .. })
        }) {
            Err("Already compacted".into())
        } else {
            Err("Nothing to compact (session too small)".into())
        };
        self.compacting = false;
        let aborted = self.cancel.is_cancelled()
            || result
                .as_ref()
                .is_err_and(|message| message == "Compaction cancelled");
        let error = result.as_ref().err().filter(|_| !aborted).map(|message| {
            format!(
                "{}: {message}",
                match reason {
                    CompactionReason::Manual => "Compaction failed",
                    CompactionReason::Threshold => "Auto-compaction failed",
                    CompactionReason::Overflow => "Context overflow recovery failed",
                }
            )
        });
        let resume = "it resumes after new input or a model response, or run /compact";
        let paused = if aborted {
            None
        } else if result.is_err() {
            (reason != CompactionReason::Manual && self.breaker.fail(source)).then(|| {
                format!("Automatic compaction paused after {MAX_FAILURES} consecutive failures on the same history; {resume}.")
            })
        } else {
            let window = self.model_info(&self.model).context_window;
            let settings = self.compaction_settings();
            let tokens = self.context_tokens()?;
            if should_compact(tokens, window, &settings) {
                self.breaker = Breaker {
                    source,
                    failures: 0,
                    stalled: true,
                };
                Some(format!(
                    "Automatic compaction paused: the context is still {tokens} tokens after the checkpoint, above the {} token threshold; {resume}.",
                    window.saturating_sub(settings.reserve_tokens)
                ))
            } else {
                self.breaker = Breaker::default();
                None
            }
        };
        self.emit(EventKind::CompactionEnded {
            reason,
            aborted,
            error,
            will_retry: result.is_ok() && will_retry,
            paused,
        });
        Ok(result.is_ok())
    }

    async fn compact_inner(
        &mut self,
        prepared: prepare::Prepared,
        operation: &mut Operation,
        instructions: Option<String>,
        will_retry: bool,
    ) -> Result<(), String> {
        let messages = self.store()?.build_context_excluding(&self.excluded);
        let model = self.model.clone();
        let context = CompactionContext {
            lifecycle: LifecycleContext {
                session_id: self.thread.0.clone(),
                turn_id: self
                    .turn
                    .as_ref()
                    .map(|turn| turn.id.0.clone())
                    .unwrap_or_default(),
                cwd: self.store()?.header().cwd.clone(),
                transcript_path: Some(self.store()?.path().to_owned()),
                model: self.model.model.0.clone(),
            },
            branch: self.branch_view()?,
            preparation: prepared.preparation,
            instructions,
            reason: operation.reason,
            will_retry,
        };
        self.progress(operation, CompactionPhase::Prepare);
        let mut supplied = None;
        for compactor in self.registry.clone().compactors() {
            let cancel = self.cancel.clone();
            let outcome = self
                .drive(async {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => Err(ToolError("Compaction cancelled".into())),
                        result = compactor.before_compact(&context, cancel.clone()) => result,
                    }
                })
                .await?
                .map_err(|e| e.to_string())?;
            match outcome {
                CompactionOutcome::Continue => {}
                CompactionOutcome::Cancel => return Err("Compaction cancelled".into()),
                CompactionOutcome::Summary(result) => supplied = Some(result),
            }
        }
        let from_extension = supplied.is_some();
        self.progress(operation, CompactionPhase::Summarize);
        let mut result = if let Some(result) = supplied {
            result
        } else {
            self.generate_summary(
                &context.preparation,
                &prepared.sources,
                context.instructions.as_deref(),
                operation,
            )
            .await?
        };
        if self.cancel.is_cancelled() {
            return Err("Compaction cancelled".into());
        }
        self.progress(operation, CompactionPhase::Restore);
        let restored = self
            .restore_context(
                &context,
                prepared.retained,
                prepared.restored,
                &result.summary,
            )
            .await?;
        if self.cancel.is_cancelled() {
            return Err("Compaction cancelled".into());
        }
        if self.model != model || self.store()?.build_context_excluding(&self.excluded) != messages
        {
            return Err("Session changed during compaction".into());
        }
        if !self
            .store()?
            .path_to_root(self.store()?.current_leaf().unwrap_or(&self.thread.0))
            .map_err(|e| e.to_string())?
            .iter()
            .any(|entry| entry.id == result.first_kept_entry_id)
        {
            return Err("First kept entry is not on the current branch".into());
        }
        self.progress(operation, CompactionPhase::Commit);
        if !restored.is_empty() {
            if result.details.is_null() {
                result.details = serde_json::json!({});
            }
            if let Some(details) = result.details.as_object_mut() {
                let items = restored.iter().map(|item| serde_json::json!({
                    "source": item.source, "key": item.key, "version": item.version, "text": item.text,
                }));
                details.insert("restored".into(), items.collect());
            }
        }
        let degraded = result.details["degraded"] == true;
        let id = self
            .append(EntryKind::Compaction {
                summary: result.summary.clone(),
                first_kept_entry_id: Some(result.first_kept_entry_id),
                system_message: None,
                tokens_before: result.tokens_before,
                usage: result.usage,
                details: result.details,
                from_extension,
            })
            .await?;
        if let Some(usage) = result.usage {
            add_usage(&mut self.total, usage);
        }
        self.context = None;
        self.last_request = None;
        self.usage_prefixes.clear();
        self.retry_available = false;
        self.emit(EventKind::RetryAvailable { available: false });
        self.item(KnownItem::Compaction {
            id: ItemId(id),
            summary: result.summary,
            tokens_before: result.tokens_before,
            degraded,
        });
        self.publish_session();
        Ok(())
    }

    /// Contributors re-supply context the checkpoint loses, within half the remaining headroom.
    async fn restore_context(
        &mut self,
        context: &CompactionContext,
        retained: Vec<Message>,
        previous: Vec<RestoredContext>,
        summary: &str,
    ) -> Result<Vec<RestoredContext>, String> {
        let window = self.model_info(&self.model).context_window;
        let threshold = window.saturating_sub(self.compaction_settings().reserve_tokens);
        let base = text_tokens(summary) + tiechui_providers::estimate::messages(&retained);
        let total_tokens = (window / 10).min(threshold.saturating_sub(base) / 2);
        if total_tokens == 0 {
            return Ok(vec![]);
        }
        let restore = RestoreContext {
            lifecycle: context.lifecycle.clone(),
            summarized: context
                .preparation
                .messages
                .iter()
                .chain(&context.preparation.turn_prefix)
                .cloned()
                .collect(),
            retained,
            previous,
            item_tokens: (window / 40).min(total_tokens),
            total_tokens,
            estimate: text_tokens,
        };
        let mut items = Vec::new();
        for compactor in self.registry.clone().compactors() {
            let cancel = self.cancel.clone();
            items.extend(
                self.drive(async {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => Err(ToolError("Compaction cancelled".into())),
                        result = compactor.restore(&restore, cancel.clone()) => result,
                    }
                })
                .await?
                .map_err(|e| e.to_string())?,
            );
        }
        let mut keys = HashSet::new();
        let mut spent = 0;
        items.retain(|item| {
            let tokens = text_tokens(&item.text);
            let keep = tokens <= restore.item_tokens
                && spent + tokens <= restore.total_tokens
                && keys.insert(item.key.clone());
            if keep {
                spent += tokens;
            }
            keep
        });
        Ok(items)
    }

    async fn generate_summary(
        &mut self,
        preparation: &CompactionPreparation,
        sources: &[String],
        instructions: Option<&str>,
        operation: &mut Operation,
    ) -> Result<CompactionResult, String> {
        let (history_sources, prefix_sources) = sources.split_at(preparation.messages.len());
        let history = if preparation.messages.is_empty() {
            preparation
                .previous_summary
                .clone()
                .unwrap_or_else(|| "No prior history.".into())
        } else {
            self.summarize(
                &preparation.messages,
                history_sources,
                preparation.previous_summary.as_deref(),
                instructions,
                false,
                operation,
            )
            .await?
        };
        let mut summary = if preparation.turn_prefix.is_empty() {
            history
        } else {
            let text = self
                .summarize(
                    &preparation.turn_prefix,
                    prefix_sources,
                    None,
                    instructions,
                    true,
                    operation,
                )
                .await?;
            format!("{history}\n\n---\n\n**Turn Context (split turn):**\n\n{text}")
        };
        let mut details = serde_json::json!({"read_files": preparation.read_files, "modified_files": preparation.modified_files});
        if !operation.omitted.is_empty() {
            let count: u64 = operation
                .omitted
                .iter()
                .filter_map(|range| range["messages"].as_u64())
                .sum();
            summary = format!(
                "Note: {count} earlier messages did not fit the summary request and are not covered by this summary; they remain in the session history and can be recovered with history tools.\n\n{summary}"
            );
            details["degraded"] = true.into();
            details["omitted"] = serde_json::json!(operation.omitted);
        }
        for (tag, files) in [
            ("read-files", &preparation.read_files),
            ("modified-files", &preparation.modified_files),
        ] {
            if !files.is_empty() {
                summary.push_str(&format!("\n\n<{tag}>\n{}\n</{tag}>", files.join("\n")));
            }
        }
        Ok(CompactionResult {
            summary,
            first_kept_entry_id: preparation.first_kept_entry_id.clone(),
            tokens_before: preparation.tokens_before,
            usage: Some(operation.usage),
            details,
        })
    }

    /// Omits the oldest whole rounds after the opening request until the input fits, at most
    /// `MAX_SHRINKS` times per operation. Only this request copy changes; the session does not.
    async fn summarize(
        &mut self,
        messages: &[Message],
        sources: &[String],
        previous: Option<&str>,
        instructions: Option<&str>,
        split: bool,
        operation: &mut Operation,
    ) -> Result<String, String> {
        let label = if split {
            "Turn prefix summarization"
        } else {
            "Summarization"
        };
        let provider = self
            .config
            .providers
            .get(&self.model.provider)
            .cloned()
            .ok_or("unknown provider")?;
        let rounds = prepare::rounds(messages);
        let parts: Vec<_> = rounds
            .iter()
            .map(|round| prepare::serialize(&messages[round.clone()]))
            .collect();
        let costs: Vec<_> = parts.iter().map(|part| text_tokens(part)).collect();
        // The opening request carries the intent and the last round the latest state.
        let pinned = usize::from(messages.first().is_some_and(|m| m.role == Role::User));
        let last = rounds.len().saturating_sub(1).max(pinned);
        let omitted_messages = |omitted: usize| {
            rounds[pinned..pinned + omitted]
                .iter()
                .map(ExactSizeIterator::len)
                .sum::<usize>()
        };
        // The summary needs output room, but never more than the model can generate.
        let capabilities = provider.capabilities(&self.model.model);
        let mut target = capabilities.context_window.saturating_sub(
            self.compaction_settings()
                .reserve_tokens
                .min(capabilities.max_output),
        );
        let mut omitted = 0;
        let mut ambiguous = false;
        loop {
            if self.cancel.is_cancelled() {
                return Err("Compaction cancelled".into());
            }
            let build = |omitted| {
                self.summary_request(prepare::prompt(
                    &prepare::conversation(&parts, pinned, omitted, omitted_messages(omitted)),
                    previous,
                    instructions,
                    split,
                ))
            };
            let start = omitted;
            let mut request = build(omitted);
            let mut tokens = tiechui_providers::estimate::messages(&request.messages);
            while tokens > target && pinned + omitted < last {
                let mut excess = tokens - target;
                while excess > 0 && pinned + omitted < last {
                    excess = excess.saturating_sub(costs[pinned + omitted].max(1));
                    omitted += 1;
                }
                request = build(omitted);
                tokens = tiechui_providers::estimate::messages(&request.messages);
            }
            if tokens > target {
                return Err(format!(
                    "{label} input is about {tokens} tokens, above its {target} token budget, and no earlier round can be omitted"
                ));
            }
            if omitted > start {
                if operation.shrinks == MAX_SHRINKS {
                    return Err(format!(
                        "{label} input still does not fit after {MAX_SHRINKS} reductions"
                    ));
                }
                operation.shrinks += 1;
                self.progress(operation, CompactionPhase::Summarize);
            }
            let cancel = self.cancel.clone();
            let error = match self
                .drive(complete(provider.clone(), request, cancel))
                .await?
            {
                Ok((message, reason, usage)) => {
                    if reason == StopReason::Length {
                        return Err(format!(
                            "{label} failed: generation hit the token cap and the summary is incomplete"
                        ));
                    }
                    if !tool_calls(&message).is_empty() {
                        return Err(format!("{label} attempted to call a tool"));
                    }
                    add_usage(&mut operation.usage, usage);
                    if omitted > 0 {
                        let first = rounds[pinned].start;
                        let end = rounds[pinned + omitted - 1].end;
                        operation.omitted.push(serde_json::json!({
                            "from": sources[first], "to": sources[end - 1], "messages": end - first,
                        }));
                    }
                    return Ok(message
                        .content
                        .iter()
                        .filter_map(|part| match part {
                            Content::Text { text, .. } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n"));
                }
                Err(error) => error,
            };
            if self.cancel.is_cancelled() || error.kind == ErrorKind::Cancelled {
                return Err("Compaction cancelled".into());
            }
            if error.kind == ErrorKind::ContextOverflow {
                // The estimate undercounted this input: omit half of what may still be omitted.
                let spare: u64 = costs[pinned + omitted..last].iter().sum();
                if spare == 0
                    || operation.shrinks == MAX_SHRINKS
                    || self
                        .config
                        .retry
                        .max_retries()
                        .is_some_and(|max| operation.retries >= max)
                {
                    return Err(format!("{label} failed: {error}"));
                }
                target = tokens.saturating_sub(spare.div_ceil(2));
                operation.retries += 1;
                continue;
            }
            let delay = if matches!(
                error.kind,
                ErrorKind::RateLimit
                    | ErrorKind::Overloaded
                    | ErrorKind::TransientNetwork
                    | ErrorKind::AmbiguousRequest
            ) && !(ambiguous && error.kind == ErrorKind::AmbiguousRequest)
            {
                self.config.retry.delay(&error, operation.retries)
            } else {
                None
            };
            let Some(delay) = delay else {
                return Err(format!("{label} failed: {error}"));
            };
            ambiguous |= error.kind == ErrorKind::AmbiguousRequest;
            operation.retries += 1;
            self.retry_event(operation.retries, Some(delay), &error.message);
            self.progress(operation, CompactionPhase::Summarize);
            let cancel = self.cancel.clone();
            self.drive(async move { tokio::select! { biased; _ = cancel.cancelled() => {}, _ = tokio::time::sleep(delay) => {} }}).await?;
            if self.cancel.is_cancelled() {
                return Err("Compaction cancelled".into());
            }
            self.retry_event(operation.retries, None, &error.message);
        }
    }

    fn summary_request(&self, prompt: String) -> ModelRequest {
        ModelRequest {
            disable_cache: true,
            short_cache: false,
            cache_key: None,
            model: self.model.model.clone(),
            messages: vec![
                Message {
                    role: Role::System,
                    content: vec![text_content(include_str!("system.txt").into())],
                    opaque: None,
                },
                Message {
                    role: Role::User,
                    content: vec![text_content(prompt)],
                    opaque: None,
                },
            ],
            tools: vec![],
            input_tokens: None,
            max_output: None,
            reasoning: self.thinking,
        }
    }
}

async fn complete(
    provider: Arc<dyn ModelProvider>,
    request: ModelRequest,
    cancel: CancellationToken,
) -> Result<(Message, StopReason, Usage), ProviderError> {
    let model = request.model.clone();
    let cancelled = || provider_error(ErrorKind::Cancelled, "Compaction cancelled");
    let mut stream = tokio::select! { biased; _ = cancel.cancelled() => return Err(cancelled()), result = provider.stream(request, cancel.clone()) => result? };
    let mut response = stream::Response::default();
    let mut model = model;
    loop {
        let event = tokio::select! { biased; _ = cancel.cancelled() => return Err(cancelled()), event = stream.next() => event };
        match event {
            Some(Ok(StreamEvent::Finish { reason, mut usage })) => {
                if reason == StopReason::Cancelled {
                    return Err(cancelled());
                }
                if reason == StopReason::Error {
                    return Err(provider_error(
                        ErrorKind::InvalidRequest,
                        "provider ended with an error",
                    ));
                }
                if usage.cost_nano_usd.is_none() {
                    usage.cost_nano_usd = provider.cost_nano_usd(&model, usage);
                }
                return Ok((response.message(), reason, usage));
            }
            Some(Ok(StreamEvent::ResponseModel { model: next })) => model = next,
            Some(Ok(event)) => response.apply(event),
            Some(Err(error)) => return Err(error),
            None => {
                return Err(provider_error(
                    ErrorKind::TransientNetwork,
                    "stream ended without finish",
                ));
            }
        }
    }
}
