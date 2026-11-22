#![allow(clippy::unwrap_used)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;
use tiechui_module_rewind::store::{SnapshotRecord, Store as RewindStore};
use tiechui_protocol::{
    ApprovalDecision, ApprovalRequest, CommandInfo, DerivedReason, DerivedTurnMarker, Event,
    EventKind, InputContent, InputDelivery, Item, KnownItem, Op, PERMISSION_MODE_EVENT,
    RuntimeCapabilities, SessionEpoch, SteeringMode, SubagentState, SubagentStatus, Submission,
    SubmissionId, ThreadId, TokenUsage, TurnId, TurnStatus,
};
use tiechui_runtime::acp::{
    AcpBackend, AcpConfig, AcpDialect, AcpLaunchSpec, AcpSubagent, AcpTimeouts, CustomApproval,
    InitializeInfo, TextDisposition, ToolState, form_elicitation_as_questions,
    interaction_permission_as_questions,
};
use tiechui_runtime::{Launch, Open, Opened, SessionConfig, open_session};
use tiechui_session::SessionStore;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

const FAKE_ACP_SERVER_JS: &str = r#"
import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";

const scenarioPath = process.env.FAKE_ACP_SCENARIO;
const logPath = process.env.FAKE_ACP_LOG;
const scenario = JSON.parse(fs.readFileSync(scenarioPath, "utf8"));

