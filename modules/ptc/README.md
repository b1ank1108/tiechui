# Programmatic tool calls

The module adds `run_code` and is enabled by default; set `[modules] ptc = false` to remove it. It leaves the other tool definitions visible.

`code` is an async JavaScript function body (up to 64 KiB UTF-8). `description` optionally supplies the activity title. `timeout` optionally limits wall-clock time to 1–3600 seconds. Return a string or JSON value; `console.log` captures additional text.

```javascript
const paths = ["src/a.rs", "src/b.rs"];
return await Promise.all(paths.map(async path => {
  const text = await tools.read({path});
  return {path, todos: text.split("\n").filter(line => line.includes("TODO"))};
}));
```

File, search, shell and MCP tools are callable by their real names, including deferred MCP tools. Each call uses the host's validation, permissions, hooks, scheduler and settlement. Errors reject with an `Error` carrying a `tool` property; caught failures still appear in the final summary. Control tools and plugin tools without programmatic support are rejected.

Each run uses a fresh QuickJS runtime on a dedicated thread, with a 64 MiB heap, 1 MiB JS stack and 30 seconds of JavaScript execution time. Waiting for tools or approval does not consume that time budget. There is no filesystem, network, module loader, `require`, or timer API. `rquickjs` embeds the engine without an external runtime; building its C sources requires a C compiler.

At most 200 child calls are accepted, with four in flight; the host serializes writes and other exclusive calls. Child text is limited to 1 MiB, including image placeholders; an oversized result reports that execution already happened. Console capture is limited to 16 KiB. Final output uses the shared output truncation and full-output file mechanism. `ProgramRun` details retain child identities, bounded argument strings, states and errors for live display and replay.

Cancellation, timeout, resource exhaustion and program completion cancel remaining calls and wait for settlement. Completed side effects remain in place, and persisted programs are never rerun on recovery.
