use std::collections::BTreeMap;

use tiechui_model::{Content, Message, Role, StreamEvent, ToolCall};

#[derive(Default)]
pub(crate) struct Response {
    blocks: BTreeMap<u32, Content>,
    arguments: BTreeMap<u32, String>,
}

impl Response {
    pub fn indices(&self) -> Vec<u32> {
        self.blocks.keys().copied().collect()
    }
    pub fn apply(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::TextDelta { index, delta } => {
                let block = self.blocks.entry(index).or_insert_with(|| Content::Text {
                    text: String::new(),
                    opaque: None,
                });
                if let Content::Text { text, .. } = block {
                    text.push_str(&delta);
                }
            }
            StreamEvent::ThinkingDelta { index, delta } => {
                let block = self
                    .blocks
                    .entry(index)
                    .or_insert_with(|| Content::Thinking {
                        text: String::new(),
                        redacted: false,
                        opaque: None,
                    });
                if let Content::Thinking { text, .. } = block {
                    text.push_str(&delta);
                }
            }
            StreamEvent::ToolCallStart { index, id, name } => {
                self.blocks.insert(
                    index,
                    Content::ToolCall(ToolCall {
                        id,
                        name,
                        arguments: serde_json::json!({}),
                        opaque: None,
                    }),
                );
            }
            StreamEvent::ToolCallArgumentsDelta { index, delta } => {
                self.arguments.entry(index).or_default().push_str(&delta);
            }
            StreamEvent::ContentComplete { index, content } => {
                self.arguments.remove(&index);
                self.blocks.insert(index, content);
            }
            StreamEvent::ResponseModel { .. }
            | StreamEvent::Finish { .. }
            | StreamEvent::Usage { .. } => {}
        }
    }

    pub fn message(&self) -> Message {
        let mut blocks = self.blocks.clone();
        for (index, raw) in &self.arguments {
            if let Some(Content::ToolCall(call)) = blocks.get_mut(index) {
                call.arguments = serde_json::from_str(raw)
                    .unwrap_or_else(|_| serde_json::Value::String(raw.clone()));
            }
        }
        Message {
            role: Role::Assistant,
            content: blocks.into_values().collect(),
            opaque: None,
        }
    }
}
