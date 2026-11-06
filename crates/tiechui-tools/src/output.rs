//! Shared display truncation and bounded streaming accumulation.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

use serde::Serialize;

pub const DEFAULT_MAX_LINES: usize = 2000;
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;
pub const STRUCTURED_MAX_BYTES: usize = 1024 * 1024;
pub const LOG_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum Policy {
    Head,
    Tail,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Truncation {
    pub content: String,
    pub truncated: bool,
    pub truncated_by: Option<&'static str>,
    pub total_lines: usize,
    pub total_bytes: usize,
    pub output_lines: usize,
    pub output_bytes: usize,
    pub last_line_partial: bool,
    pub first_line_exceeds_limit: bool,
    pub max_lines: usize,
    pub max_bytes: usize,
}

pub fn truncate(text: &str, policy: Policy, max_lines: usize, max_bytes: usize) -> Truncation {
    let lines: Vec<_> = text.split_terminator('\n').collect();
    let mut result = Truncation {
        content: text.into(),
        truncated: false,
        truncated_by: None,
        total_lines: lines.len(),
        total_bytes: text.len(),
        output_lines: lines.len(),
        output_bytes: text.len(),
        last_line_partial: false,
        first_line_exceeds_limit: false,
        max_lines,
        max_bytes,
    };
    if lines.len() <= max_lines && text.len() <= max_bytes {
        return result;
    }
    result.truncated = true;
    result.truncated_by = Some("lines");
    if matches!(policy, Policy::Head) && lines.first().is_some_and(|line| line.len() > max_bytes) {
        result.content.clear();
        result.output_lines = 0;
        result.output_bytes = 0;
        result.truncated_by = Some("bytes");
        result.first_line_exceeds_limit = true;
        return result;
    }
    let mut selected = Vec::new();
    let mut bytes = 0;
    let ordered: Box<dyn Iterator<Item = &&str>> = match policy {
        Policy::Head => Box::new(lines.iter()),
        Policy::Tail => Box::new(lines.iter().rev()),
    };
    for &line in ordered.take(max_lines) {
        let size = line.len() + usize::from(!selected.is_empty());
        if bytes + size > max_bytes {
            result.truncated_by = Some("bytes");
            if selected.is_empty() {
                match policy {
                    Policy::Head => result.first_line_exceeds_limit = true,
                    Policy::Tail => {
                        let mut start = line.len().saturating_sub(max_bytes);
                        while !line.is_char_boundary(start) {
                            start += 1;
                        }
                        selected.push(&line[start..]);
                        result.last_line_partial = true;
                    }
                }
            }
            break;
        }
        selected.push(line);
        bytes += size;
    }
    // The line limit takes precedence, even for a single partial tail line.
    if selected.len() >= max_lines {
        result.truncated_by = Some("lines");
    }
    if matches!(policy, Policy::Tail) {
        selected.reverse();
    }
    result.content = selected.join("\n");
    result.output_lines = selected.len();
    result.output_bytes = result.content.len();
    result
}

pub fn format_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else {
        let (unit, divisor) = if bytes < 1024 * 1024 {
            ("KB", 1024_u128)
        } else {
            ("MB", 1024_u128 * 1024)
        };
        // JavaScript toFixed rounds exact positive ties up, unlike Rust's formatting.
        let tenths = (bytes as u128 * 10 + divisor / 2) / divisor;
        format!("{}.{}{unit}", tenths / 10, tenths % 10)
    }
}

/// Bounded head collection with complete decoded statistics, shared by file tools.
pub struct TextHead {
    prefix: Vec<u16>,
    bytes: usize,
    newlines: usize,
    ends_in_newline: bool,
    max_lines: usize,
}

impl TextHead {
    pub fn new(max_lines: usize) -> Self {
        Self {
            prefix: Vec::new(),
            bytes: 0,
            newlines: 0,
            ends_in_newline: false,
            max_lines,
        }
    }

    pub fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.bytes += text.len();
        self.newlines += text.bytes().filter(|&byte| byte == b'\n').count();
        self.ends_in_newline = text.ends_with('\n');
        let available = (DEFAULT_MAX_BYTES + 1).saturating_sub(self.prefix.len());
        self.prefix.extend(text.encode_utf16().take(available));
    }

    pub fn finish(&self) -> Truncation {
        let mut result = truncate(
            &String::from_utf16_lossy(&self.prefix),
            Policy::Head,
            self.max_lines,
            DEFAULT_MAX_BYTES,
        );
        result.total_bytes = self.bytes;
        result.total_lines = if self.bytes == 0 {
            0
        } else {
            self.newlines + usize::from(!self.ends_in_newline)
        };
        result
    }
}

