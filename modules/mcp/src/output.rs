use std::io::Write;

use base64::Engine;
use tiechui_plugin_api::model::Content;
use tiechui_plugin_api::serde_json::{self, Value, json};
use tiechui_plugin_api::{ToolError, ToolOutput};

fn text(value: impl Into<String>) -> Content {
    Content::Text {
        text: value.into(),
        opaque: None,
    }
}

fn save(data: &[u8], suffix: &str) -> Result<String, ToolError> {
    let mut file = tempfile::Builder::new()
        .prefix("tiechui-mcp-")
        .suffix(suffix)
        .tempfile()?;
    file.write_all(data)?;
    let (_, path) = file.keep().map_err(|e| ToolError(e.to_string()))?;
    Ok(path.display().to_string())
}

fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

fn block(server: &str, block: &Value, readable: bool) -> Result<Content, ToolError> {
    Ok(match string(block, "type") {
        "text" => text(string(block, "text")),
        "image" => Content::Image {
            data: string(block, "data").into(),
            mime_type: string(block, "mimeType").into(),
        },
        "audio" => text(format!("[audio {} omitted]", string(block, "mimeType"))),
        "resource_link" => {
            let mut details = Vec::new();
            if let Some(mime) = block["mimeType"].as_str() {
                details.push(mime.to_owned());
            }
            if let Some(size) = block["size"].as_u64() {
                details.push(tiechui_plugin_api::tools::output::format_size(
                    size as usize,
                ));
            }
            let details = if details.is_empty() {
                String::new()
            } else {
                format!(" ({})", details.join(", "))
            };
            let description = block["description"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map_or(String::new(), |s| format!(": {s}"));
            let read = if readable {
                format!(". Read it with read_mcp_resource (server \"{server}\")")
            } else {
                String::new()
            };
            text(format!(
                "[Resource {} \"{}\"{details}{description}{}]",
                string(block, "uri"),
                block["title"].as_str().unwrap_or(string(block, "name")),
                read
            ))
        }
        "resource" => {
            let resource = &block["resource"];
            if let Some(value) = resource["text"].as_str() {
                text(value)
            } else {
                let mime = string(resource, "mimeType");
                let blob = string(resource, "blob");
                if mime.starts_with("image/") {
                    Content::Image {
                        data: blob.into(),
                        mime_type: mime.into(),
                    }
                } else {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(blob)
                        .map_err(|_| ToolError("Invalid base64 MCP resource".into()))?;
                    let mime = mime
                        .split(';')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_ascii_lowercase();
                    if mime.starts_with("text/")
                        || mime == "application/json"
                        || mime.ends_with("+json")
                        || mime.ends_with("+xml")
                    {
                        text(String::from_utf8_lossy(&bytes))
                    } else {
                        let uri = string(resource, "uri");
                        let suffix = uri
                            .rsplit_once('.')
                            .map(|(_, extension)| extension)
                            .filter(|s| {
                                !s.is_empty()
                                    && s.len() <= 8
                                    && s.chars().all(|c| c.is_ascii_alphanumeric())
                            })
                            .map_or(".bin".into(), |s| format!(".{s}"));
                        let location = match save(&bytes, &suffix) {
                            Ok(path) => format!("saved to {path}"),
                            Err(error) => format!("could not be saved: {error}"),
                        };
                        text(format!(
                            "[Binary resource {uri} ({}, {}) {location}]",
                            if mime.is_empty() {
                                "unknown type"
                            } else {
                                &mime
                            },
                            tiechui_plugin_api::tools::output::format_size(bytes.len())
                        ))
                    }
                }
            }
        }
        kind => text(format!("[unsupported MCP content {kind}]")),
    })
}

pub(super) fn limit(output: &mut ToolOutput) {
    let combined = output
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if combined.len() <= 20 * 1024 {
        return;
    }
    let mut head = 10 * 1024;
    let mut tail = combined.len() - 10 * 1024;
    while !combined.is_char_boundary(head) {
        head -= 1;
    }
    while !combined.is_char_boundary(tail) {
        tail += 1;
    }
    let removed = combined[head..tail].chars().count();
    let location = match save(combined.as_bytes(), ".txt") {
        Ok(path) => {
            if let Some(details) = output.details.as_mut() {
                details["fullOutputPath"] = json!(path);
            }
            format!("[Full output: {path} (read it with offset/limit)]")
        }
        Err(error) => format!("[Could not save the full output: {error}]"),
    };
    output
        .content
        .retain(|c| matches!(c, Content::Image { .. }));
    output.content.insert(0, text(format!(
        "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{}…{removed} chars truncated…{}\n\n{location}",
        combined.len().div_ceil(4), combined.lines().count(), &combined[..head], &combined[tail..]
    )));
}

pub(super) fn convert(
    server: &str,
    tool: &str,
    mut result: Value,
    readable: bool,
) -> Result<ToolOutput, ToolError> {
    if let Some(object) = result.as_object_mut() {
        object.remove("_meta");
    }
    let mut content = result["content"]
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .map(|b| block(server, b, readable))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    if content.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        content.push(text(
            serde_json::to_string_pretty(structured).map_err(|e| ToolError(e.to_string()))?,
        ));
    }
    let is_error = result["isError"].as_bool().unwrap_or(false);
    if is_error
        && !content
            .iter()
            .any(|c| matches!(c, Content::Text { text, .. } if !text.is_empty()))
    {
        content.push(text(format!("MCP tool {server}/{tool} returned an error")));
    }
    let mut output = ToolOutput {
        content,
        is_error,
        details: Some(json!({"server":server,"tool":tool})),
        structured_content: Some(result),
    };
    limit(&mut output);
    Ok(output)
}