function logFrame(entry) {
  if (logPath) {
    fs.appendFileSync(logPath, JSON.stringify(entry) + "\n");
  }
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
    if (step.writeFile) {
      const target = path.resolve(process.cwd(), step.writeFile.path);
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.writeFileSync(target, step.writeFile.content, "utf8");
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
      logFrame({ kind: "permission_reply", reply: res });
      if (step.echoDecision) {
        send({
          jsonrpc: "2.0",
          method: "session/update",
          params: {
            sessionId,
            update: {
              sessionUpdate: "agent_message_chunk",
              content: { type: "text", text: `decision=${JSON.stringify(res.outcome)}` },
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
      logFrame({ kind: "elicitation_reply", reply: res });
      if (step.echoDecision) {
        send({
          jsonrpc: "2.0",
          method: "session/update",
          params: {
            sessionId,
            update: {
              sessionUpdate: "agent_message_chunk",
              content: { type: "text", text: `elicitation=${JSON.stringify(res)}` },
            },
          },
        });
      }
    }
    if (step.waitCancel) {
      while (activePromptId === promptId) {
        await new Promise((r) => setTimeout(r, 15));
      }
      return "cancelled";
    }
  }
  return null;
}

const rl = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });

rl.on("line", async (line) => {
  if (!line.trim()) return;
  const msg = JSON.parse(line);
  logFrame({ kind: "inbound", msg });

  if (msg.id !== undefined && msg.method === undefined) {
    const waiter = pendingServerRequests.get(String(msg.id));
    if (waiter) {
      pendingServerRequests.delete(String(msg.id));
      waiter(msg.result ?? { error: msg.error });
    }
    return;
  }

  if (msg.method === "initialize") {
    if (scenario.exitOnInitialize) {
      console.error("fatal boot error");
      process.exit(1);
    }
    if (scenario.hangOnInitialize) {
      return;
    }
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: scenario.initialize ?? {
        protocolVersion: 1,
        agentInfo: { name: "fake-acp", title: "Fake ACP", version: "1.5.13" },
        agentCapabilities: {
          loadSession: true,
          promptCapabilities: { image: true },
          sessionCapabilities: { resume: {} },
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
    const sessionId = scenario.newSessionId ?? "acp-sess-new";
    if (scenario.commandsOnStart) {
      send({
        jsonrpc: "2.0",
        method: "session/update",
        params: {
          sessionId,
          update: {
            sessionUpdate: "available_commands_update",
            availableCommands: scenario.commandsOnStart,
          },
        },
      });
    }
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        sessionId,
        modes: scenario.modes ?? {
          currentModeId: "default",
          availableModes: [
            { id: "default", name: "Default" },
            { id: "accept-edits", name: "Accept edits" },
          ],
        },
        models: scenario.models ?? {
          currentModelId: "gemini-2.5-pro",
          availableModels: [
            { modelId: "gemini-2.5-pro", name: "Gemini 2.5 Pro" },
            { modelId: "gemini-2.5-flash", name: "Gemini 2.5 Flash" },
          ],
        },
      },
    });
    return;
  }

  if (msg.method === "session/resume") {
    if (scenario.resumeError) {
      send({ jsonrpc: "2.0", id: msg.id, error: scenario.resumeError });
      return;
    }
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: {
        modes: { currentModeId: "default", availableModes: [{ id: "default", name: "Default" }] },
        models: {
          currentModelId: "gemini-2.5-pro",
          availableModels: [{ modelId: "gemini-2.5-pro", name: "Gemini 2.5 Pro" }],
        },
      },
    });
    return;
  }

  if (msg.method === "session/load") {
    if (scenario.loadError) {
      send({ jsonrpc: "2.0", id: msg.id, error: scenario.loadError });
      return;
    }
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
        modes: { currentModeId: "default", availableModes: [{ id: "default", name: "Default" }] },
        models: {
          currentModelId: "gemini-2.5-pro",
          availableModels: [{ modelId: "gemini-2.5-pro", name: "Gemini 2.5 Pro" }],
        },
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

  if (msg.method === "session/set_mode" || msg.method === "session/set_model") {
    send({ jsonrpc: "2.0", id: msg.id, result: {} });
    return;
  }

  if (msg.method === "session/cancel") {
    activePromptId = null;
    return;
  }

  if (msg.method === "session/prompt") {
    const turnSpec = (scenario.turns ?? [])[promptCount++] ?? {};
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

struct AcpRig {
    _root: TempDir,
    home: PathBuf,
    cwd: PathBuf,
    script_path: PathBuf,
    scenario_path: PathBuf,
    log_path: PathBuf,
}

impl AcpRig {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let cwd = root.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let script_path = root.path().join("fake-acp.mjs");
        let scenario_path = root.path().join("scenario.json");
        let log_path = root.path().join("acp.log");
        std::fs::write(&script_path, FAKE_ACP_SERVER_JS).unwrap();
        Self {
            _root: root,
            home,
            cwd,
            script_path,
            scenario_path,
            log_path,
        }
    }

    fn init_git(&self) {
        let git = |args: &[&str]| {
            let status = Command::new("git")
                .args(args)
                .current_dir(&self.cwd)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
    }

    fn write_scenario(&self, scenario: &Value) {
        std::fs::write(&self.scenario_path, serde_json::to_vec(scenario).unwrap()).unwrap();
    }

    fn spec(&self) -> AcpLaunchSpec {
        let mut spec = AcpLaunchSpec::new("node", vec![self.script_path.display().to_string()]);
        spec.env_set.push((
            "FAKE_ACP_SCENARIO".into(),
            self.scenario_path.display().to_string(),
        ));
        spec.env_set
            .push(("FAKE_ACP_LOG".into(), self.log_path.display().to_string()));
        spec
    }

    fn config(&self) -> AcpConfig {
        let mut config = AcpConfig::generic(Launch::from_env(), self.spec());
        config.timeouts = AcpTimeouts {
            handshake: Duration::from_millis(800),
            session: Duration::from_secs(5),
            request: Duration::from_secs(5),
            wakeup_idle: Duration::from_millis(120),
        };
        config
    }

    fn log_entries(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log_path)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    fn inbound_methods(&self) -> Vec<(String, Value)> {
        self.log_entries()
            .into_iter()
            .filter(|entry| entry["kind"] == "inbound")
            .filter_map(|entry| {
                let msg = &entry["msg"];
                Some((msg["method"].as_str()?.to_owned(), msg.clone()))
            })
            .collect()
    }
}

struct LiveSession {
    opened: Opened,
    submissions: mpsc::Sender<Submission>,
    events: broadcast::Receiver<Event>,
    shutdown: CancellationToken,
    seq: u64,
}

impl LiveSession {
    async fn start(
        rig: &AcpRig,
        open: Open,
        capabilities: RuntimeCapabilities,
        config: AcpConfig,
        mode: Option<&str>,
    ) -> Result<Self, String> {
        let shutdown = CancellationToken::new();
        let mut session_config = SessionConfig::new(rig.home.clone(), open);
        session_config.mode = mode.map(str::to_owned);
        let opened = open_session(
            "acp",
            capabilities,
            Box::new(AcpBackend::new(config)),
            session_config,
            shutdown.clone(),
        )
        .await?;
        let submissions = opened.handle.submissions.clone();
        let events = opened.handle.events.resubscribe();
        Ok(Self {
            opened,
            submissions,
            events,
            shutdown,
            seq: 0,
        })
    }

    async fn submit(&mut self, op: Op) -> SubmissionId {
        self.seq += 1;
        let id = SubmissionId(format!("sub-{}", self.seq));
        self.submissions
            .send(Submission {
                id: id.clone(),
                thread_id: ThreadId(self.opened.id.clone()),
                epoch: SessionEpoch(0),
                op,
            })
            .await
            .unwrap();
        id
    }

    async fn send_input(&mut self, content: Vec<InputContent>) -> SubmissionId {
        self.submit(Op::UserInput {
            content,
            delivery: InputDelivery::FollowUp,
        })
        .await
    }

    async fn send_text(&mut self, text: &str) -> SubmissionId {
        self.send_input(vec![InputContent::Text { text: text.into() }])
            .await
    }

    async fn until(&mut self, pred: impl Fn(&EventKind) -> bool) -> Vec<EventKind> {
        let mut out = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), self.events.recv())
                .await
                .expect("timed out waiting for session event")
                .unwrap();
            let hit = pred(&event.event);
            out.push(event.event);
            if hit {
                return out;
            }
        }
    }

    async fn close(self) {
        self.shutdown.cancel();
        let _ = self.opened.handle.task.await;
    }
}

fn acp_capabilities() -> RuntimeCapabilities {
    RuntimeCapabilities {
        steering: SteeringMode::Queue,
        commands: true,
        model: true,
        ..RuntimeCapabilities::minimal()
    }
}

