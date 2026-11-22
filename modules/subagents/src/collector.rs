use std::{collections::BTreeMap, path::Path, time::Instant};
use tiechui_plugin_api::{model::Usage, protocol::*, serde_json, *};

use crate::output;

pub(crate) struct Collector {
    pub published: Instant,
    pub partial: bool,
    pub output: output::Output,
    pub thinking: output::Output,
    kinds: BTreeMap<String, bool>,
    blocks: Vec<(String, String)>,
    thinking_blocks: Vec<(String, String)>,
    pub status: Option<TurnStatus>,
    pub error: Option<String>,
    pub usage: TokenUsage,
    pub turns: u64,
    pub model: String,
    next_response: bool,
}

impl Collector {
    pub fn new(directory: &Path) -> Self {
        Self {
            published: Instant::now(),
            partial: false,
            output: output::Output::new(directory),
            thinking: output::Output::new(directory),
            kinds: BTreeMap::new(),
            blocks: vec![],
            thinking_blocks: vec![],
            status: None,
            error: None,
            usage: TokenUsage::default(),
            turns: 0,
            model: String::new(),
            next_response: false,
        }
    }

    fn start_assistant(&mut self, id: &str, text: &str) -> Result<(), ToolError> {
        if self.next_response {
            self.output.clear();
            self.blocks.clear();
            self.kinds.retain(|_, thinking| *thinking);
            self.next_response = false;
        } else if !self.output.text.is_empty() {
            self.output.push("\n")?;
        }
        self.kinds.insert(id.to_owned(), false);
        self.blocks.push((id.to_owned(), text.to_owned()));
        self.output.push(text)
    }

    fn push_delta(&mut self, item_id: &str, delta: &str) -> Result<(), ToolError> {
        let Some(&thinking) = self.kinds.get(item_id) else {
            return Ok(());
        };
        let (blocks, out) = if thinking {
            (&mut self.thinking_blocks, &mut self.thinking)
        } else {
            (&mut self.blocks, &mut self.output)
        };
        if let Some((_, text)) = blocks.iter_mut().find(|(id, _)| id == item_id) {
            text.push_str(delta);
        }
        out.push(delta)
    }

    fn complete_text(&mut self, id: &str, text: &str) -> Result<(), ToolError> {
        let thinking = self.kinds.get(id).copied().unwrap_or(false);
        let (blocks, out) = if thinking {
            (&mut self.thinking_blocks, &mut self.thinking)
        } else {
            (&mut self.blocks, &mut self.output)
        };
        let Some((_, previous)) = blocks.iter_mut().find(|(key, _)| key == id) else {
            return Ok(());
        };
        if previous == text {
            return Ok(());
        }
        *previous = text.to_owned();
        out.clear();
        for (i, (_, block_text)) in blocks.iter().enumerate() {
            if i > 0 {
                out.push("\n")?;
            }
            out.push(block_text)?;
        }
        Ok(())
    }

    fn complete_item(&mut self, item: &Item) -> Result<bool, ToolError> {
        match item {
            Item::Known(
                KnownItem::AssistantMessage { id, text } | KnownItem::Thinking { id, text },
            ) => {
                self.complete_text(&id.0, text)?;
            }
            Item::Known(KnownItem::ToolResult {
                output,
                is_error: true,
                ..
            }) if output["details"]["panic"] == true => {
                self.error = Some("Child tool panicked".into());
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn item_event(&mut self, event: &EventKind) -> Result<bool, ToolError> {
        match event {
            EventKind::ItemStarted {
                item: Item::Known(KnownItem::AssistantMessage { id, text }),
                ..
            } => self.start_assistant(&id.0, text)?,
            EventKind::ItemStarted {
                item: Item::Known(KnownItem::Thinking { id, text }),
                ..
            } => {
                self.kinds.insert(id.0.clone(), true);
                self.thinking_blocks.push((id.0.clone(), text.clone()));
                self.thinking.push(text)?;
            }
            EventKind::ItemDelta { item_id, delta, .. } => self.push_delta(&item_id.0, delta)?,
            EventKind::ItemCompleted { item, .. } => return self.complete_item(item),
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn check_limits(&self) -> Result<(), ToolError> {
        let total_bytes: usize = self
            .blocks
            .iter()
            .chain(&self.thinking_blocks)
            .map(|(_, text)| text.len())
            .sum();
        if self.blocks.len() + self.thinking_blocks.len() > 256 || total_bytes > 8 * 1024 * 1024 {
            return Err(ToolError(
                "Child live messages exceeded the 8 MiB / 256 block memory limit".into(),
            ));
        }
        Ok(())
    }

    pub fn event(&mut self, event: &EventKind) -> Result<(), ToolError> {
        if !self.item_event(event)? {
            match event {
                EventKind::Extension { name, data } if name == ACTOR_USAGE_EVENT => {
                    self.partial |= serde_json::from_value::<Option<Usage>>(data.clone())
                        .ok()
                        .flatten()
                        .is_none_or(|usage| usage.partial);
                }
                EventKind::TokenUsageUpdated { total, .. } => {
                    self.usage = *total;
                    self.turns += 1;
                    self.next_response = true;
                }
                EventKind::SessionUpdated { model, .. } => {
                    self.model = format!("{}/{}", model.provider, model.id);
                }
                EventKind::TurnCompleted { turn } => self.status = Some(turn.status),
                EventKind::Error { message } => self.error = Some(message.clone()),
                _ => {}
            }
        }
        self.check_limits()
    }

    pub fn update(&self, task: &mut AgentTask) {
        task.output = self.output.text.clone();
        task.thinking = self.thinking.text.clone();
        task.output_path = self.output.path.as_ref().map(|p| p.display().to_string());
        task.thinking_path = self.thinking.path.as_ref().map(|p| p.display().to_string());
        task.usage = self.usage;
        task.turns = self.turns;
        task.model = self.model.clone();
    }
}

pub(crate) fn read_completed_output(buffer: &mut Collector) -> Option<(String, String)> {
    if buffer.error.is_some() || buffer.status != Some(TurnStatus::Completed) {
        return None;
    }
    match buffer
        .output
        .full()
        .and_then(|text| Ok((text, buffer.thinking.full()?)))
    {
        Ok(full) => Some(full),
        Err(error) => {
            buffer.error = Some(error.0);
            None
        }
    }
}

pub(crate) fn resolve_task_state(buffer: &Collector, cancel: &CancellationToken) -> AgentState {
    if buffer.error.is_some() {
        return AgentState::Failed;
    }
    if cancel.is_cancelled() && buffer.status != Some(TurnStatus::Completed) {
        return AgentState::Cancelled;
    }
    match buffer.status {
        Some(TurnStatus::Completed) => AgentState::Completed,
        Some(TurnStatus::Interrupted) => AgentState::Cancelled,
        _ => AgentState::Failed,
    }
}
