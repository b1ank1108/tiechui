//! Request observations are bounded display copies, independent of replay and persistence.

use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::io::Write;

use tiechui_plugin_api::{
    MeasuredMessage, PromptFragment, RequestObservation, ResponseUsage, SessionView, TurnInput,
};
use tiechui_providers::estimate;

use super::*;

const TEXT_LIMIT: usize = 1024 * 1024;
const ITEM_LIMIT: usize = 65536;

struct Preview {
    text: String,
    left: usize,
}
impl Preview {
    fn push(&mut self, text: &str) {
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if self.left < ch.len_utf8() {
                self.left = 0;
                break;
            }
            if ch == '\x1b' {
                match chars.next() {
                    Some('[') => {
                        for ch in chars.by_ref() {
                            if ('@'..='~').contains(&ch) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        let mut escape = false;
                        for ch in chars.by_ref() {
                            if ch == '\x07' || (escape && ch == '\\') {
                                break;
                            }
                            escape = ch == '\x1b';
                        }
                    }
                    _ => {}
                }
                continue;
            }
            // No terminal controls, including C1 and bidi formatting, reach a panel.
            if (ch.is_control() && !matches!(ch, '\n' | '\t'))
                || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            {
                continue;
            }
            self.text.push(ch);
            self.left -= ch.len_utf8();
        }
    }
    fn content(&mut self, content: &[Content]) {
        for part in content {
            if self.left == 0 {
                break;
            }
            if !self.text.is_empty() {
                self.push("\n\n");
            }
            match part {
                Content::Text { text, .. } => self.push(text),
                Content::Thinking {
                    text,
                    redacted,
                    opaque,
                } => {
                    if *redacted {
                        self.push("[redacted thinking omitted]");
                    } else {
                        self.push(text);
                    }
                    if opaque.is_some() {
                        self.push("\n[thinking signature omitted]");
                    }
                }
                Content::Image { mime_type, .. } => {
                    self.push("[image: ");
                    self.push(mime_type);
                    self.push("; data omitted]");
                }
                Content::ToolCall(call) => {
                    self.push(&call.name);
                    self.push("\n");
                    self.push(&serde_json::to_string_pretty(&call.arguments).unwrap_or_default());
                }
                Content::ToolResult(result) => self.content(&result.content),
            }
        }
    }
}

fn clip(text: &str, budget: &mut usize) -> String {
    let limit = (*budget).min(ITEM_LIMIT);
    let mut preview = Preview {
        text: String::new(),
        left: limit,
    };
    preview.push(text);
    *budget -= limit - preview.left;
    if preview.left == 0 {
        preview
            .text
            .push_str("\n[Diagnostic preview clipped; original request is unchanged.]");
    }
    preview.text
}

fn measured(message: &Message, index: usize, budget: &mut usize) -> MeasuredMessage {
    let limit = (*budget).min(ITEM_LIMIT);
    let mut preview = Preview {
        text: String::new(),
        left: limit,
    };
    preview.content(&message.content);
    *budget -= limit - preview.left;
    if preview.left == 0 {
        preview
            .text
            .push_str("\n[Diagnostic preview clipped; original request is unchanged.]");
    }
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "toolResult",
    };
    MeasuredMessage {
        role: message.role,
        item: ContextItem {
            label: format!("{role} #{}", index + 1),
            tokens: estimate::messages([message]),
            text: preview.text,
            source: "Current branch projection".into(),
            ..ContextItem::default()
        },
    }
}