pub struct Snapshot {
    pub truncation: Truncation,
    pub full_output_path: Option<PathBuf>,
    pub last_line_bytes: usize,
    /// False once the log reached its size limit; the file then holds only the start.
    pub log_complete: bool,
}

impl Snapshot {
    pub fn text(&self, empty: &str) -> String {
        let t = &self.truncation;
        let mut text = if t.content.is_empty() {
            empty.into()
        } else {
            t.content.clone()
        };
        if t.truncated
            && let Some(path) = &self.full_output_path
        {
            let start = t.total_lines - t.output_lines + 1;
            let end = t.total_lines;
            let notice = if t.last_line_partial {
                format!(
                    "Showing last {} of line {end} (line is {})",
                    format_size(t.output_bytes),
                    format_size(self.last_line_bytes)
                )
            } else if t.truncated_by == Some("lines") {
                format!("Showing lines {start}-{end} of {}", t.total_lines)
            } else {
                format!(
                    "Showing lines {start}-{end} of {} ({} limit)",
                    t.total_lines,
                    format_size(t.max_bytes)
                )
            };
            let log = if self.log_complete {
                "Full output".into()
            } else {
                format!("Output log (first {})", format_size(LOG_LIMIT))
            };
            text.push_str(&format!("\n\n[{notice}. {log}: {}]", path.display()));
        }
        text
    }

    pub fn details(&self) -> Option<serde_json::Value> {
        self.truncation.truncated.then(|| {
            serde_json::json!({
                "truncation": self.truncation,
                "fullOutputPath": self.full_output_path,
            })
        })
    }
}

pub struct OutputAccumulator {
    raw: Vec<u8>,
    pending: Vec<u8>,
    tail: String,
    tail_at_boundary: bool,
    decoded_started: bool,
    total_bytes: usize,
    completed_lines: usize,
    last_line_bytes: usize,
    spill: Option<(File, PathBuf)>,
    logged: usize,
    log_limit: Option<usize>,
    finished: bool,
}

impl Default for OutputAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputAccumulator {
    pub fn new() -> Self {
        Self {
            raw: Vec::new(),
            pending: Vec::new(),
            tail: String::new(),
            tail_at_boundary: true,
            decoded_started: false,
            total_bytes: 0,
            completed_lines: 0,
            last_line_bytes: 0,
            spill: None,
            logged: 0,
            log_limit: None,
            finished: false,
        }
    }

    /// Caps the spill file at [`LOG_LIMIT`] for output of unbounded duration.
    pub fn bounded() -> Self {
        Self {
            log_limit: Some(LOG_LIMIT),
            ..Self::new()
        }
    }

    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Decoded text still held in memory and its offset in the decoded stream.
    pub fn retained(&self) -> (usize, &str) {
        (self.total_bytes - self.tail.len(), &self.tail)
    }

    fn log_complete(&self) -> bool {
        self.log_limit.is_none_or(|limit| self.logged <= limit)
    }

    fn total_lines(&self) -> usize {
        self.completed_lines + usize::from(self.last_line_bytes > 0)
    }

    pub fn append(&mut self, data: &[u8]) -> io::Result<()> {
        if self.finished {
            return Err(io::Error::other(
                "Cannot append to a finished output accumulator",
            ));
        }
        self.decode(data, false);
        if self.spill.is_none()
            && (self.raw.len() + data.len() > DEFAULT_MAX_BYTES || self.is_truncated())
        {
            self.spill()?;
        }
        let room = self
            .log_limit
            .map_or(data.len(), |limit| limit.saturating_sub(self.logged));
        match &mut self.spill {
            Some((file, _)) => file.write_all(&data[..room.min(data.len())])?,
            None => self.raw.extend_from_slice(data),
        }
        self.logged += data.len();
        Ok(())
    }

    fn decode(&mut self, data: &[u8], finish: bool) {
        self.pending.extend_from_slice(data);
        let mut text = String::new();
        let mut consumed = 0;
        while consumed < self.pending.len() {
            match std::str::from_utf8(&self.pending[consumed..]) {
                Ok(valid) => {
                    text.push_str(valid);
                    consumed = self.pending.len();
                }
                Err(error) => {
                    let valid_end = consumed + error.valid_up_to();
                    // valid_up_to always delimits valid UTF-8.
                    if let Ok(valid) = std::str::from_utf8(&self.pending[consumed..valid_end]) {
                        text.push_str(valid);
                    }
                    consumed = valid_end;
                    match error.error_len() {
                        Some(length) => {
                            text.push('\u{fffd}');
                            consumed += length;
                        }
                        None if finish => {
                            text.push('\u{fffd}');
                            consumed = self.pending.len();
                        }
                        None => break,
                    }
                }
            }
        }
        self.pending.drain(..consumed);
        if !text.is_empty() && !self.decoded_started {
            self.decoded_started = true;
            if text.starts_with('\u{feff}') {
                text.remove(0);
            }
        }
        self.total_bytes += text.len();
        self.completed_lines += text.bytes().filter(|&b| b == b'\n').count();
        self.last_line_bytes = match text.rfind('\n') {
            Some(at) => text.len() - at - 1,
            None => self.last_line_bytes + text.len(),
        };
        self.tail.push_str(&text);
        if self.tail.len() > DEFAULT_MAX_BYTES * 4 {
            let mut start = self.tail.len() - DEFAULT_MAX_BYTES * 2;
            while !self.tail.is_char_boundary(start) {
                start += 1;
            }
            self.tail_at_boundary = self.tail.as_bytes()[start - 1] == b'\n';
            self.tail.drain(..start);
        }
    }

