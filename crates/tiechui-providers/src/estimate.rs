//! Input-token heuristics used for trailing history and requests without usage.

use tiechui_model::{Content, Message, ModelRequest, ToolSpec};

// A UTF-16 character heuristic: 3.5 characters/token, 4800 characters/image.
fn chars(text: &str) -> u64 {
    text.encode_utf16().count() as u64
}

fn content_chars(content: &[Content]) -> u64 {
    content
        .iter()
        .map(|part| match part {
            Content::Text { text, .. } | Content::Thinking { text, .. } => chars(text),
            Content::Image { .. } => 4800,
            Content::ToolCall(call) => chars(&call.name) + chars(&call.arguments.to_string()),
            Content::ToolResult(result) => content_chars(&result.content),
        })
        .sum()
}

pub fn messages<'a>(messages: impl IntoIterator<Item = &'a Message>) -> u64 {
    messages
        .into_iter()
        .map(|message| (content_chars(&message.content) * 2).div_ceil(7))
        .sum()
}

pub fn input(messages: &[Message], tools: &[ToolSpec]) -> u64 {
    let mut input = self::messages(messages);
    for tool in tools {
        input += ((chars(&tool.name)
            + chars(&tool.description)
            + chars(&tool.input_schema.to_string()))
            * 2)
        .div_ceil(7);
    }
    input
}

pub(crate) fn clamp(request: &ModelRequest, window: u64, output: u64) -> u64 {
    if window == 0 {
        return output.max(1);
    }
    let input = request
        .input_tokens
        .unwrap_or_else(|| input(&request.messages, &request.tools));
    output.min(window.saturating_sub(input).saturating_sub(4096).max(1))
}