#[tokio::test]
async fn streaming_text_thinking_tools_plan_commands_and_usage_map_to_session_events() {
    let rig = AcpRig::new();
    rig.write_scenario(&json!({
        "commandsOnStart": [
            { "name": "plan", "description": "Create an execution plan", "input": { "hint": "[goal]" } },
            { "name": "review", "description": "Review changes" }
        ],
        "turns": [{
            "steps": [
                { "notify": { "params": { "update": {
                    "sessionUpdate": "usage_update",
                    "used": 1200,
                    "size": 1048576
                }}}},
                { "notify": { "params": { "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": "Inspecting " }
                }}}},
                { "notify": { "params": { "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": "files..." }
                }}}},
                { "notify": { "params": { "update": {
                    "sessionUpdate": "plan",
                    "entries": [
                        { "content": "Read greeting", "priority": "high", "status": "in_progress" },
                        { "content": "Update greeting", "priority": "medium", "status": "pending" }
                    ]
                }}}},
                { "notify": { "params": { "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call-read",
                    "kind": "read",
                    "title": "Read src/hello.ts",
                    "status": "in_progress",
                    "locations": [{ "path": "src/hello.ts", "line": 10 }],
                    "rawInput": { "path": "src/hello.ts", "offset": 10, "limit": 5 }
                }}}},
                { "notify": { "params": { "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-read",
                    "status": "completed",
                    "content": [{ "type": "content", "content": { "type": "text", "text": "export const hi = 1;" } }]
                }}}},
                { "notify": { "params": { "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call-edit",
                    "kind": "edit",
                    "title": "Edit src/hello.ts",
                    "status": "completed",
                    "content": [{
                        "type": "diff",
                        "path": "src/hello.ts",
                        "oldText": "export const hi = 1;\n",
                        "newText": "export const hi = 2;\n"
                    }]
                }}}},
                { "notify": { "params": { "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call-exec",
                    "kind": "execute",
                    "title": "npm test",
                    "status": "in_progress",
                    "rawInput": { "command": "npm test" }
                }}}},
                { "notify": { "params": { "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "call-exec",
                    "status": "completed",
                    "rawOutput": { "stdout": "all green\n", "exitCode": 0 }
                }}}},
                { "notify": { "params": { "update": {
                    "sessionUpdate": "plan",
                    "entries": [
                        { "content": "Read greeting", "priority": "high", "status": "completed" },
                        { "content": "Update greeting", "priority": "medium", "status": "completed" }
                    ]
                }}}},
                { "notify": { "params": { "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "Updated " }
                }}}},
                { "notify": { "params": { "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "successfully." }
                }}}}
            ],
            "usage": {
                "inputTokens": 400,
                "outputTokens": 50,
                "cachedReadTokens": 100,
                "cachedWriteTokens": 0,
                "thoughtTokens": 15,
                "totalTokens": 550
            },
            "stopReason": "end_turn"
        }]
    }));

    let mut session = LiveSession::start(
        &rig,
        Open::New {
            cwd: rig.cwd.clone(),
        },
        acp_capabilities(),
        rig.config(),
        Some("auto"),
    )
    .await
    .unwrap();

    let startup = session
        .until(|e| matches!(e, EventKind::CommandsUpdated { commands } if !commands.is_empty()))
        .await;
    assert!(startup.iter().any(|e| matches!(
        e,
        EventKind::CommandsUpdated { commands }
            if commands
                == &vec![
                    CommandInfo {
                        name: "plan".into(),
                        description: "Create an execution plan".into(),
                        argument_hint: Some("[goal]".into()),
                    },
                    CommandInfo {
                        name: "review".into(),
                        description: "Review changes".into(),
                        argument_hint: None,
                    },
                ]
    )));

    session
        .send_input(vec![
            InputContent::Text {
                text: "Update greeting".into(),
            },
            InputContent::Image {
                mime_type: "image/png".into(),
                data: "aGVsbG8=".into(),
            },
        ])
        .await;

    let events = session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, EventKind::ItemDelta { delta, .. } if delta.contains("Updated")))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        EventKind::SessionUpdated { model, .. } if model.context_window == 1_048_576
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        EventKind::TokenUsageUpdated {
            last: TokenUsage {
                input: 400,
                output: 50,
                cache_read: 100,
                reasoning: 15,
                ..
            },
            ..
        }
    )));

    let EventKind::TurnCompleted { turn } = events.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(turn.status, TurnStatus::Completed);
    assert!(turn.items.iter().any(|item| matches!(
        item,
        Item::Known(KnownItem::Thinking { text, .. }) if text == "Inspecting files..."
    )));
    assert!(turn.items.iter().any(|item| matches!(
        item,
        Item::Known(KnownItem::ToolCall { name, arguments, .. })
            if name == "read" && arguments["path"] == "src/hello.ts"
    )));
    assert!(turn.items.iter().any(|item| matches!(
        item,
        Item::Known(KnownItem::ToolCall { name, arguments, .. })
            if name == "edit" && arguments["path"] == "src/hello.ts"
    )));
    assert!(turn.items.iter().any(|item| matches!(
        item,
        Item::Known(KnownItem::ToolCall { name, arguments, .. })
            if name == "bash" && arguments["command"] == "npm test"
    )));
    assert!(turn.items.iter().any(|item| matches!(
        item,
        Item::Known(KnownItem::ToolCall { name, arguments, .. })
            if name == "plan"
                && arguments["plan"]
                    .as_str()
                    .is_some_and(|p| p.contains("- [x] Read greeting") && p.contains("- [x] Update greeting"))
    )));
    assert!(turn.items.iter().any(|item| matches!(
        item,
        Item::Known(KnownItem::AssistantMessage { text, .. }) if text == "Updated successfully."
    )));

    let inbound = rig.inbound_methods();
    let (_, init_msg) = inbound.iter().find(|(m, _)| m == "initialize").unwrap();
    assert_eq!(
        init_msg["params"]["clientCapabilities"]["fs"],
        json!({ "readTextFile": false, "writeTextFile": false })
    );
    assert_eq!(init_msg["params"]["clientCapabilities"]["terminal"], false);
    assert!(
        inbound
            .iter()
            .any(|(m, msg)| m == "session/set_mode" && msg["params"]["modeId"] == "accept-edits")
    );
    let (_, prompt_msg) = inbound.iter().find(|(m, _)| m == "session/prompt").unwrap();
    assert_eq!(
        prompt_msg["params"]["prompt"],
        json!([
            { "type": "text", "text": "Update greeting" },
            { "type": "image", "mimeType": "image/png", "data": "aGVsbG8=" }
        ])
    );

    session.close().await;
}