    fn is_truncated(&self) -> bool {
        self.total_bytes > DEFAULT_MAX_BYTES || self.total_lines() > DEFAULT_MAX_LINES
    }

    fn spill(&mut self) -> io::Result<()> {
        if self.spill.is_some() {
            return Ok(());
        }
        let temp = tempfile::Builder::new()
            .prefix("tc-bash-")
            .suffix(".log")
            .tempfile()?;
        let (mut file, path) = temp.keep().map_err(|error| error.error)?;
        file.write_all(&self.raw)?;
        self.raw.clear();
        self.spill = Some((file, path));
        Ok(())
    }

    pub fn finish(&mut self) -> io::Result<()> {
        if !self.finished {
            self.decode(&[], true);
            self.finished = true;
            if self.is_truncated() {
                self.spill()?;
            }
        }
        Ok(())
    }

    pub fn snapshot(&self) -> Snapshot {
        let text = if self.tail_at_boundary {
            &self.tail
        } else {
            self.tail
                .find('\n')
                .map_or(self.tail.as_str(), |at| &self.tail[at + 1..])
        };
        let mut truncation = truncate(text, Policy::Tail, DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES);
        truncation.truncated = self.is_truncated();
        truncation.truncated_by = if truncation.truncated {
            truncation
                .truncated_by
                .or(Some(if self.total_bytes > DEFAULT_MAX_BYTES {
                    "bytes"
                } else {
                    "lines"
                }))
        } else {
            None
        };
        truncation.total_lines = self.total_lines();
        truncation.total_bytes = self.total_bytes;
        Snapshot {
            truncation,
            full_output_path: self.spill.as_ref().map(|(_, path)| path.clone()),
            last_line_bytes: self.last_line_bytes,
            log_complete: self.log_complete(),
        }
    }

    pub fn full_output(&mut self, max_bytes: usize) -> io::Result<(String, bool)> {
        if !self.log_complete() {
            let (_, text) = self.retained();
            let mut start = text.len().saturating_sub(max_bytes);
            while !text.is_char_boundary(start) {
                start += 1;
            }
            return Ok((text[start..].to_owned(), true));
        }
        let Some((file, _)) = &mut self.spill else {
            return Ok((decode_full(&self.raw), false));
        };
        let size = file.metadata()?.len() as usize;
        file.seek(SeekFrom::Start(0))?;
        if size <= max_bytes {
            let mut bytes = Vec::with_capacity(size);
            file.read_to_end(&mut bytes)?;
            return Ok((decode_full(&bytes), false));
        }
        let head_bytes = max_bytes / 2;
        let tail_bytes = max_bytes - head_bytes;
        let mut head = vec![0; head_bytes];
        let mut tail = vec![0; tail_bytes];
        file.read_exact(&mut head)?;
        file.seek(SeekFrom::Start((size - tail_bytes) as u64))?;
        file.read_exact(&mut tail)?;
        let mut head_end = head.len();
        // Streaming TextDecoder holds an incomplete final code point back.
        if let Err(error) = std::str::from_utf8(&head) {
            let mut offset = error.valid_up_to();
            while offset < head.len() {
                match std::str::from_utf8(&head[offset..]) {
                    Ok(_) => break,
                    Err(error) => {
                        offset += error.valid_up_to();
                        match error.error_len() {
                            Some(length) => offset += length,
                            None => {
                                head_end = offset;
                                break;
                            }
                        }
                    }
                }
            }
        }
        let tail_start = tail
            .iter()
            .position(|byte| byte & 0xc0 != 0x80)
            .unwrap_or(tail.len());
        Ok((
            format!(
                "{}\n\n[... {} bytes omitted ...]\n\n{}",
                decode_full(&head[..head_end]),
                size - max_bytes,
                decode_full(&tail[tail_start..])
            ),
            true,
        ))
    }
}

fn decode_full(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_string()
}
