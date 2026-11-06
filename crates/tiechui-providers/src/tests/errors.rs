use std::time::{Duration, SystemTime};

use reqwest::header::{HeaderMap, HeaderValue};

use super::*;
use crate::{
    ErrorKind,
    error::{classify, retry_after},
};

#[tokio::test]
async fn http_errors_use_status_and_type_without_retrying() {
    let cases = [
        (401, "invalid_request_error", ErrorKind::Auth),
        (403, "invalid_request_error", ErrorKind::Auth),
        (429, "invalid_request_error", ErrorKind::RateLimit),
        (413, "invalid_request_error", ErrorKind::ContextOverflow),
        (529, "api_error", ErrorKind::Overloaded),
        (503, "api_error", ErrorKind::Overloaded),
        (500, "api_error", ErrorKind::TransientNetwork),
        (408, "unknown", ErrorKind::TransientNetwork),
        (400, "context_length_exceeded", ErrorKind::ContextOverflow),
        (400, "invalid_request_error", ErrorKind::InvalidRequest),
    ];
    for (status, kind, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("retry-after", "2")
                    .insert_header("retry-after-ms", "1250")
                    .set_body_json(json!({"error":{"type":kind,"message":"fixture failure"}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let error = provider(&server.uri())
            .stream(request(), CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind, expected);
        assert_eq!(error.retry_after, Some(Duration::from_millis(1250)));
    }
}

#[tokio::test]
async fn http_error_messages_handle_empty_malformed_and_string_bodies() {
    for (body, message) in [
        ("", "HTTP 502"),
        ("<html>bad gateway</html>", "HTTP 502"),
        ("{}", "HTTP 502"),
        (r#"{"error":{"type":"api_error"}}"#, "HTTP 502"),
        (r#"{"error":{"message":null}}"#, "HTTP 502"),
        (r#"{"error":{"message":17}}"#, "HTTP 502"),
        (r#"{"error":{"message":" \n\t"}}"#, "HTTP 502"),
        (r#"{"error":""}"#, "HTTP 502"),
        (r#"{"error":"fixture failure"}"#, "fixture failure"),
        (r#"{"message":"top level failure"}"#, "top level failure"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(502).set_body_raw(body, "application/json"))
            .expect(1)
            .mount(&server)
            .await;
        let error = provider(&server.uri())
            .stream(request(), CancellationToken::new())
            .await
            .err()
            .unwrap();
        assert_eq!(error.message, message);
        assert_eq!(error.kind, ErrorKind::TransientNetwork);
    }
}

#[test]
fn structured_stream_errors_and_narrow_overflow_exception() {
    for (kind, expected) in [
        ("authentication_error", ErrorKind::Auth),
        ("permission_error", ErrorKind::Auth),
        ("rate_limit_error", ErrorKind::RateLimit),
        ("overloaded_error", ErrorKind::Overloaded),
        ("request_too_large", ErrorKind::ContextOverflow),
        ("request_too_large_error", ErrorKind::ContextOverflow),
        ("context_window_exceeded", ErrorKind::ContextOverflow),
        ("api_error", ErrorKind::TransientNetwork),
        ("invalid_request_error", ErrorKind::InvalidRequest),
    ] {
        assert_eq!(
            classify(
                None,
                &json!({"type":kind,"message":"prompt is too long"}),
                None
            )
            .kind,
            expected
        );
    }
    for message in ["Prompt is too long: fixture limits", "PROMPT TOO LONG"] {
        let body = json!({"error":{"type":"invalid_request_error","message":message}});
        assert_eq!(
            classify(Some(400), &body, None).kind,
            ErrorKind::ContextOverflow
        );
        assert_eq!(
            classify(Some(422), &body, None).kind,
            ErrorKind::InvalidRequest
        );
    }
    assert_eq!(
        classify(
            Some(400),
            &json!({"error":{"type":"unknown","message":"prompt is too long"}}),
            None
        )
        .kind,
        ErrorKind::InvalidRequest
    );
    assert_eq!(classify(Some(400), &json!({"error":{"type":"invalid_request_error","message":"rate limit auth overloaded timeout"}}), None).kind, ErrorKind::InvalidRequest);
}

#[test]
fn ambiguous_request_requires_a_bounded_structured_gateway_error() {
    let wrapped = json!({"type":"upstream_error", "code":"upstream_400", "param":"", "message":"400 · Request contains an invalid argument."});
    for message in [
        "Request contains an invalid argument",
        "request contains an invalid argument.",
        "400: Request contains an invalid argument.",
        "400 · Request contains an invalid argument.",
    ] {
        for param in [json!(""), Value::Null] {
            let mut error = wrapped.clone();
            error["param"] = param;
            error["message"] = json!(message);
            for status in [Some(400), None] {
                assert_eq!(
                    classify(status, &error, None).kind,
                    ErrorKind::AmbiguousRequest
                );
                assert_eq!(
                    classify(status, &json!({"error":error}), None).kind,
                    ErrorKind::AmbiguousRequest
                );
            }
        }
    }
    for (field, value) in [
        ("type", json!("invalid_request_error")),
        ("code", json!("invalid_argument")),
        ("param", json!("tools")),
        ("param", json!(" ")),
        ("param", json!(false)),
        ("message", json!("invalid argument")),
        (
            "message",
            json!("Request contains an invalid argument: tools"),
        ),
        ("message", json!("invalid schema")),
        (
            "message",
            json!("Request contains an invalid argument. signature mismatch"),
        ),
        (
            "message",
            json!("Request contains an invalid argument. insufficient_quota"),
        ),
        ("extra", json!("x".repeat(16_384))),
    ] {
        let mut error = wrapped.clone();
        error[field] = value;
        assert_eq!(
            classify(Some(400), &error, None).kind,
            ErrorKind::InvalidRequest,
            "{field}"
        );
    }
    let mut missing = wrapped.clone();
    missing.as_object_mut().unwrap().remove("param");
    assert_eq!(
        classify(Some(400), &missing, None).kind,
        ErrorKind::InvalidRequest
    );
    for body in [
        json!({"error":wrapped.to_string()}),
        json!({"message":wrapped.to_string()}),
        Value::Null,
    ] {
        assert_eq!(
            classify(Some(400), &body, None).kind,
            ErrorKind::InvalidRequest
        );
    }
    assert_eq!(classify(Some(401), &wrapped, None).kind, ErrorKind::Auth);
    assert_ne!(
        classify(Some(422), &wrapped, None).kind,
        ErrorKind::AmbiguousRequest
    );
}

#[test]
fn retry_after_seconds_milliseconds_dates_and_invalid_headers() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    for (milliseconds, seconds, expected) in [
        (Some("1250"), Some("9"), Some(Duration::from_millis(1250))),
        (None, Some("1.5"), Some(Duration::from_millis(1500))),
        (Some("invalid"), Some("2"), Some(Duration::from_secs(2))),
        (Some("-1"), Some("NaN"), None),
        (None, Some("inf"), None),
        (None, None, None),
    ] {
        let mut headers = HeaderMap::new();
        if let Some(value) = milliseconds {
            headers.insert("retry-after-ms", HeaderValue::from_str(value).unwrap());
        }
        if let Some(value) = seconds {
            headers.insert("retry-after", HeaderValue::from_str(value).unwrap());
        }
        assert_eq!(retry_after(&headers, now), expected);
    }
    for (date, expected) in [
        (now + Duration::from_secs(10), Duration::from_secs(10)),
        (now - Duration::from_secs(10), Duration::ZERO),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            HeaderValue::from_str(&httpdate::fmt_http_date(date)).unwrap(),
        );
        assert_eq!(retry_after(&headers, now), Some(expected));
    }
}