#[tokio::test]
async fn permission_requests_and_form_elicitation_round_trip_decisions() {
    let rig = AcpRig::new();
    rig.write_scenario(&json!({
        "turns": [{
            "steps": [
                {
                    "permission": {
                        "toolCall": {
                            "toolCallId": "call-touch-1",
                            "kind": "execute",
                            "title": "touch approved.txt",
                            "rawInput": { "command": "touch approved.txt" }
                        },
                        "options": [
                            { "optionId": "opt-once", "name": "Allow", "kind": "allow_once" },
                            { "optionId": "opt-deny", "name": "Deny", "kind": "reject_once" }
                        ]
                    },
                    "echoDecision": true
                },
                {
                    "permission": {
                        "toolCall": {
                            "toolCallId": "call-touch-2",
                            "kind": "execute",
                            "title": "touch denied.txt",
                            "rawInput": { "command": "touch denied.txt" }
                        },
                        "options": [
                            { "optionId": "opt-once", "name": "Allow", "kind": "allow_once" },
                            { "optionId": "opt-always", "name": "Allow Session", "kind": "allow_always" },
                            { "optionId": "opt-deny", "name": "Deny", "kind": "reject_once" }
                        ]
                    },
                    "echoDecision": true
                },
                {
                    "elicitation": {
                        "mode": "form",
                        "message": "Configure deployment",
                        "requestedSchema": {
                            "type": "object",
                            "required": ["env", "replicas"],
                            "properties": {
                                "env": {
                                    "type": "string",
                                    "title": "Environment",
                                    "oneOf": [
                                        { "const": "staging", "title": "Staging" },
                                        { "const": "prod", "title": "Production" }
                                    ]
                                },
                                "replicas": {
                                    "type": "integer",
                                    "title": "Replicas"
                                },
                                "dryRun": {
                                    "type": "boolean",
                                    "title": "Dry run"
                                }
                            }
                        }
                    },
                    "echoDecision": true
                }
            ],
            "stopReason": "end_turn"
        }]
    }));

    let mut session = LiveSession::start(
        &rig,
        Open::New {
            cwd: rig.cwd.clone(),
        },
        acp_capabilities(),
        rig.config(),
        Some("rules"),
    )
    .await
    .unwrap();

    session.send_text("deploy").await;

    let first = session
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested {
        approval_id: id1,
        request: ApprovalRequest::Permission { tool, values, .. },
        ..
    } = first.last().unwrap().clone()
    else {
        panic!("expected permission approval: {first:?}");
    };
    assert_eq!(tool, "bash");
    assert_eq!(values, vec!["touch approved.txt".to_owned()]);
    session
        .submit(Op::ApprovalResponse {
            approval_id: id1,
            decision: ApprovalDecision::AllowSession {
                both_directions: false,
            },
        })
        .await;

    let second = session
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested {
        approval_id: id2, ..
    } = second.last().unwrap().clone()
    else {
        panic!("expected second permission: {second:?}");
    };
    session
        .submit(Op::ApprovalResponse {
            approval_id: id2,
            decision: ApprovalDecision::Deny {
                reason: Some("no".into()),
            },
        })
        .await;

    let third = session
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested {
        approval_id: id3,
        request: ApprovalRequest::Elicitation { request, .. },
        ..
    } = third.last().unwrap().clone()
    else {
        panic!("expected elicitation approval: {third:?}");
    };
    assert_eq!(
        request["requestedSchema"]["properties"]["env"]["enum"],
        json!(["staging", "prod"])
    );
    session
        .submit(Op::ApprovalResponse {
            approval_id: id3,
            decision: ApprovalDecision::Accept {
                content: Some(json!({
                    "env": "staging",
                    "replicas": "3",
                    "dryRun": true
                })),
            },
        })
        .await;

    let done = session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    let EventKind::TurnCompleted { turn } = done.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(turn.status, TurnStatus::Completed);

    let logs = rig.log_entries();
    let perm_replies: Vec<_> = logs
        .iter()
        .filter(|e| e["kind"] == "permission_reply")
        .collect();
    assert_eq!(
        perm_replies[0]["reply"]["outcome"],
        json!({ "outcome": "selected", "optionId": "opt-once" })
    );
    assert_eq!(
        perm_replies[1]["reply"]["outcome"],
        json!({ "outcome": "selected", "optionId": "opt-deny" })
    );
    let elicit_reply = logs
        .iter()
        .find(|e| e["kind"] == "elicitation_reply")
        .unwrap();
    assert_eq!(
        elicit_reply["reply"],
        json!({
            "action": "accept",
            "content": {
                "env": "staging",
                "replicas": 3,
                "dryRun": true
            }
        })
    );

    session.close().await;
}

