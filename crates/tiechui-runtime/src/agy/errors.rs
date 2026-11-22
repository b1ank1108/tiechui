//! Antigravity (`agy acp`) error and streamed notice classification.

use serde_json::Value;

use crate::acp::TextDisposition;

pub const LOGIN_REQUIRED_MESSAGE: &str = "agy 未登录或登录已过期，请在终端运行 agy 完成登录";

const INVALID_OUTPUT_RETRY_NOTICE: &str = "⚠ Model produced invalid output.";

const AUTH_PATTERNS: &[&str] = &[
    "unauthenticated",
    "code 401",
    "\"code\":401",
    "\"code\": 401",
    "401 unauthorized",
    "http 401",
    "invalid authentication credentials",
    "expected oauth 2 access token",
    "oauth token",
    "not logged in",
    "login required",
    "please run agy",
    "api key not valid",
    "missing credentials",
    "failed to create gemini client",
];

pub fn classify_text_chunk(thinking: bool, text: &str) -> TextDisposition {
    if thinking {
        return TextDisposition::Keep;
    }
    let trimmed = text.trim();
    if trimmed == INVALID_OUTPUT_RETRY_NOTICE {
        return TextDisposition::Ignore;
    }
    if is_error_notice_chunk(trimmed) {
        let lower = trimmed.to_ascii_lowercase();
        let message = if is_auth_text(&lower) {
            LOGIN_REQUIRED_MESSAGE.to_owned()
        } else {
            clean_chunk_notice(trimmed)
        };
        return TextDisposition::FailTurn(message);
    }
    TextDisposition::Keep
}

fn is_error_notice_chunk(trimmed: &str) -> bool {
    if let Some(rest) = trimmed.strip_prefix('⚠') {
        let first_line = rest.lines().next().unwrap_or_default().trim();
        return first_line.contains("(code ")
            || first_line.starts_with("Internal error:")
            || is_auth_text(&first_line.to_ascii_lowercase());
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("api key not valid") || lower.starts_with("failed to create gemini client")
}

fn clean_chunk_notice(trimmed: &str) -> String {
    let without_icon = trimmed.trim_start_matches('⚠').trim_start();
    strip_stack_trace(without_icon)
}

pub fn is_auth_error(code: i64, message: &str, data: Option<&Value>) -> bool {
    if code == 401 {
        return true;
    }
    let mut combined = message.to_ascii_lowercase();
    if let Some(extra) = data {
        combined.push(' ');
        combined.push_str(&extra.to_string().to_ascii_lowercase());
    }
    is_auth_text(&combined)
}

pub fn format_error(code: i64, message: &str, data: Option<&Value>) -> String {
    if is_auth_error(code, message, data) {
        return LOGIN_REQUIRED_MESSAGE.to_owned();
    }
    let stripped = strip_stack_trace(message);
    if let Some(unpacked) = unpack_cloud_code_message(&stripped) {
        return strip_stack_trace(&unpacked);
    }
    stripped
}

fn is_auth_text(lower: &str) -> bool {
    AUTH_PATTERNS.iter().any(|needle| lower.contains(needle))
}

fn strip_stack_trace(text: &str) -> String {
    let kept: Vec<&str> = text
        .lines()
        .take_while(|line| !is_stack_frame_line(line))
        .collect();
    let joined = kept.join("\n").trim().to_owned();
    if joined.is_empty() {
        text.lines().next().unwrap_or_default().trim().to_owned()
    } else {
        joined
    }
}

fn is_stack_frame_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("at ") && (trimmed.contains('(') || trimmed.contains('/'))
}

fn unpack_cloud_code_message(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let parsed: Value = serde_json::from_str(text[start..].trim()).ok()?;
    let inner = parsed
        .pointer("/error/message")
        .or_else(|| parsed.get("message"))?
        .as_str()?
        .trim();
    if inner.is_empty() {
        return None;
    }
    if let Some(nested) = unpack_cloud_code_message(inner) {
        return Some(nested);
    }
    Some(inner.to_owned())
}