// Hash directly into a writer so even image payloads are never retained in the observation.
struct Fingerprint(DefaultHasher);
impl Write for Fingerprint {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.write(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn fingerprint(message: &Message) -> u64 {
    let mut writer = Fingerprint(DefaultHasher::new());
    let _ = serde_json::to_writer(&mut writer, message);
    writer.0.finish()
}

fn tools_fingerprint(tools: &[tiechui_model::ToolSpec]) -> u64 {
    let mut writer = Fingerprint(DefaultHasher::new());
    let _ = serde_json::to_writer(&mut writer, tools);
    writer.0.finish()
}

pub(super) fn prefix(messages: &[Message], tools: &[tiechui_model::ToolSpec]) -> u64 {
    let mut writer = Fingerprint(DefaultHasher::new());
    for message in messages
        .iter()
        .filter(|message| message.role == Role::System)
    {
        let _ = serde_json::to_writer(&mut writer, message);
    }
    let _ = serde_json::to_writer(&mut writer, tools);
    writer.0.finish()
}

pub(super) fn observe(branch: &[Message], request: &[Message]) -> RequestObservation {
    let mut counts = BTreeMap::<u64, usize>::new();
    for message in branch {
        *counts.entry(fingerprint(message)).or_default() += 1;
    }
    let mut result = RequestObservation::default();
    let mut budget = TEXT_LIMIT;
    for (index, message) in request.iter().enumerate() {
        let count = counts.entry(fingerprint(message)).or_default();
        if *count > 0 {
            *count -= 1;
        } else {
            result.added.push(measured(message, index, &mut budget));
        }
    }
    for (index, message) in branch.iter().enumerate() {
        let count = counts.entry(fingerprint(message)).or_default();
        if *count > 0 {
            *count -= 1;
            result.removed.push(measured(message, index, &mut budget));
        }
    }
    result
}

impl SessionActor {
    fn tool_sources(&self, items: &mut [ContextItem]) {
        for item in items {
            if let Some(source) = self
                .registry
                .tools()
                .get(&item.label)
                .and_then(|tool| tool.source())
            {
                let mut budget = ITEM_LIMIT;
                let source = clip(&source, &mut budget);
                item.group = source.clone();
                item.source = format!("Current registry: {source}");
            }
        }
    }

    pub(super) fn observe_request(
        &self,
        messages: &[Message],
        system: Option<&Message>,
        tools: &[tiechui_model::ToolSpec],
    ) -> Result<RequestObservation, String> {
        let branch = self.store()?.build_context_excluding(&self.excluded);
        let mut observation = observe(&branch, messages);
        observation.tools_fingerprint = tools_fingerprint(tools);
        let mut blocks: Vec<_> = self
            .session_context
            .iter()
            .chain(&self.turn_context)
            .map(|text| PromptFragment {
                label: "Appended prompt".into(),
                category: "Appended prompt".into(),
                source: "Lifecycle contribution; extension attribution unavailable".into(),
                text: text.clone(),
                references: vec![],
            })
            .collect();
        blocks.extend(self.system_fragments.clone());
        if self.system_fragments.is_empty() {
            blocks.push(restored_prompt(&self.system));
        }
        let expected = assemble_system_prompt(
            &blocks
                .iter()
                .map(|block| block.text.clone())
                .collect::<Vec<_>>(),
        );
        let expected = (!expected.is_empty()).then(|| Message {
            role: Role::System,
            content: vec![text_content(expected)],
            opaque: None,
        });
        // The assembled prompt belongs to the fragment projection, not branch history.
        if system != expected.as_ref() {
            let used = observation
                .added
                .iter()
                .chain(&observation.removed)
                .map(|message| message.item.text.len())
                .sum();
            let mut budget = TEXT_LIMIT.saturating_sub(used);
            if let Some(system) = system {
                observation.added.push(measured(system, 0, &mut budget));
            }
            if let Some(expected) = expected {
                observation
                    .removed
                    .push(measured(&expected, 0, &mut budget));
            }
        }
        let mut budget = TEXT_LIMIT;
        let prompt = measure_prompt(&blocks, tools, &mut budget);
        observation.system = prompt.system;
        observation.references = prompt.references;
        observation.tools = prompt.tools;
        self.tool_sources(&mut observation.tools);
        Ok(observation)
    }

    pub(super) async fn session_view(
        &self,
        cancel: CancellationToken,
    ) -> Result<SessionView, String> {
        let mut fragments = self.system_fragments.clone();
        if fragments.is_empty() && self.retry_available && !self.system.is_empty() {
            fragments.push(restored_prompt(&self.system));
        }
        if fragments.is_empty() {
            let input = TurnInput {
                cwd: PathBuf::from(&self.store()?.header().cwd),
                content: vec![],
            };
            for provider in self.registry.context_providers() {
                let text = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err("Operation aborted".into()),
                    result = provider.fragments(&input, cancel.clone()) => result.map_err(|e| e.to_string())?,
                };
                fragments.extend(provider.describe(&text));
            }
        }
        let mut blocks: Vec<_> = self
            .session_context
            .iter()
            .chain(&self.turn_context)
            .map(|text| PromptFragment {
                label: "Appended prompt".into(),
                category: "Appended prompt".into(),
                source: "Lifecycle contribution; extension attribution unavailable".into(),
                text: text.clone(),
                references: vec![],
            })
            .collect();
        blocks.extend(fragments);
        let branch = if let Some(provider) = self.config.providers.get(&self.model.provider) {
            replay::prepare(
                self.store()?.build_context_with_origins(&self.excluded),
                &self.model,
                provider.capabilities(&self.model.model).images,
                provider.error_tool_images(),
                |id| provider.normalize_tool_call_id(id),
            )
        } else {
            self.store()?.build_context_excluding(&self.excluded)
        };
        let tools = self.visible_tools();
        let mut input = branch.clone();
        let system = assemble_system_prompt(
            &blocks
                .iter()
                .map(|block| block.text.clone())
                .collect::<Vec<_>>(),
        );
        if !system.is_empty() {
            input.insert(
                0,
                Message {
                    role: Role::System,
                    content: vec![text_content(system)],
                    opaque: None,
                },
            );
        }
        let mut view = SessionView {
            model: self.model.model.0.clone(),
            window: self.model_info(&self.model).context_window,
            input_tokens: super::estimate::input(self.store()?, &self.excluded, &input, &tools)?,
            request: self
                .last_request
                .as_ref()
                .filter(|request| request.tools_fingerprint == tools_fingerprint(&tools))
                .cloned(),
            ..SessionView::default()
        };
        let mut budget = TEXT_LIMIT;
        let prompt = view
            .request
            .clone()
            .unwrap_or_else(|| measure_prompt(&blocks, &tools, &mut budget));
        view.system = prompt.system;
        view.references = prompt.references;
        view.tools = prompt.tools;
        self.tool_sources(&mut view.tools);
        for (index, message) in branch.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }
            let measured = measured(message, index, &mut budget);
            if message.role == Role::System {
                view.system.push(ContextItem {
                    category: "System prompt".into(),
                    ..measured.item
                });
            } else {
                view.messages.push(measured);
            }
        }
        let store = self.store()?;
        let path = store
            .path_to_root(store.current_leaf().unwrap_or(&store.header().id))
            .map_err(|e| e.to_string())?;
        let mut pending = None;
        let mut pending_prefix = None;
        let mut previous_prefix = None;
        let mut reset = true;
        let mut responses = VecDeque::new();
        let mut fold: Option<tiechui_plugin_api::ContextFold> = None;
        for entry in path {
            match &entry.kind {
                EntryKind::Custom { kind, data } => {
                    for projection in self.registry.projections() {
                        if let Some(next) = projection.fold(kind, data) {
                            view.folds += 1;
                            view.forced_folds += usize::from(next.forced);
                            view.reclaimed =
                                view.reclaimed.saturating_add(next.before - next.after);
                            fold = Some(match fold.take() {
                                Some(previous) => tiechui_plugin_api::ContextFold {
                                    before: previous.before,
                                    after: next.after,
                                    prefix: previous.prefix.min(next.prefix),
                                    forced: previous.forced || next.forced,
                                },
                                None => next,
                            });
                        }
                    }
                }
                EntryKind::Usage {
                    usage, request_id, ..
                } => {
                    pending = Some(*usage);
                    let id = request_id.as_ref().unwrap_or(&entry.id);
                    pending_prefix = self
                        .usage_prefixes
                        .iter()
                        .find(|(key, _)| key == id)
                        .map(|(_, prefix)| *prefix);
                }
                EntryKind::ModelChange { .. } | EntryKind::Compaction { .. } => {
                    reset = true;
                    pending = None;
                    view.provider_usage = None;
                    fold = None;
                }
                EntryKind::Message {
                    message,
                    usage,
                    model,
                    stop_reason,
                    ..
                } => {
                    if message.role == Role::System {
                        reset = true;
                        fold = None;
                    }
                    if message.role != Role::Assistant {
                        continue;
                    }
                    let usage = usage.or(pending.take()).unwrap_or_default();
                    reset |= pending_prefix != previous_prefix;
                    previous_prefix = pending_prefix.take();
                    if usage
                        .input
                        .saturating_add(usage.cache_read)
                        .saturating_add(usage.cache_write)
                        == 0
                    {
                        view.unavailable_responses += 1;
                        reset = true;
                        fold = None;
                        continue;
                    }
                    if !matches!(stop_reason, Some(StopReason::Error | StopReason::Cancelled)) {
                        view.provider_usage = Some(tokens(usage));
                    }
                    responses.push_back(ResponseUsage {
                        model: model
                            .as_ref()
                            .map(|model| format!("{}/{}", model.provider, model.model.0))
                            .unwrap_or_default(),
                        usage: tokens(usage),
                        reset,
                        fold: fold.take(),
                    });
                    reset = false;
                    if responses.len() > 132 {
                        responses.pop_front();
                        view.omitted_responses += 1;
                    }
                }
                _ => {}
            }
        }
        view.responses = responses.into();
        Ok(view)
    }
}