#[tokio::test]
async fn dialect_hooks_support_question_conversion_subagents_and_error_notices() {
    struct TestDialect {
        spec: AcpLaunchSpec,
    }
    impl AcpDialect for TestDialect {
        fn provider_id(&self) -> &'static str {
            "agy-test"
        }
        fn launch(
            &self,
            _launch: &Launch,
            _params: &tiechui_runtime::StartParams,
        ) -> Result<AcpLaunchSpec, String> {
            Ok(self.spec.clone())
        }
        fn check_initialize(&self, info: &InitializeInfo) -> Result<(), String> {
            if info.agent_version.as_deref() == Some("1.5.1") {
                return Err("requires 1.5.2 or newer".into());
            }
            Ok(())
        }
        fn filter_command(&self, command: &CommandInfo) -> bool {
            command.name != "logout"
        }
        fn on_text_chunk(&self, _thinking: bool, text: &str) -> TextDisposition {
            if text.contains("UNAUTHENTICATED (code 401)") {
                TextDisposition::FailTurn("Please run agy in terminal to sign in".into())
            } else {
                TextDisposition::Keep
            }
        }
        fn normalize_tool(&self, tool: &mut ToolState) {
            if let Some(cmd) = tool
                .raw_input
                .as_ref()
                .and_then(|v| v["CommandLine"].as_str())
                .map(str::to_owned)
            {
                tool.raw_input = Some(json!({ "command": cmd }));
            }
        }
        fn tool_subagent(&self, tool: &ToolState) -> Option<AcpSubagent> {
            (tool.name.as_deref() == Some("invoke_subagent")).then(|| AcpSubagent {
                agent_id: tool.id.clone(),
                name: Some("research".into()),
                state: match tool.status.as_deref() {
                    Some("completed") => SubagentState::Completed,
                    Some("failed") => SubagentState::Failed,
                    _ => SubagentState::Running,
                },
                message: tool
                    .raw_output
                    .as_ref()
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        }
        fn intercept_request(&self, method: &str, params: &Value) -> Option<CustomApproval> {
            match method {
                "session/request_permission" => interaction_permission_as_questions(params),
                "elicitation/create" => form_elicitation_as_questions(params),
                _ => None,
            }
        }
    }

    let rig = AcpRig::new();
    rig.write_scenario(&json!({
        "commandsOnStart": [
            { "name": "plan", "description": "Plan" },
            { "name": "logout", "description": "Sign out" }
        ],
        "turns": [
            {
                "steps": [
                    {
                        "permission": {
                            "toolCall": {
                                "toolCallId": "interaction_1",
                                "title": "Pick a framework"
                            },
                            "options": [
                                { "optionId": "vitest", "name": "Vitest (Recommended)", "kind": "allow_once" },
                                { "optionId": "jest", "name": "Jest", "kind": "allow_once" }
                            ]
                        }
                    },
                    {
                        "elicitation": {
                            "mode": "form",
                            "message": "The agent has a question.",
                            "requestedSchema": {
                                "type": "object",
                                "properties": {
                                    "q0": {
                                        "type": "string",
                                        "title": "Target runtime?",
                                        "enum": ["Node", "Bun"]
                                    },
                                    "q0__other": {
                                        "type": "string",
                                        "title": "Other"
                                    }
                                }
                            }
                        }
                    },
                    { "notify": { "params": { "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": "sub-1",
                        "name": "invoke_subagent",
                        "title": "Running invoke_subagent",
                        "status": "in_progress",
                        "rawInput": { "prompt": "Check package.json" }
                    }}}},
                    { "notify": { "params": { "update": {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": "sub-1",
                        "name": "invoke_subagent",
                        "status": "completed",
                        "rawOutput": "Found node >= 22"
                    }}}}
                ],
                "stopReason": "end_turn"
            },
            {
                "steps": [
                    { "notify": { "params": { "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "\n⚠ UNAUTHENTICATED (code 401): token expired\n" }
                    }}}}
                ],
                "stopReason": "end_turn"
            }
        ]
    }));

    let mut cfg = rig.config();
    cfg.dialect = Arc::new(TestDialect { spec: rig.spec() });

    let mut session = LiveSession::start(
        &rig,
        Open::New {
            cwd: rig.cwd.clone(),
        },
        acp_capabilities(),
        cfg,
        None,
    )
    .await
    .unwrap();

    let cmds = session
        .until(|e| matches!(e, EventKind::CommandsUpdated { commands } if !commands.is_empty()))
        .await;
    assert!(cmds.iter().any(|e| matches!(
        e,
        EventKind::CommandsUpdated { commands }
            if commands.len() == 1 && commands[0].name == "plan"
    )));

    session.send_text("ask me").await;

    let q1 = session
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested {
        approval_id: aid1,
        request: ApprovalRequest::Questions { questions },
        ..
    } = q1.last().unwrap().clone()
    else {
        panic!("expected Questions for interaction_1: {q1:?}");
    };
    assert_eq!(questions[0].question, "Pick a framework");
    session
        .submit(Op::ApprovalResponse {
            approval_id: aid1,
            decision: ApprovalDecision::Accept {
                content: Some(json!({
                    "answers": { &questions[0].id: { "answers": ["Vitest"] } }
                })),
            },
        })
        .await;

    let q2 = session
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested {
        approval_id: aid2,
        request: ApprovalRequest::Questions { questions: qs2 },
        ..
    } = q2.last().unwrap().clone()
    else {
        panic!("expected Questions for form elicitation: {q2:?}");
    };
    assert_eq!(qs2.len(), 1);
    session
        .submit(Op::ApprovalResponse {
            approval_id: aid2,
            decision: ApprovalDecision::Accept {
                content: Some(json!({
                    "answers": { &qs2[0].id: { "answers": ["Deno custom"] } }
                })),
            },
        })
        .await;

    let turn1 = session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    let EventKind::TurnCompleted { turn } = turn1.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(turn.status, TurnStatus::Completed);
    let statuses: Vec<_> = turn
        .items
        .iter()
        .filter_map(SubagentStatus::from_item)
        .collect();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].state, SubagentState::Completed);
    assert_eq!(statuses[0].message.as_deref(), Some("Found node >= 22"));

    session.send_text("next").await;
    let turn2 = session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    assert!(turn2.iter().any(|e| matches!(
        e,
        EventKind::Error { message } if message == "Please run agy in terminal to sign in"
    )));
    let EventKind::TurnCompleted { turn: t2 } = turn2.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(t2.status, TurnStatus::Failed);
    assert!(
        !t2.items
            .iter()
            .any(|i| matches!(i, Item::Known(KnownItem::AssistantMessage { .. })))
    );

    let logs = rig.log_entries();
    let perm_reply = logs
        .iter()
        .find(|e| e["kind"] == "permission_reply")
        .unwrap();
    assert_eq!(
        perm_reply["reply"]["outcome"],
        json!({ "outcome": "selected", "optionId": "vitest" })
    );
    let elicit_reply = logs
        .iter()
        .find(|e| e["kind"] == "elicitation_reply")
        .unwrap();
    assert_eq!(
        elicit_reply["reply"],
        json!({ "action": "accept", "content": { "q0__other": "Deno custom" } })
    );

    session.close().await;
}

