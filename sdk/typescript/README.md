# @tiechui/plugin-sdk

The SDK has no npm dependencies or build step. Run the
[TypeScript example](../../../tiechui-plugins/plugins/sdk-typescript) with Node 25 and its
built-in type stripping. The runtime is JavaScript so the package also works
from `node_modules`, where Node does not strip TypeScript. `index.d.ts` exposes
typed handlers; `protocol.ts` is generated from Rust with ts-rs.

```ts
import { serve } from "./sdk/index.js";
import type { Plugin } from "./sdk/index.d.ts";

const plugin: Plugin = {
  async tool(name, arguments_, context) {
    await context.progress(1, "Ready");
    return { text: "Hello", isError: false };
  },
};
await serve(plugin, { signal: new AbortController().signal });
```

Declare the tool in `.tiechui-plugin/plugin.json`. `serve` validates and echoes
initialization and serves the manifest's lists. Optional handlers are
`initialize(declaration, signal)`, `tool(name, arguments, context)`,
`command(name, arguments, context)`, `before(stage, call, context)` and
`after(stage, call, output, context)`. The before result is a `PluginVerdict`;
after returns a `PluginOutput` or `null` to leave output unchanged. Missing
handlers fail closed. Throw `RpcError(code, message)` for a protocol error;
other exceptions become execution errors.

Use `context.signal` with cancellable work. Cancellation settles the request
promptly and ignores late handler results; JavaScript handlers must observe the
signal to stop their own work. `context.progress(number, message)` uses the
request's progress token. It does not extend interceptor deadlines.

`context.notify` shows a transient notice and resolves with no value after the
host's immediate `null` response, without waiting for dismissal. Headless sessions
write the notice to stderr. `select`, `confirm`, `input`, and `panel` await an
`ApprovalDecision`; handle accept, decline and cancel. `status(text)` sets a
status item (empty removes it). Each helper requires its manifest capability.
Nested UI requests share the handler's cancellation signal. `context.request`
is a raw escape hatch; the host still checks the method and capability.

Stdout carries only protocol traffic; use `console.error` for diagnostics.
`serve` accepts `input` and `output` streams for offline testing. It rejects on
malformed envelopes, invalid UTF-8, oversized frames or transport failure and
cancels pending work without replay. The SDK adds no permissions or host APIs.

Regenerate types with `cargo run -p xtask -- typescript`. `just check` checks
type drift and runs `node --test sdk/typescript/tests/*.test.ts` (with a clear
skip only if Node is missing). The shared transcript in
`tests/plugins/transcripts/sdk.jsonl` records sanitized bidirectional exchanges
and is replayed through both SDKs. No package is published by these commands.
