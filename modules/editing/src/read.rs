use std::fs::{File, OpenOptions};
use std::io::{Read, Take};
use std::path::Path;

use serde_json::{Value, json};
use tiechui_plugin_api::model::ToolCall;
use tiechui_plugin_api::tools::{
    ToolContext, ToolError, ToolOutput,
    output::{DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TextHead, format_size},
};
use tokio_util::sync::CancellationToken;

use crate::{EditingState, check, mutation::open_nofollow, read_path, state::version, string};

const ZERO_LEN_STREAM_CAP: u64 = 8 * 1024 * 1024;

pub(crate) fn open_regular(path: &Path) -> Result<Take<File>, ToolError> {
    let target = std::fs::canonicalize(path)?;
    let reject = || {
        ToolError(format!(
            "Only regular files can be read: {}",
            path.display()
        ))
    };
    if !std::fs::symlink_metadata(&target)?.is_file() {
        return Err(reject());
    }
    let file = open_nofollow(OpenOptions::new().read(true)).open(&target)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(reject());
    }
    let cap = if meta.len() == 0 {
        ZERO_LEN_STREAM_CAP
    } else {
        u64::MAX
    };
    Ok(file.take(cap))
}

fn read_bytes(path: &Path, cancel: &CancellationToken) -> Result<Vec<u8>, ToolError> {
    let mut file = open_regular(path)?;
    let mut bytes = Vec::new();
    let mut chunk = [0; 65536];
    loop {
        check(cancel)?;
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    check(cancel)?;
    Ok(bytes)
}

// Keep only incomplete UTF-8 between reads, matching Node's replacement decoder.
pub(crate) fn scan(
    path: &Path,
    cancel: &CancellationToken,
    mut visit: impl FnMut(&str),
) -> Result<(), ToolError> {
    let mut file = open_regular(path)?;
    let mut chunk = [0; 65536];
    let mut pending = Vec::new();
    loop {
        check(cancel)?;
        let n = file.read(&mut chunk)?;
        pending.extend_from_slice(&chunk[..n]);
        let mut at = 0;
        while at < pending.len() {
            match std::str::from_utf8(&pending[at..]) {
                Ok(text) => {
                    visit(text);
                    at = pending.len();
                }
                Err(error) => {
                    let end = at + error.valid_up_to();
                    if let Ok(text) = std::str::from_utf8(&pending[at..end]) {
                        visit(text);
                    }
                    at = end;
                    match error.error_len() {
                        Some(n) => {
                            visit("�");
                            at += n;
                        }
                        None if n == 0 => {
                            visit("�");
                            at = pending.len();
                        }
                        None => break,
                    }
                }
            }
        }
        pending.drain(..at);
        if n == 0 {
            break;
        }
    }
    check(cancel)
}

pub(crate) fn run(
    call: ToolCall,
    ctx: ToolContext,
    state: &EditingState,
    auto_resize_images: bool,
) -> Result<ToolOutput, ToolError> {
    let input = call.arguments;
    let display = string(&input, "path")?;
    let path = read_path(display, &ctx.cwd)?;
    let window = !input["column"].is_null() || !input["maxChars"].is_null();
    let offset = input["offset"].as_f64().unwrap_or(1.0);
    let column = input["column"].as_f64().unwrap_or(1.0);
    let max_chars = input["maxChars"].as_f64().unwrap_or(8192.0);
    if window
        && (![offset, column, max_chars]
            .iter()
            .all(|v| v.fract() == 0.0 && *v >= 1.0 && *v <= 9_007_199_254_740_991.0)
            || max_chars > 8192.0
            || (!input["limit"].is_null() && input["limit"] != 1))
    {
        return Err(ToolError("Character windows require a positive offset/column, maxChars between 1 and 8192, and limit omitted or 1.".into()));
    }
    let mut prefix = [0; 4100];
    let n = open_regular(&path)?.read(&mut prefix)?;
    if let Some(mime) = crate::image::mime(&prefix[..n]) {
        if window {
            return Err(ToolError(
                "Character windows apply only to text files.".into(),
            ));
        }
        let bytes = read_bytes(&path, &ctx.cancel)?;
        return crate::image::read(bytes, mime, &ctx.cancel, auto_resize_images);
    }
    let generation = state.generation();
    let before = version(&path)?;
    let start = if offset == 0.0 {
        0.0
    } else {
        (offset - 1.0).max(0.0)
    };
    let end = input["limit"]
        .as_f64()
        .map(|limit| start + limit)
        .unwrap_or(f64::INFINITY);
    let mut line = 0;
    let mut head = TextHead::new(DEFAULT_MAX_LINES);
    // Fractional or negative inputs truncate toward zero and count negatives from the end.
    let legacy = offset.fract() != 0.0
        || offset < 0.0
        || input["limit"]
            .as_f64()
            .is_some_and(|v| v.fract() != 0.0 || v < 0.0);
    let mut legacy_text = String::new();
    if !window {
        scan(&path, &ctx.cancel, |chunk| {
            if legacy {
                legacy_text.push_str(chunk);
                return;
            }
            for (i, part) in chunk.split('\n').enumerate() {
                if i > 0 {
                    line += 1;
                    if line as f64 > start && (line as f64) < end {
                        head.append("\n");
                    }
                }
                if line as f64 >= start && (line as f64) < end {
                    head.append(part);
                }
            }
        })?;
    }
    if legacy {
        let all: Vec<_> = legacy_text.split('\n').collect();
        line = all.len() - 1;
        let slice_at = |index: f64| -> usize {
            if index < 0.0 {
                (all.len() as f64 + index.trunc()).max(0.0) as usize
            } else {
                (index.trunc() as usize).min(all.len())
            }
        };
        let a = slice_at(start);
        let b = slice_at(end);
        if b >= a {
            head.append(&all[a..b].join("\n"));
        }
    }
    let total = line + 1;
    if !window && start >= total as f64 {
        return Err(ToolError(format!(
            "Offset {offset} is beyond end of file ({total} lines total)"
        )));
    }
    let truncation = head.finish();
    let (text, view, full, details) = if window || (!legacy && truncation.first_line_exceeds_limit)
    {
        character_window(
            &path,
            display,
            offset.max(1.0) as usize,
            column as usize,
            max_chars as usize,
            &ctx.cancel,
        )?
    } else {
        let first = start + 1.0;
        let mut text = truncation.content.clone();
        if truncation.truncated {
            let last = first + truncation.output_lines as f64 - 1.0;
            text += &format!(
                "\n\n[Showing lines {first}-{last} of {total}{}. Use offset={} to continue.]",
                if truncation.truncated_by == Some("bytes") {
                    format!(" ({} limit)", format_size(DEFAULT_MAX_BYTES))
                } else {
                    String::new()
                },
                last + 1.0
            );
        } else if !input["limit"].is_null() && end.min(total as f64) < total as f64 {
            let next = end.min(total as f64);
            text += &format!(
                "\n\n[{} more lines in file. Use offset={} to continue.]",
                total as f64 - next,
                next + 1.0
            );
        }
        let details = truncation
            .truncated
            .then(|| json!({"truncation":truncation}));
        (
            text,
            truncation.content.clone(),
            start == 0.0 && end >= total as f64 && !truncation.truncated,
            details,
        )
    };
    let after = version(&path)?;
    if before == after {
        state.record(after, &view, generation, full, Some(call.id), true);
    }
    let mut output = ToolOutput::text(text.clone());
    output.structured_content = Some(Value::String(text));
    output.details = details;
    Ok(output)
}

fn character_window(
    path: &Path,
    display: &str,
    offset: usize,
    column: usize,
    max_chars: usize,
    cancel: &CancellationToken,
) -> Result<(String, String, bool, Option<Value>), ToolError> {
    let (mut line, mut chars, mut pending_cr) = (1, 0, false);
    let mut view = String::new();
    scan(path, cancel, |chunk| {
        for c in chunk.chars() {
            if pending_cr {
                line += 1;
                pending_cr = false;
                if c == '\n' {
                    continue;
                }
            }
            match c {
                '\r' => pending_cr = true,
                '\n' => line += 1,
                _ if line == offset => {
                    chars += 1;
                    if chars >= column && chars < column + max_chars {
                        view.push(c);
                    }
                }
                _ => {}
            }
        }
    })?;
    if pending_cr {
        line += 1;
    }
    if offset > line {
        return Err(ToolError(format!(
            "Offset {offset} is beyond end of file ({line} lines total)"
        )));
    }
    if column > chars + 1 {
        return Err(ToolError(format!(
            "Column {column} is beyond line {offset} ({chars} characters)."
        )));
    }
    let count = view.chars().count();
    let next = if column + count <= chars {
        Some(json!({"path":display, "offset":offset,"column":column+count,"maxChars":max_chars}))
    } else if offset < line {
        Some(json!({"path":display,"offset":offset+1}))
    } else {
        None
    };
    let continuation = match &next {
        Some(next) => {
            // JavaScript inserts path, offset, column, maxChars in that order.
            let mut text = format!(
                "{{\"path\":{},\"offset\":{}",
                serde_json::to_string(display).map_err(|e| ToolError(e.to_string()))?,
                next["offset"]
            );
            if next.get("column").is_some() {
                text += &format!(
                    ",\"column\":{},\"maxChars\":{}",
                    next["column"], next["maxChars"]
                );
            }
            text += "}";
            format!(" Continue with read {text}.")
        }
        None => " End of file.".into(),
    };
    let text = format!(
        "{view}\n\n[Line {offset}, Unicode characters {column}-{} of {chars}; {line} lines total.{continuation}]",
        column + count - 1
    );
    let mut metadata = json!({"line":offset,"column":column,"characters":count,"lineCharacters":chars,"totalLines":line});
    if let Some(next) = next {
        metadata["next"] = next;
    }
    Ok((text, view, false, Some(json!({"characterWindow":metadata}))))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn scan_keeps_exact_totals_with_bounded_head_and_decodes_multibyte_boundaries() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a");
        let content = "x".repeat(65535) + "😀中文\r\n";
        let content = content.repeat(128);
        File::create(&path)
            .unwrap()
            .write_all(content.as_bytes())
            .unwrap();
        let mut head = TextHead::new(DEFAULT_MAX_LINES);
        let mut chunks = 0;
        scan(&path, &CancellationToken::new(), |text| {
            chunks += 1;
            head.append(text);
        })
        .unwrap();
        assert!(chunks > 128);
        let result = head.finish();
        assert_eq!(result.total_bytes, content.len());
        assert_eq!(result.total_lines, 128);
        assert!(result.first_line_exceeds_limit);
        let cancel = CancellationToken::new();
        let error = scan(&path, &cancel, |_| cancel.cancel()).unwrap_err();
        assert_eq!(error.0, "Operation aborted");
    }

    #[cfg(unix)]
    #[test]
    fn open_regular_rejects_writerless_fifo_and_devices_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("pipe.fifo");
        let c_fifo = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) }, 0);
        let err = match open_regular(&fifo) {
            Ok(_) => panic!("expected writerless FIFO to be rejected"),
            Err(err) => err,
        };
        assert!(
            err.0.contains("Only regular files can be read"),
            "{}",
            err.0
        );
        let dev = Path::new("/dev/null");
        if dev.exists() {
            let err = match open_regular(dev) {
                Ok(_) => panic!("expected /dev/null to be rejected"),
                Err(err) => err,
            };
            assert!(
                err.0.contains("Only regular files can be read"),
                "{}",
                err.0
            );
        }
    }

    use std::io::Write;
}