#[tokio::test]
async fn interrupt_sends_session_cancel_withdraws_pending_approval_and_completes_interrupted() {
    let rig = AcpRig::new();
    rig.write_scenario(&json!({
        "turns": [{
            "steps": [
                { "notify": { "params": { "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "Working..." }
                }}}},
                {
                    "permission": {
                        "toolCall": {
                            "toolCallId": "call-hang",
                            "kind": "execute",
                            "title": "sleep 60",
                            "rawInput": { "command": "sleep 60" }
                        },
                        "options": [
                            { "optionId": "allow", "name": "Allow", "kind": "allow_once" }
                        ]
                    }
                },
                { "waitCancel": true }
            ]
        }]
    }));

    let mut session = LiveSession::start(
        &rig,
        Open::New {
            cwd: rig.cwd.clone(),
        },
        acp_capabilities(),
        rig.config(),
        None,
    )
    .await
    .unwrap();

    session.send_text("long task").await;
    let asked = session
        .until(|e| matches!(e, EventKind::ApprovalRequested { .. }))
        .await;
    let EventKind::ApprovalRequested { approval_id, .. } = asked.last().unwrap().clone() else {
        unreachable!()
    };

    session
        .submit(Op::Interrupt {
            turn_id: TurnId("any".into()),
        })
        .await;

    let done = session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    assert!(done.iter().any(|e| matches!(
        e,
        EventKind::ApprovalResolved { approval_id: resolved } if resolved == &approval_id
    )));
    let EventKind::TurnCompleted { turn } = done.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(turn.status, TurnStatus::Interrupted);
    assert!(
        rig.inbound_methods()
            .iter()
            .any(|(m, _)| m == "session/cancel")
    );

    session.close().await;
}

#[tokio::test]
async fn resume_prefers_session_resume_falls_back_to_session_load_and_discards_replayed_history() {
    let rig = AcpRig::new();
    rig.write_scenario(&json!({
        "newSessionId": "acp-sess-42",
        "turns": [{
            "steps": [{
                "notify": { "params": { "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "Remembered amber-42." }
                }}}
            }],
            "stopReason": "end_turn"
        }]
    }));

    let session_path = {
        let mut s1 = LiveSession::start(
            &rig,
            Open::New {
                cwd: rig.cwd.clone(),
            },
            acp_capabilities(),
            rig.config(),
            None,
        )
        .await
        .unwrap();
        s1.send_text("Remember amber-42").await;
        s1.until(|e| matches!(e, EventKind::TurnCompleted { .. }))
            .await;
        let path = s1.opened.path.clone();
        s1.close().await;
        path
    };

    rig.write_scenario(&json!({
        "turns": []
    }));
    {
        let s2 = LiveSession::start(
            &rig,
            Open::Path(session_path.clone()),
            acp_capabilities(),
            rig.config(),
            None,
        )
        .await
        .unwrap();
        assert!(
            rig.inbound_methods().iter().any(
                |(m, msg)| m == "session/resume" && msg["params"]["sessionId"] == "acp-sess-42"
            )
        );
        s2.close().await;
    }

    let _ = std::fs::remove_file(&rig.log_path);
    rig.write_scenario(&json!({
        "initialize": {
            "protocolVersion": 1,
            "agentInfo": { "name": "fake-acp", "version": "1.5.13" },
            "agentCapabilities": {
                "loadSession": true,
                "promptCapabilities": { "image": true },
                "sessionCapabilities": {}
            }
        },
        "loadReplayBefore": [
            { "update": {
                "sessionUpdate": "user_message_chunk",
                "content": { "type": "text", "text": "Remember amber-42" }
            }},
            { "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "Remembered amber-42." }
            }},
            { "update": {
                "sessionUpdate": "available_commands_update",
                "availableCommands": [{ "name": "goal", "description": "Run until done" }]
            }}
        ],
        "loadReplayAfter": [
            { "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "old-call",
                "kind": "read",
                "title": "old read",
                "status": "completed"
            }}
        ],
        "turns": [{
            "steps": [{
                "notify": { "params": { "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "The code is amber-42." }
                }}}
            }],
            "stopReason": "end_turn"
        }]
    }));

    let mut s3 = LiveSession::start(
        &rig,
        Open::Path(session_path.clone()),
        acp_capabilities(),
        rig.config(),
        None,
    )
    .await
    .unwrap();
    let initial = s3
        .until(|e| matches!(e, EventKind::CommandsUpdated { commands } if !commands.is_empty()))
        .await;
    assert!(initial.iter().any(|e| matches!(
        e,
        EventKind::ThreadStarted { thread, .. } if thread.turns.len() == 1
    )));

    s3.send_text("What was the code?").await;
    s3.until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    s3.close().await;

    let store = SessionStore::open_read_only(&session_path, &CancellationToken::new()).unwrap();
    let turns = tiechui_runtime::turns(&store).len();
    assert_eq!(turns, 2);
}

