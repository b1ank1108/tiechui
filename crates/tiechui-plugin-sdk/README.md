# Rust plugin SDK

Implement `Plugin` and pass it to `stdio(plugin, CancellationToken::new()).await`
from a Tokio runtime. Use `serve(reader, writer, plugin, cancel)` for in-memory
tests. See the [complete example](../../../tiechui-plugins/plugins/sdk-rust).

The SDK validates and echoes `initialize`, then serves the manifest's tool and
command lists. An optional `initialize` handler can reject unsupported
contributions. Implement `tool`, `command`, `before` and `after` for the declared
capabilities. Tool arguments and the host's after-interception output are JSON;
results use `PluginOutput`, `PluginVerdict`, or `Option<PluginOutput>` (where
`None` leaves the output unchanged). Returning `error(code, message)` produces a
JSON-RPC error. Missing handlers fail closed.

Every handler receives a request-scoped `Context` (initialization receives a
`CancellationToken`). Check `context.cancel` during work and pass it to cancellable
operations. The runtime drops cancelled handler futures; avoid detached work
that outlives the request. Use `context.progress(number, message)` to report tool
progress with the host's token. Progress does not extend interceptor deadlines.

`context.notify` shows a transient notice and returns `()` after the host's
immediate `null` response, without waiting for dismissal. Headless sessions
write the notice to stderr. `select`, `confirm`, `input`, and `panel` return the host's
`ApprovalDecision`; inspect accept, decline and cancel explicitly. `status(text)`
sets a status item, and an empty string removes it. These helpers await the host
and propagate cancellation to pending UI. `request` exposes raw protocol calls
for advanced use; it does not grant capabilities. Declare the corresponding
capabilities in `.tiechui-plugin/plugin.json`.

Stdout is reserved for protocol frames. Log to stderr. Frames over 1 MiB or
invalid envelopes close the transport, and disconnection cancels handlers and
pending UI without replay. Local installation copies a package, so build and
copy its executable into that package before `tc plugin add`.
