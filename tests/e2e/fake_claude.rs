//! A stand-in for `claude` in the end-to-end tests. `--version` prints a version; any other
//! invocation replays the ndjson script named by `FAKE_CLAUDE_SCRIPT` over stdio. Every inbound
//! line is appended to the file named by `FAKE_CLAUDE_LOG`, preceded by one `{"argv": [..]}` line
//! per launch. When `FAKE_CLAUDE_ENV_LOG` is set the environment received is written to it as
//! `NAME=value` lines. A launch with index N (0-based, counted in the log) plays
//! `<script>.<N>` when that file exists, else `<script>`.
//!
//! Script steps, one JSON object per line:
//! - `{"expect": {..}}`: read one inbound line; it must contain every key of the object
//!   (objects match by subset). A mismatch is logged and exits with 3.
//! - `{"emit": {..}}`: write the frame; any string `"$id"` takes the `request_id` of the last
//!   inbound control request.
//! - `{"sleep_ms": n}`, `{"hang": true}`, `{"exit": code}`, `{"stderr": "text"}`.
//! - `{"self_pid": "<file>"}`: write this process's pid to the file.
//! - `{"write": {"path": "<file>", "content": ".."}}`: write the file, relative to the working
//!   directory (a stand-in for Claude editing the workspace).
//! - `{"grandchild": "<file>"}`: start a long-lived child in this process group and write its
//!   pid to the file.

#![allow(clippy::unwrap_used, clippy::print_stdout, clippy::zombie_processes)]

use std::io::{BufRead, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

fn matches(expected: &Value, actual: &Value) -> bool {
    match (expected, actual) {
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
            log_argv(&args);
            let version = std::env::var("FAKE_CLAUDE_VERSION").unwrap_or("2.1.322".into());
            println!("{version} (Claude Code)");
        }
        Some("--sleeper") => std::thread::sleep(Duration::from_secs(600)),
        _ => serve(&args),
    }
}

fn log_argv(args: &[String]) {
    log(
        &std::env::var("FAKE_CLAUDE_LOG").ok(),
        &serde_json::json!({ "argv": args }).to_string(),
    );
}

fn launch_index() -> usize {
    let Ok(path) = std::env::var("FAKE_CLAUDE_LOG") else {
        return 0;
    };
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.starts_with("{\"argv\":") && !line.contains("\"--version\""))
        .count()
}

fn replace_id(value: &mut Value, id: &Value) {
    match value {
        Value::String(text) if text == "$id" => *value = id.clone(),
        Value::Array(items) => items.iter_mut().for_each(|item| replace_id(item, id)),
        Value::Object(fields) => fields.values_mut().for_each(|item| replace_id(item, id)),
        _ => {}
    }
}

fn serve(args: &[String]) {
    let base = std::env::var("FAKE_CLAUDE_SCRIPT").unwrap();
    let numbered = format!("{base}.{}", launch_index());
    let script = std::fs::read_to_string(&numbered)
        .or_else(|_| std::fs::read_to_string(&base))
        .unwrap();
    let log_path = std::env::var("FAKE_CLAUDE_LOG").ok();
    log_argv(args);
    if let Ok(path) = std::env::var("FAKE_CLAUDE_ENV_LOG") {
        let lines: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
        std::fs::write(path, lines.join("\n")).unwrap();
    }
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
                eprintln!("fake claude: expected {expected}, got {text}");
                std::process::exit(3);
            }
            if let Some(id) = actual.get("request_id") {
                last_id = id.clone();
            }
        } else if let Some(frame) = step.get("emit") {
            let mut frame = frame.clone();
            replace_id(&mut frame, &last_id);
            writeln!(stdout, "{frame}").unwrap();
            stdout.flush().unwrap();
        } else if let Some(ms) = step.get("sleep_ms").and_then(Value::as_u64) {
            std::thread::sleep(Duration::from_millis(ms));
        } else if step.get("hang").is_some() {
            loop {
                std::thread::sleep(Duration::from_secs(3600));
            }
        } else if let Some(text) = step.get("stderr").and_then(Value::as_str) {
            eprintln!("{text}");
        } else if let Some(code) = step.get("exit").and_then(Value::as_i64) {
            std::process::exit(code as i32);
        } else if let Some(file) = step.get("self_pid").and_then(Value::as_str) {
            std::fs::write(file, std::process::id().to_string()).unwrap();
        } else if let Some(write) = step.get("write") {
            std::fs::write(
                write["path"].as_str().unwrap(),
                write["content"].as_str().unwrap(),
            )
            .unwrap();
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
