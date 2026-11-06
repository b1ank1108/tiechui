#!/usr/bin/env node
"use strict";
// Runs the native build that the matching optional dependency installed.
const { spawn } = require("child_process");
const path = require("path");

const PLATFORMS = ["darwin-arm64", "darwin-x64", "linux-arm64", "linux-x64", "win32-x64"];
const platform = `${process.platform}-${process.arch}`;

let binary;
try {
  const root = path.dirname(require.resolve(`@tiechui/cli-${platform}/package.json`));
  binary = path.join(root, process.platform === "win32" ? "tc.exe" : "tc");
} catch {
  console.error(
    PLATFORMS.includes(platform)
      ? `tc: the ${platform} build is missing; optional dependencies were probably skipped. Reinstall with one of:\n` +
          "  npm i -g @tiechui/cli\n  pnpm add -g @tiechui/cli\n  yarn global add @tiechui/cli\n  bun add -g @tiechui/cli"
      : `tc: no build for ${platform}; supported: ${PLATFORMS.join(", ")}`,
  );
  process.exit(1);
}

const child = spawn(binary, process.argv.slice(2), {
  stdio: "inherit",
  env: { ...process.env, TIECHUI_INSTALL: "npm" },
});
child.on("error", (error) => {
  console.error(`tc: ${binary}: ${error.message}`);
  process.exit(1);
});
// The terminal already sends Ctrl+C to tc, which reads a second one as "exit now".
process.on("SIGINT", () => {});
for (const signal of ["SIGTERM", "SIGHUP"]) {
  process.on(signal, () => child.kill(signal));
}
child.on("exit", (code, signal) => {
  if (signal) {
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
  } else {
    process.exit(code ?? 1);
  }
});
