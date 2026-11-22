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

const FAKE_AGY_MJS: &str = r#"
import fs from "node:fs";
import readline from "node:readline";

const scenarioPath = process.env.FAKE_AGY_SCENARIO;
const logPath = process.env.FAKE_AGY_LOG;
const scenario = scenarioPath && fs.existsSync(scenarioPath)
  ? JSON.parse(fs.readFileSync(scenarioPath, "utf8"))
  : {};

function logEntry(entry) {
  if (logPath) {
    fs.appendFileSync(logPath, JSON.stringify(entry) + "\n");
  }
}

const capturedEnv = {
  AGY_CLI_DISABLE_AUTO_UPDATE: process.env.AGY_CLI_DISABLE_AUTO_UPDATE ?? null,
  GEMINI_API_KEY: process.env.GEMINI_API_KEY ?? null,
  GOOGLE_API_KEY: process.env.GOOGLE_API_KEY ?? null,
};

const args = process.argv.slice(2);
logEntry({ kind: "spawn", argv: args, cwd: process.cwd(), pid: process.pid, env: capturedEnv });

if (args[0] === "--version") {
  if (scenario.versionExitCode) {
    process.stderr.write("agy broken\n");
    process.exit(scenario.versionExitCode);
  }
  process.stdout.write(`${scenario.cliVersion ?? "1.5.14"}\n`);
  process.exit(0);
}

if (args[0] === "models") {
  process.stderr.write("Fetching available models...\n");
  const lines = scenario.cliModels ?? [
    "gemini-4-neon-high\tGemini 4 Neon (High)",
    "gemini-3.8-flash-low\tGemini 3.8 Flash (Low)",
    "claude-opus-5-5-high\tClaude Opus 5.5 (High)",
  ];
  process.stdout.write(lines.join("\n") + "\n");
  process.exit(0);
}

if (args[0] !== "acp") {
  process.stderr.write(`unexpected args: ${args.join(" ")}\n`);
  process.exit(2);
}

function send(frame) {
  process.stdout.write(JSON.stringify(frame) + "\n");
}

let promptCount = 0;
let activePromptId = null;
const pendingServerRequests = new Map();
let nextServerReqId = 1;

function askClient(method, params) {
  const id = `srv-${nextServerReqId++}`;
  return new Promise((resolve) => {
    pendingServerRequests.set(id, resolve);
    send({ jsonrpc: "2.0", id, method, params });
  });
}

async function runSteps(steps, sessionId, promptId) {
  for (const step of steps ?? []) {
    if (activePromptId !== promptId && promptId !== null) {
      return "cancelled";
    }
    if (step.sleepMs) {
      await new Promise((r) => setTimeout(r, step.sleepMs));
      if (activePromptId !== promptId && promptId !== null) {
        return "cancelled";
      }
    }
    if (step.notify) {
      send({
        jsonrpc: "2.0",
        method: step.notify.method ?? "session/update",
        params: { sessionId, ...step.notify.params },
      });
    }
    if (step.permission) {
      const res = await askClient("session/request_permission", {
        sessionId,
        ...step.permission,
      });
      logEntry({ kind: "permission_reply", reply: res });
      if (step.echoDecision) {
        send({
          jsonrpc: "2.0",
          method: "session/update",
          params: {
            sessionId,
            update: {
              sessionUpdate: "agent_message_chunk",
              content: { type: "text", text: `perm=${JSON.stringify(res.outcome)}` },
            },
          },
        });
      }
    }
    if (step.elicitation) {
      const res = await askClient("elicitation/create", {
        sessionId,
        ...step.elicitation,
      });
      logEntry({ kind: "elicitation_reply", reply: res });
      if (step.echoDecision) {
        send({
          jsonrpc: "2.0",
          method: "session/update",
          params: {
            sessionId,
            update: {
              sessionUpdate: "agent_message_chunk",
              content: { type: "text", text: `elicit=${JSON.stringify(res)}` },
            },
          },
        });
      }
    }
  }
  return null;
}

const defaultModels = {
  currentModelId: "gemini-4-neon-high",
  availableModels: [
    { modelId: "gemini-4-neon-high", name: "Gemini 4 Neon (High)" },
    { modelId: "gemini-3.8-flash-low", name: "Gemini 3.8 Flash (Low)" },
    { modelId: "claude-opus-5-5-high", name: "Claude Opus 5.5 (High)" },
  ],
};

const defaultModes = {
  currentModeId: "default",
  availableModes: [
    { id: "default", name: "Default", description: "Ask before risky actions." },
    { id: "accept-edits", name: "Accept Edits", description: "Auto-approve file edits." },
    { id: "auto", name: "Auto", description: "Auto-approve actions." },
    { id: "plan", name: "Plan", description: "Read-only planning." },
  ],
};

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });

