import childProcess from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import readline from "node:readline";

const scenarioPath = process.env.FAKE_ACP_SCENARIO ?? process.env.FAKE_AGY_SCENARIO;
const logPath = process.env.FAKE_ACP_LOG ?? process.env.FAKE_AGY_LOG;
const scenario =
  scenarioPath && fs.existsSync(scenarioPath)
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

if (args.length > 0 && args[0] !== "acp") {
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

function askClient(method, params, explicitId) {
  const id = explicitId !== undefined ? String(explicitId) : `srv-${nextServerReqId++}`;
  return new Promise((resolve) => {
    pendingServerRequests.set(id, resolve);
    send({ jsonrpc: "2.0", id: explicitId ?? id, method, params });
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
    if (step.spawnChild) {
      const child = childProcess.spawn(
        process.execPath,
        ["-e", "setInterval(() => {}, 1000)"],
        { stdio: "ignore" }
      );
      logEntry({ kind: "grandchild", pid: child.pid });
    }
    if (step.crashCode !== undefined) {
      if (step.crashStderr) {
        process.stderr.write(`${step.crashStderr}\n`);
      }
      process.exit(step.crashCode);
    }
    if (step.writeFile) {
      const target = path.resolve(process.cwd(), step.writeFile.path);
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.writeFileSync(target, step.writeFile.content, "utf8");
    }
    if (step.rawFrame) {
      send(step.rawFrame);
    }
    if (step.notify) {
      send({
        jsonrpc: "2.0",
        method: step.notify.method ?? "session/update",
        params: { sessionId, ...step.notify.params },
      });
    }
    if (step.permission) {
      const { id: explicitId, ...rest } = step.permission;
      const res = await askClient(
        "session/request_permission",
        { sessionId, ...rest },
        explicitId
      );
      logEntry({ kind: "permission_reply", reply: res });
      if (step.echoDecision) {
        const prefix = step.echoPrefix ?? "decision=";
        send({
          jsonrpc: "2.0",
          method: "session/update",
          params: {
            sessionId,
            update: {
              sessionUpdate: "agent_message_chunk",
              content: { type: "text", text: `${prefix}${JSON.stringify(res.outcome)}` },
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
        const prefix = step.echoPrefix ?? "elicitation=";
        send({
          jsonrpc: "2.0",
          method: "session/update",
          params: {
            sessionId,
            update: {
              sessionUpdate: "agent_message_chunk",
              content: { type: "text", text: `${prefix}${JSON.stringify(res)}` },
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

const isAgyCli = args[0] === "acp";

const defaultModels = isAgyCli
  ? {
      currentModelId: "gemini-4-neon-high",
      availableModels: [
        { modelId: "gemini-4-neon-high", name: "Gemini 4 Neon (High)" },
        { modelId: "gemini-3.8-flash-low", name: "Gemini 3.8 Flash (Low)" },
        { modelId: "claude-opus-5-5-high", name: "Claude Opus 5.5 (High)" },
      ],
    }
  : {
      currentModelId: "gemini-2.5-pro",
      availableModels: [
        { modelId: "gemini-2.5-pro", name: "Gemini 2.5 Pro" },
        { modelId: "gemini-2.5-flash", name: "Gemini 2.5 Flash" },
      ],
    };

const defaultModes = isAgyCli
  ? {
      currentModeId: "default",
      availableModes: [
        { id: "default", name: "Default", description: "Ask before risky actions." },
        { id: "accept-edits", name: "Accept Edits", description: "Auto-approve file edits." },
        { id: "auto", name: "Auto", description: "Auto-approve actions." },
        { id: "plan", name: "Plan", description: "Read-only planning." },
      ],
    }
  : {
      currentModeId: "default",
      availableModes: [
        { id: "default", name: "Default" },
        { id: "accept-edits", name: "Accept edits" },
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
    if (scenario.exitOnInitialize) {
      console.error("fatal boot error");
      process.exit(1);
    }
    if (scenario.hangOnInitialize) {
      return;
    }
    if (scenario.initializeError) {
      send({ jsonrpc: "2.0", id: msg.id, error: scenario.initializeError });
      return;
    }
    send({
      jsonrpc: "2.0",
      id: msg.id,
      result: scenario.initialize ?? {
        protocolVersion: 1,
        agentInfo: {
          name: isAgyCli ? "Antigravity ACP Server" : "fake-acp",
          title: isAgyCli ? undefined : "Fake ACP",
          version: scenario.agentVersion ?? scenario.cliVersion ?? (isAgyCli ? "1.5.14" : "1.5.13"),
        },
        agentCapabilities: scenario.agentCapabilities ?? {
          loadSession: true,
          promptCapabilities: { image: true, audio: false, embeddedContext: true },
          sessionCapabilities: { resume: {}, close: {}, load: {}, list: {} },
          ...(isAgyCli ? { auth: { logout: {} } } : {}),
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
    const sessionId =
      scenario.newSessionId ??
      scenario.sessionId ??
      (isAgyCli ? "0bae8452-4bec-4a00-b5c0-ec54df96624c" : "acp-sess-new");
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
      result: scenario.newResult ?? {
        sessionId,
        models: scenario.omitSessionModels ? undefined : (scenario.models ?? defaultModels),
        modes: scenario.modes ?? defaultModes,
      },
    });
    for (const raw of scenario.afterNewRaw ?? []) {
      send(raw);
    }
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
    if (isAgyCli) {
      send({
        jsonrpc: "2.0",
        method: "session/update",
        params: {
          sessionId: msg.params.sessionId,
          update: { sessionUpdate: "current_mode_update", currentModeId: msg.params.modeId },
        },
      });
    }
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
      ? Number(
          fs.existsSync(scenarioPath + ".turn")
            ? fs.readFileSync(scenarioPath + ".turn", "utf8")
            : "0"
        )
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
        ...(turnSpec._meta ? { _meta: turnSpec._meta } : {}),
      },
    });
    if (turnSpec.afterEndTurn && stopReason === "end_turn") {
      void (async () => {
        await runSteps(turnSpec.afterEndTurn, sessionId, null);
      })();
    }
  }
});