#[tokio::test]
async fn resume_falls_back_to_session_new_on_unknown_session_but_fails_on_auth_timeout_or_exit() {
    let rig = AcpRig::new();
    rig.write_scenario(&json!({ "newSessionId": "acp-old" }));
    let session_path = {
        let s1 = LiveSession::start(
            &rig,
            Open::New {
                cwd: rig.cwd.clone(),
            },
            acp_capabilities(),
            rig.config(),
            None,
        )
        .await
        .unwrap();
        let path = s1.opened.path.clone();
        s1.close().await;
        path
    };

    rig.write_scenario(&json!({
        "resumeError": { "code": -32002, "message": "Session acp-old not found" },
        "newSessionId": "acp-replacement"
    }));
    {
        let mut s2 = LiveSession::start(
            &rig,
            Open::Path(session_path.clone()),
            acp_capabilities(),
            rig.config(),
            None,
        )
        .await
        .unwrap();
        let startup = s2
            .until(|e| {
                matches!(
                    e,
                    EventKind::Error { message } if message.contains("could not resume")
                )
            })
            .await;
        assert!(!startup.is_empty());
        s2.close().await;
    }

    rig.write_scenario(&json!({
        "resumeError": { "code": -32000, "message": "Authentication required: please sign in" }
    }));
    let auth_err = LiveSession::start(
        &rig,
        Open::Path(session_path.clone()),
        acp_capabilities(),
        rig.config(),
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(auth_err.contains("Authentication required"), "{auth_err}");

    rig.write_scenario(&json!({ "hangOnInitialize": true }));
    let timeout_err = LiveSession::start(
        &rig,
        Open::Path(session_path.clone()),
        acp_capabilities(),
        rig.config(),
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(timeout_err.contains("timed out"), "{timeout_err}");

    rig.write_scenario(&json!({ "exitOnInitialize": true }));
    let exit_err = LiveSession::start(
        &rig,
        Open::Path(session_path),
        acp_capabilities(),
        rig.config(),
        None,
    )
    .await
    .err()
    .unwrap();
    assert!(exit_err.contains("exited"), "{exit_err}");
    assert!(exit_err.contains("fatal boot error"), "{exit_err}");
}

#[tokio::test]
async fn post_end_turn_updates_open_a_derived_turn_and_close_on_idle_or_next_turn() {
    let rig = AcpRig::new();
    rig.write_scenario(&json!({
        "turns": [
            {
                "steps": [
                    { "notify": { "params": { "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": "bg-1",
                        "kind": "execute",
                        "title": "sleep 1 && echo DONE",
                        "status": "in_progress",
                        "rawInput": { "command": "sleep 1 && echo DONE" }
                    }}}},
                    { "notify": { "params": { "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "Started in background." }
                    }}}}
                ],
                "stopReason": "end_turn",
                "afterEndTurn": [
                    { "sleepMs": 30 },
                    { "notify": { "params": { "update": {
                        "sessionUpdate": "tool_call_update",
                        "toolCallId": "bg-1",
                        "status": "completed",
                        "rawOutput": { "stdout": "DONE\n", "exitCode": 0 }
                    }}}},
                    { "notify": { "params": { "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "Background task finished: DONE" }
                    }}}}
                ]
            },
            {
                "steps": [
                    { "notify": { "params": { "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "Second root turn reply." }
                    }}}}
                ],
                "stopReason": "end_turn"
            }
        ]
    }));

    let mut session = LiveSession::start(
        &rig,
        Open::New {
            cwd: rig.cwd.clone(),
        },
        acp_capabilities(),
        rig.config(),
        None,
    )
    .await
    .unwrap();

    session.send_text("start background").await;
    let first = session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    let EventKind::TurnCompleted { turn: root_turn } = first.last().unwrap().clone() else {
        unreachable!()
    };
    assert_eq!(root_turn.status, TurnStatus::Completed);

    let derived_events = session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    let EventKind::TurnCompleted { turn: derived_turn } = derived_events.last().unwrap() else {
        unreachable!()
    };
    let marker = DerivedTurnMarker::from_item(&derived_turn.items[0]).unwrap();
    assert_eq!(marker.derived_from, root_turn.id);
    assert_eq!(marker.reason, DerivedReason::Runtime);
    assert!(derived_turn.items.iter().any(|item| matches!(
        item,
        Item::Known(KnownItem::AssistantMessage { text, .. })
            if text == "Background task finished: DONE"
    )));

    session.send_text("follow up").await;
    let second = session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    let EventKind::TurnCompleted { turn: second_turn } = second.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(second_turn.status, TurnStatus::Completed);
    assert!(DerivedTurnMarker::from_item(&second_turn.items[0]).is_none());
    assert!(second_turn.items.iter().any(|item| matches!(
        item,
        Item::Known(KnownItem::AssistantMessage { text, .. }) if text == "Second root turn reply."
    )));

    assert_eq!(
        rig.log_entries()
            .into_iter()
            .filter(|e| e["kind"] == "inbound" && e["msg"]["method"] == "session/prompt")
            .count(),
        2
    );

    session.close().await;
}

