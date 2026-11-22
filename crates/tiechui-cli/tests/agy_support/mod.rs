#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tiechui_protocol::EventKind;
use tiechui_protocol::app::{SessionEvent, Snapshot};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

const FAKE_AGY_MJS: &str =
    include_str!("../../../tiechui-runtime/tests/fixtures/fake-acp-server.mjs");

pub struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next: u64,
    pub events: Vec<SessionEvent>,
    pub notifications: Vec<Value>,
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
            notifications: vec![],
        }
    }

    pub async fn finish(self) -> std::process::ExitStatus {
        let Self {
            mut child, stdin, ..
        } = self;
        drop(stdin);
        tokio::time::timeout(Duration::from_secs(20), child.wait())
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

    fn stash_notification(&mut self, message: Value) {
        if message["method"] == "session/event" {
            self.events
                .push(serde_json::from_value(message["params"].clone()).unwrap());
        } else if message.get("method").is_some() {
            self.notifications.push(message);
        }
    }

    pub async fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let id = self.send(method, params).await;
        loop {
            let message = self.read().await;
            if message["id"] == id {
                return match message.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(message["result"].clone()),
                };
            }
            self.stash_notification(message);
        }
    }

    pub async fn ok(&mut self, method: &str, params: Value) -> Value {
        self.call(method, params)
            .await
            .unwrap_or_else(|err| panic!("{method} failed: {err}"))
    }

    pub async fn until(&mut self, matches: impl Fn(&EventKind) -> bool) -> Vec<SessionEvent> {
        loop {
            if let Some(index) = self.events.iter().position(|e| matches(&e.event.event)) {
                return self.events.drain(..=index).collect();
            }
            let message = self.read().await;
            self.stash_notification(message);
        }
    }

    pub async fn until_notification(&mut self, method: &str) -> Value {
        loop {
            if let Some(index) = self
                .notifications
                .iter()
                .position(|n| n["method"] == method)
            {
                return self.notifications.remove(index);
            }
            let message = self.read().await;
            self.stash_notification(message);
        }
    }
}

pub struct AgyFixture {
    pub root: TempDir,
    pub home: PathBuf,
    pub cwd: PathBuf,
    pub bin: PathBuf,
    pub scenario_path: PathBuf,
    pub log_path: PathBuf,
}

impl AgyFixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cwd = root.path().join("work");
        let bin = root.path().join("bin");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(home.join("config.toml"), "model = \"fixture/model\"\n").unwrap();
        std::fs::write(
            home.join("providers.toml"),
            "[fixture]\nwire = \"openai-completions\"\nbase_url = \"http://127.0.0.1:9\"\napi_key_env = \"TC_AGY_TEST_KEY\"\nmodels = [{ id = \"model\", context_window = 200000, max_output = 4096 }]\n",
        )
        .unwrap();
        let mjs_path = root.path().join("fake-agy.mjs");
        std::fs::write(&mjs_path, FAKE_AGY_MJS).unwrap();
        let scenario_path = root.path().join("scenario.json");
        std::fs::write(&scenario_path, "{}").unwrap();
        let log_path = root.path().join("fake-agy.log");
        install_fake_agy(&bin, &mjs_path);
        Self {
            root,
            home,
            cwd,
            bin,
            scenario_path,
            log_path,
        }
    }

    pub fn agy_binary_path(&self) -> PathBuf {
        if cfg!(windows) {
            self.bin.join("agy.cmd")
        } else {
            self.bin.join("agy")
        }
    }

    pub fn write_scenario(&self, value: &Value) {
        std::fs::write(&self.scenario_path, serde_json::to_vec(value).unwrap()).unwrap();
    }

    pub fn logs(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    pub fn client(&self) -> Client {
        self.client_with(&[], true)
    }

    pub fn client_without_agy(&self) -> Client {
        self.client_with(&[], false)
    }

    pub fn client_with(&self, extra_env: &[(&str, &str)], include_bin: bool) -> Client {
        let mut dirs: Vec<&Path> = Vec::new();
        if include_bin {
            dirs.push(self.bin.as_path());
        }
        #[cfg(unix)]
        {
            dirs.push(Path::new("/usr/bin"));
            dirs.push(Path::new("/bin"));
        }
        let search = std::env::join_paths(dirs).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_chui"));
        cmd.env("PATH", search)
            .env("TIECHUI_HOME", &self.home)
            .env("HOME", self.root.path())
            .env("TC_AGY_TEST_KEY", "fake-native-key")
            .env("FAKE_AGY_SCENARIO", &self.scenario_path)
            .env("FAKE_AGY_LOG", &self.log_path)
            .env("CHUI_AGY_WAKEUP_IDLE_MS", "120")
            .current_dir(&self.cwd)
            .arg("app-server");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        Client::spawn(cmd)
    }
}

fn find_node() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is set");
    for dir in std::env::split_paths(&path) {
        let candidate = if cfg!(windows) {
            dir.join("node.exe")
        } else {
            dir.join("node")
        };
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("node executable not found on PATH");
}

fn install_fake_agy(bin_dir: &Path, mjs_path: &Path) {
    let node = find_node();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let wrapper = bin_dir.join("agy");
        let script = format!(
            "#!/bin/sh\nexec \"{}\" \"{}\" \"$@\"\n",
            node.display(),
            mjs_path.display()
        );
        std::fs::write(&wrapper, script).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    #[cfg(windows)]
    {
        let wrapper = bin_dir.join("agy.cmd");
        let script = format!(
            "@echo off\r\n\"{}\" \"{}\" %*\r\n",
            node.display(),
            mjs_path.display()
        );
        std::fs::write(&wrapper, script).unwrap();
    }
}

pub fn snapshot(value: Value) -> Snapshot {
    serde_json::from_value(value).unwrap()
}

pub fn runtime_entry<'a>(list: &'a Value, id: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == id)
        .unwrap_or_else(|| panic!("runtimes/list has no {id}: {list}"))
}