fn measure_prompt(
    blocks: &[PromptFragment],
    tools: &[tiechui_model::ToolSpec],
    budget: &mut usize,
) -> RequestObservation {
    let mut view = RequestObservation::default();
    for block in blocks.iter().cloned() {
        if block.text.is_empty() {
            continue;
        }
        let message = Message {
            role: Role::System,
            content: vec![text_content(block.text.clone())],
            opaque: None,
        };
        view.system.push(ContextItem {
            label: block.label,
            category: block.category,
            source: block.source,
            tokens: estimate::messages([&message]),
            text: clip(&block.text, budget),
            ..ContextItem::default()
        });
        for (path, text) in block.references {
            let message = Message {
                role: Role::System,
                content: vec![text_content(text.clone())],
                opaque: None,
            };
            view.references.push(ContextItem {
                label: clip(&path, budget),
                category: "Instruction file".into(),
                source: "Instruction content included in System estimate".into(),
                tokens: estimate::messages([&message]),
                text: clip(&text, budget),
                ..ContextItem::default()
            });
        }
    }
    for tool in tools {
        view.tools.push(ContextItem {
            label: clip(&tool.name, budget),
            category: "Tool definitions".into(),
            source: "Current registry; declaration sent to the model".into(),
            group: "Registered tools".into(),
            tokens: estimate::input(&[], std::slice::from_ref(tool)),
            text: clip(
                &serde_json::to_string_pretty(tool).unwrap_or_default(),
                budget,
            ),
            json: true,
        });
    }
    view
}

fn restored_prompt(text: &str) -> PromptFragment {
    PromptFragment {
        label: "System prompt".into(),
        category: "System prompt".into(),
        source: "Restored request system state; source metadata unavailable".into(),
        text: text.into(),
        references: vec![],
    }
}
