//! A stand-in for `codex` in the end-to-end tests. `--version` prints a version; `app-server`
//! replays the ndjson script named by `FAKE_CODEX_SCRIPT` over stdio. Every inbound line is
//! appended to the file named by `FAKE_CODEX_LOG`.
//!
//! Script steps, one JSON object per line:
//! - `{"expect": {..}}`: read one inbound line; it must contain every key of the object
//!   (objects match by subset, `"*"` matches anything). A mismatch is logged and exits with 3.
//! - `{"emit": {..}}`: write the frame; a top-level `"id": "$id"` takes the id of the last
//!   inbound request.
//! - `{"sleep_ms": n}`, `{"hang": true}`, `{"exit": code}`.
//! - `{"self_pid": "<file>"}`: write this process's pid to the file.
//! - `{"grandchild": "<file>"}`: start a long-lived child in this process group and write its
//!   pid to the file.
//!
//! When the script ends the fake keeps reading (and logging) stdin until it closes.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::zombie_processes)]

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

fn matches(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
        (Value::String(text), _) if text == "*" => true,
        (Value::Object(expected), Value::Object(actual)) => expected
            .iter()
            .all(|(key, value)| actual.get(key).is_some_and(|found| matches(value, found))),
        (Value::Array(expected), Value::Array(actual)) => {
            expected.len() == actual.len()
                && expected.iter().zip(actual).all(|(e, a)| matches(e, a))
        }
        _ => expected == actual,
    }
}

fn log(path: &Option<String>, line: &str) {
    if let Some(path) = path
        && let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version") => {
            let version = std::env::var("FAKE_CODEX_VERSION").unwrap_or("0.171.0".into());
            println!("codex-cli {version}");
        }
        Some("--sleeper") => std::thread::sleep(Duration::from_secs(600)),
        Some("app-server") => serve(),
        other => {
            eprintln!("fake codex: unexpected arguments {other:?}");
            std::process::exit(2);
        }
    }
}

fn serve() {
    let script = std::fs::read_to_string(std::env::var("FAKE_CODEX_SCRIPT").unwrap()).unwrap();
    let log_path = std::env::var("FAKE_CODEX_LOG").ok();
    let stdin = std::io::stdin();
    let mut inbound = stdin.lock().lines();
    let mut stdout = std::io::stdout();
    let mut last_id = Value::Null;
    for line in script.lines().filter(|line| !line.trim().is_empty()) {
        let step: Value = serde_json::from_str(line).unwrap();
        if let Some(expected) = step.get("expect") {
            let Some(Ok(text)) = inbound.next() else {
                log(&log_path, "EOF while expecting");
                std::process::exit(4);
            };
            log(&log_path, &text);
            let actual: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if !matches(expected, &actual) {
                log(&log_path, &format!("MISMATCH expected {expected}"));
                eprintln!("fake codex: expected {expected}, got {text}");
                std::process::exit(3);
            }
            if let Some(id) = actual.get("id") {
                last_id = id.clone();
            }
        } else if let Some(frame) = step.get("emit") {
            let mut frame = frame.clone();
            if frame.get("id").and_then(Value::as_str) == Some("$id") {
                frame["id"] = last_id.clone();
            }
            writeln!(stdout, "{frame}").unwrap();
            stdout.flush().unwrap();
        } else if let Some(ms) = step.get("sleep_ms").and_then(Value::as_u64) {
            std::thread::sleep(Duration::from_millis(ms));
        } else if step.get("hang").is_some() {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        } else if let Some(code) = step.get("exit").and_then(Value::as_i64) {
            std::process::exit(code as i32);
        } else if let Some(file) = step.get("self_pid").and_then(Value::as_str) {
            std::fs::write(file, std::process::id().to_string()).unwrap();
        } else if let Some(file) = step.get("grandchild").and_then(Value::as_str) {
            let child = Command::new(std::env::current_exe().unwrap())
                .arg("--sleeper")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            std::fs::write(file, child.id().to_string()).unwrap();
        }
    }
    for text in inbound.map_while(Result::ok) {
        log(&log_path, &text);
    }
}
