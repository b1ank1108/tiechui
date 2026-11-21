//! A JSON-RPC client of a spawned `chui app-server`, shared by the runtime end-to-end tests.
#![allow(dead_code, clippy::unwrap_used)]

use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tiechui_protocol::EventKind;
use tiechui_protocol::app::SessionEvent;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next: u64,
    pub events: Vec<SessionEvent>,
}

impl Client {
    pub fn spawn(mut command: Command) -> Self {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            lines,
            next: 0,
            events: vec![],
        }
    }

    /// Closes the app-server's stdin and waits for it to exit.
    pub async fn finish(self) -> std::process::ExitStatus {
        let Self {
            mut child, stdin, ..
        } = self;
        drop(stdin);
        tokio::time::timeout(Duration::from_secs(30), child.wait())
            .await
            .expect("app-server did not exit")
            .unwrap()
    }

    pub async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(20), self.lines.next_line())
            .await
            .expect("app-server stalled")
            .unwrap()
            .expect("app-server exited");
        serde_json::from_str(&line).unwrap()
    }

    pub async fn send(&mut self, method: &str, params: Value) -> u64 {
        self.next += 1;
        let request =
            json!({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params});
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        self.next
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.send(method, params).await;
        self.reply(id).await
    }

    pub async fn reply(&mut self, id: u64) -> Result<Value, Value> {
        loop {
            let message = self.read().await;
            if message["id"] == id {
                return match message.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(message["result"].clone()),
                };
            }
            self.buffer(message);
        }
    }

    fn buffer(&mut self, message: Value) {
        if message["method"] == "session/event" {
            self.events
                .push(serde_json::from_value(message["params"].clone()).unwrap());
        }
    }

    pub async fn ok(&mut self, method: &str, params: Value) -> Value {
        self.call(method, params).await.unwrap()
    }

    /// Takes the buffered and incoming events up to the first one that matches.
    pub async fn until(&mut self, matches: impl Fn(&EventKind) -> bool) -> Vec<SessionEvent> {
        loop {
            if let Some(index) = self.events.iter().position(|e| matches(&e.event.event)) {
                return self.events.drain(..=index).collect();
            }
            let message = self.read().await;
            self.buffer(message);
        }
    }
}