pub(super) fn resource(
    server: &str,
    uri: &str,
    mut contents: Vec<Value>,
) -> Result<ToolOutput, ToolError> {
    for resource in &mut contents {
        if let Some(object) = resource.as_object_mut() {
            object.remove("_meta");
        }
    }
    let mut content = vec![];
    for resource in &contents {
        if contents.len() > 1 {
            content.push(text(format!("{}:", string(resource, "uri"))));
        }
        content.push(block(
            server,
            &json!({"type":"resource","resource":resource}),
            true,
        )?);
    }
    if content.is_empty() {
        content.push(text(format!("Resource {uri} is empty.")));
    }
    let mut output = ToolOutput {
        content,
        structured_content: Some(json!({"server":server,"uri":uri,"contents":contents})),
        details: Some(json!({"server":server,"tool":"read_mcp_resource"})),
        ..ToolOutput::default()
    };
    limit(&mut output);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_preserves_structured_envelope_and_error_without_meta() {
        let output = convert("s", "t", json!({"content":[],"structuredContent":{"answer":42},"isError":true,"_meta":{"secret":true}}), false).unwrap();
        assert!(output.is_error);
        assert_eq!(
            output.structured_content.as_ref().unwrap()["structuredContent"]["answer"],
            42
        );
        assert!(
            output
                .structured_content
                .as_ref()
                .unwrap()
                .get("_meta")
                .is_none()
        );
        assert!(matches!(&output.content[0], Content::Text { text, .. } if text.contains("42")));
        let empty = convert("s", "t", json!({"content":[],"isError":true}), false).unwrap();
        assert_eq!(empty.content, vec![text("MCP tool s/t returned an error")]);
    }

    #[test]
    fn maps_text_images_resources_and_audio() {
        let output = convert("s", "t", json!({"content":[
            {"type":"text","text":"hello"}, {"type":"image","data":"aA==","mimeType":"image/png"},
            {"type":"resource","resource":{"uri":"x","text":"embedded"}},
            {"type":"resource","resource":{"uri":"x","blob":"aA==","mimeType":"image/png"}},
            {"type":"resource","resource":{"uri":"x","blob":"aA==","mimeType":"text/plain"}},
            {"type":"audio","data":"","mimeType":"audio/wav"},
            {"type":"resource_link","uri":"x","name":"link"}
        ]}), true).unwrap();
        assert_eq!(output.content[0], text("hello"));
        assert!(matches!(&output.content[1], Content::Image { .. }));
        assert_eq!(output.content[2], text("embedded"));
        assert!(matches!(&output.content[3], Content::Image { .. }));
        assert_eq!(output.content[4], text("h"));
        assert!(
            matches!(&output.content[6], Content::Text { text, .. } if text.contains("read_mcp_resource"))
        );
    }
}