rl.on("line", async (line) => {
  if (!line.trim()) return;
  const msg = JSON.parse(line);
  logEntry({ kind: "inbound", argv: args, msg });

  if (msg.id !== undefined && msg.method === undefined) {
    const waiter = pendingServerRequests.get(String(msg.id));
    if (waiter) {
      pendingServerRequests.delete(String(msg.id));
      waiter(msg.result ?? { error: msg.error });
    }
    return;
  }

  if (msg.method === "initialize") {
    if (scenario.initializeError) {
      send({ jsonrpc: "2.0", id: msg.id, error: scenario.initializeError });
      return;
    }
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        protocolVersion: 1,
        agentInfo: {
          name: "Antigravity ACP Server",
          version: scenario.agentVersion ?? scenario.cliVersion ?? "1.5.14",
        },
        agentCapabilities: scenario.agentCapabilities ?? {
          loadSession: true,
          promptCapabilities: { image: true, audio: false, embeddedContext: true },
          sessionCapabilities: { resume: {}, close: {}, load: {}, list: {} },
          auth: { logout: {} },
        },
      },
    });
    return;
  }

  if (msg.method === "session/new") {
    if (scenario.newError) {
      send({ jsonrpc: "2.0", id: msg.id, error: scenario.newError });
      return;
    }
    const sessionId = scenario.sessionId ?? "0bae8452-4bec-4a00-b5c0-ec54df96624c";
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        sessionId,
        models: scenario.omitSessionModels ? undefined : (scenario.models ?? defaultModels),
        modes: scenario.modes ?? defaultModes,
      },
    });
    if (scenario.commands) {
      send({
        jsonrpc: "2.0",
        method: "session/update",
        params: {
          sessionId,
          update: {
            sessionUpdate: "available_commands_update",
            availableCommands: scenario.commands,
          },
        },
      });
    }
    return;
  }

  if (msg.method === "session/resume") {
    if (scenario.resumeError) {
      send({ jsonrpc: "2.0", id: msg.id, error: scenario.resumeError });
      return;
    }
    const sessionId = msg.params.sessionId;
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        sessionId,
        models: scenario.omitSessionModels ? undefined : (scenario.resumeModels ?? defaultModels),
        modes: scenario.modes ?? defaultModes,
      },
    });
    return;
  }

  if (msg.method === "session/load") {
    const sessionId = msg.params.sessionId;
    for (const replay of scenario.loadReplayBefore ?? []) {
      send({
        jsonrpc: "2.0",
        method: "session/update",
        params: { sessionId, ...replay },
      });
    }
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        sessionId,
        models: scenario.omitSessionModels ? undefined : (scenario.models ?? defaultModels),
        modes: scenario.modes ?? defaultModes,
      },
    });
    for (const replay of scenario.loadReplayAfter ?? []) {
      send({
        jsonrpc: "2.0",
        method: "session/update",
        params: { sessionId, ...replay },
      });
    }
    return;
  }

  if (msg.method === "session/set_mode") {
    send({ jsonrpc: "2.0", id: msg.id, result: {} });
    send({
      jsonrpc: "2.0",
      method: "session/update",
      params: {
        sessionId: msg.params.sessionId,
        update: { sessionUpdate: "current_mode_update", currentModeId: msg.params.modeId },
      },
    });
    return;
  }

  if (msg.method === "session/set_model") {
    send({ jsonrpc: "2.0", id: msg.id, result: {} });
    return;
  }

  if (msg.method === "session/cancel") {
    activePromptId = null;
    return;
  }

  if (msg.method === "session/prompt") {
    const idx = scenario.useGlobalTurnCounter
      ? Number(fs.existsSync(scenarioPath + ".turn") ? fs.readFileSync(scenarioPath + ".turn", "utf8") : "0")
      : promptCount;
    if (scenario.useGlobalTurnCounter) {
      fs.writeFileSync(scenarioPath + ".turn", String(idx + 1), "utf8");
    }
    promptCount++;
    const turnSpec = (scenario.turns ?? [])[idx] ?? {};
    if (turnSpec.error) {
      send({ jsonrpc: "2.0", id: msg.id, error: turnSpec.error });
      return;
    }
    const sessionId = msg.params.sessionId;
    const promptId = msg.id;
    activePromptId = promptId;
    const interrupted = await runSteps(turnSpec.steps ?? [], sessionId, promptId);
    const stopReason = interrupted ?? turnSpec.stopReason ?? "end_turn";
    if (activePromptId === promptId) {
      activePromptId = null;
    }
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        stopReason,
        ...(turnSpec.usage ? { usage: turnSpec.usage } : {}),
      },
    });
    if (turnSpec.afterEndTurn && stopReason === "end_turn") {
      void (async () => {
        await runSteps(turnSpec.afterEndTurn, sessionId, null);
      })();
    }
  }
});
"#;

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
            if message["method"] == "session/event" {
                self.events
                    .push(serde_json::from_value(message["params"].clone()).unwrap());
            }
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
            if message["method"] == "session/event" {
                self.events
                    .push(serde_json::from_value(message["params"].clone()).unwrap());
            }
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