#[tokio::test]
async fn starting_a_new_turn_during_an_open_wakeup_flushes_derived_text_before_closing_it() {
    let rig = AcpRig::new();
    rig.write_scenario(&json!({
        "turns": [
            {
                "steps": [{ "notify": { "params": { "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "First done." }
                }}}}],
                "stopReason": "end_turn",
                "afterEndTurn": [
                    { "sleepMs": 20 },
                    { "notify": { "params": { "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "Wakeup still open" }
                    }}}}
                ]
            },
            {
                "steps": [{ "notify": { "params": { "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "Next root reply." }
                }}}}],
                "stopReason": "end_turn"
            }
        ]
    }));

    let mut cfg = rig.config();
    cfg.timeouts.wakeup_idle = Duration::from_secs(10);
    let mut session = LiveSession::start(
        &rig,
        Open::New {
            cwd: rig.cwd.clone(),
        },
        acp_capabilities(),
        cfg,
        None,
    )
    .await
    .unwrap();

    session.send_text("first").await;
    session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;

    session
        .until(|e| matches!(e, EventKind::ItemDelta { delta, .. } if delta.contains("Wakeup")))
        .await;

    session.send_text("second").await;
    let derived_closed = session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    let EventKind::TurnCompleted { turn: derived } = derived_closed.last().unwrap() else {
        unreachable!()
    };
    assert!(DerivedTurnMarker::from_item(&derived.items[0]).is_some());
    assert!(derived.items.iter().any(|item| matches!(
        item,
        Item::Known(KnownItem::AssistantMessage { text, .. }) if text == "Wakeup still open"
    )));

    let second_closed = session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    let EventKind::TurnCompleted { turn: second } = second_closed.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(second.status, TurnStatus::Completed);
    assert!(second.items.iter().any(|item| matches!(
        item,
        Item::Known(KnownItem::AssistantMessage { text, .. }) if text == "Next root reply."
    )));

    session.close().await;
}

#[tokio::test]
async fn turns_record_git_snapshots_and_commands_and_set_model_flow_through_session() {
    let rig = AcpRig::new();
    rig.init_git();
    let hello = rig.cwd.join("src/hello.ts");
    std::fs::create_dir_all(hello.parent().unwrap()).unwrap();
    std::fs::write(&hello, "export const v = 'before';\n").unwrap();

    rig.write_scenario(&json!({
        "turns": [
            {
                "steps": [
                    {
                        "writeFile": {
                            "path": "src/hello.ts",
                            "content": "export const v = 'after';\n"
                        }
                    },
                    { "notify": { "params": { "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "Edited src/hello.ts" }
                    }}}}
                ],
                "stopReason": "end_turn"
            },
            {
                "steps": [
                    { "notify": { "params": { "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "Plan executed." }
                    }}}}
                ],
                "stopReason": "end_turn"
            }
        ]
    }));

    let mut session = LiveSession::start(
        &rig,
        Open::New {
            cwd: rig.cwd.clone(),
        },
        acp_capabilities(),
        rig.config(),
        Some("rules"),
    )
    .await
    .unwrap();
    session
        .until(|e| matches!(e, EventKind::ThreadStarted { .. }))
        .await;

    session.send_text("edit hello").await;
    let done = session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    let EventKind::TurnCompleted { turn } = done.last().unwrap().clone() else {
        unreachable!()
    };
    assert_eq!(
        std::fs::read_to_string(&hello).unwrap(),
        "export const v = 'after';\n"
    );

    let rewind =
        RewindStore::new(&rig.home.join("state/rewind"), &rig.cwd, &session.opened.id).unwrap();
    let start_snap = rewind.snapshot(&turn.id.0).unwrap();
    let end_snap = rewind
        .snapshot(&tiechui_module_rewind::end_anchor(&turn.id.0))
        .unwrap();
    let (
        Some(SnapshotRecord::Ready {
            tree: start_tree, ..
        }),
        Some(SnapshotRecord::Ready { tree: end_tree, .. }),
    ) = (start_snap, end_snap)
    else {
        panic!("expected Ready start and end snapshots");
    };
    assert_ne!(
        start_tree, end_tree,
        "turn modified src/hello.ts so trees must differ"
    );

    session
        .submit(Op::SetModel {
            provider: "acp".into(),
            model: "acp/gemini-2.5-flash".into(),
        })
        .await;
    let model_events = session
        .until(|e| matches!(e, EventKind::SessionUpdated { model, .. } if model.id == "gemini-2.5-flash"))
        .await;
    assert!(!model_events.is_empty());

    session
        .submit(Op::RunCommand {
            name: "permissions".into(),
            arguments: "auto".into(),
        })
        .await;
    let mode_events = session
        .until(|e| {
            matches!(
                e,
                EventKind::Extension { name, data }
                    if name == PERMISSION_MODE_EVENT && data == "auto"
            )
        })
        .await;
    assert!(!mode_events.is_empty());

    session
        .submit(Op::RunCommand {
            name: "plan".into(),
            arguments: "ship it".into(),
        })
        .await;
    let slash_done = session
        .until(|e| matches!(e, EventKind::TurnCompleted { .. }))
        .await;
    let EventKind::TurnCompleted { turn: slash_turn } = slash_done.last().unwrap() else {
        unreachable!()
    };
    assert_eq!(slash_turn.status, TurnStatus::Completed);

    let inbound = rig.inbound_methods();
    assert!(
        inbound
            .iter()
            .any(|(m, msg)| m == "session/set_model"
                && msg["params"]["modelId"] == "gemini-2.5-flash")
    );
    assert!(
        inbound
            .iter()
            .any(|(m, msg)| m == "session/set_mode" && msg["params"]["modeId"] == "accept-edits")
    );
    assert!(
        inbound.iter().any(|(m, msg)| m == "session/prompt"
            && msg["params"]["prompt"][0]["text"] == "/plan ship it")
    );

    session.close().await;
}
