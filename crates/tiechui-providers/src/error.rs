use std::fmt;
use std::time::{Duration, SystemTime};

use reqwest::header::HeaderMap;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Auth,
    RateLimit,
    ContextOverflow,
    Overloaded,
    TransientNetwork,
    /// An ambiguous request rejection eligible for one bounded recovery attempt.
    AmbiguousRequest,
    InvalidRequest,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub kind: ErrorKind,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl ProviderError {
    pub(crate) fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: tiechui_config::redact_proxy_secrets(&message.into()),
            retry_after: None,
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "request cancelled")
    }

    pub(crate) fn transport(error: reqwest::Error) -> Self {
        Self::new(ErrorKind::TransientNetwork, error.without_url().to_string())
    }

    pub(crate) fn stream(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::TransientNetwork, message)
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(output, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ProviderError {}

pub(crate) fn classify(
    status: Option<u16>,
    body: &Value,
    retry_after: Option<Duration>,
) -> ProviderError {
    let error = body.get("error").unwrap_or(body);
    let error_type = error.get("type").and_then(Value::as_str).unwrap_or("");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .filter(|message| !message.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| match status {
            Some(status) => format!("HTTP {status}"),
            None => "provider request failed".into(),
        });
    let kind = if terminal_limit(error, &message) {
        ErrorKind::InvalidRequest
    } else {
        match status {
            Some(401 | 403) => ErrorKind::Auth,
            Some(429) => ErrorKind::RateLimit,
            Some(413) => ErrorKind::ContextOverflow,
            Some(503 | 529) => ErrorKind::Overloaded,
            None | Some(400) if ambiguous_request(error) => ErrorKind::AmbiguousRequest,
            _ => match error_type {
                "authentication_error" | "permission_error" => ErrorKind::Auth,
                "rate_limit_error" => ErrorKind::RateLimit,
                "overloaded_error" => ErrorKind::Overloaded,
                "request_too_large"
                | "request_too_large_error"
                | "context_length_exceeded"
                | "context_window_exceeded" => ErrorKind::ContextOverflow,
                _ if matches!(
                    error.get("code").and_then(Value::as_str),
                    Some("context_length_exceeded" | "context_window_exceeded")
                ) =>
                {
                    ErrorKind::ContextOverflow
                }
                // Native overflow shares invalid_request_error, so the text exception is scoped
                // to that type and HTTP 400; see README.md.
                "invalid_request_error"
                    if status == Some(400) && {
                        let lower = message.to_ascii_lowercase();
                        lower.contains("prompt is too long") || lower.contains("prompt too long")
                    } =>
                {
                    ErrorKind::ContextOverflow
                }
                "api_error" => ErrorKind::TransientNetwork,
                _ if status
                    .is_some_and(|status| status == 408 || status == 409 || status >= 500) =>
                {
                    ErrorKind::TransientNetwork
                }
                _ => ErrorKind::InvalidRequest,
            },
        }
    };
    ProviderError {
        kind,
        message: tiechui_config::redact_proxy_secrets(&message)
            .chars()
            .take(2048)
            .collect(),
        retry_after,
    }
}

fn ambiguous_request(error: &Value) -> bool {
    if error["type"] != "upstream_error"
        || error["code"] != "upstream_400"
        || !error
            .get("param")
            .is_some_and(|param| param.is_null() || param.as_str() == Some(""))
        || serde_json::to_vec(error).map_or(true, |bytes| bytes.len() > 16_384)
    {
        return false;
    }
    let Some(message) = error["message"]
        .as_str()
        .filter(|message| message.len() <= 128)
    else {
        return false;
    };
    let message = message.strip_prefix("400").map_or(Some(message), |tail| {
        tail.trim_start()
            .strip_prefix(['·', ':'])
            .map(str::trim_start)
    });
    message.is_some_and(|message| {
        message
            .strip_suffix('.')
            .unwrap_or(message)
            .eq_ignore_ascii_case("Request contains an invalid argument")
    })
}

fn terminal_limit(error: &Value, message: &str) -> bool {
    let text = format!(
        "{} {} {message}",
        error.get("type").and_then(Value::as_str).unwrap_or(""),
        error.get("code").and_then(Value::as_str).unwrap_or("")
    )
    .to_ascii_lowercase();
    [
        "gousagelimiterror",
        "freeusagelimiterror",
        "monthly usage limit reached",
        "available balance",
        "insufficient_quota",
        "out of budget",
        "quota exceeded",
        "billing",
        "subscription_sharing_usage_limit_exceeded",
        "subscription_sharing_workplace_individual_usage_limit_exceeded",
    ]
    .iter()
    .any(|pattern| text.contains(pattern))
}

pub(crate) fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    fn seconds(value: &str) -> Option<Duration> {
        Duration::try_from_secs_f64(value.parse().ok()?).ok()
    }
    if let Some(value) = headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        && let Some(delay) = seconds(value).map(|delay| delay.div_f64(1000.0))
    {
        return Some(delay);
    }
    let value = headers.get("retry-after")?.to_str().ok()?;
    seconds(value).or_else(|| {
        httpdate::parse_http_date(value)
            .ok()
            .map(|date| date.duration_since(now).unwrap_or_default())
    })
}
