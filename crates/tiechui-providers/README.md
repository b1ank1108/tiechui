# tiechui-providers

Neutral model requests and cancellable provider streams. The crate implements
the `anthropic-messages`, `openai-completions` and `openai-responses` wire formats using reqwest,
rustls, and eventsource-stream.

`from_config(name, entry)` resolves the key from `entry.api_key` or `entry.api_key_env`. Requests use qualified
model IDs (`name/model-id`); the wire request uses the configured model ID. Token
limits and capabilities come from the entry's model list. `base_url` is the API
root: Anthropic defaults to `https://api.anthropic.com` and appends `/messages`;
OpenAI defaults to `https://api.openai.com` and appends `/chat/completions`, or
`/responses` for Responses.
A root without a version segment (`v` and a digit, such as `v1`, `v4` or
`v1beta`) gets `/v1` first, so `https://gateway.example` and
`https://gateway.example/v1` reach the same endpoint while `/api/paas/v4` keeps
its own version. All preserve any proxy path prefix. Redirects and automatic HTTP retries are disabled. TLS
uses native trust roots plus the `[network]` `ca_cert` file; `[network]` `proxy`
replaces the `*_PROXY` environment variables. A 300-second read timeout bounds silent connections.

The Anthropic stream emits deltas in arrival order and an authoritative `ContentComplete`
per known block, including opaque thinking signatures and redacted payloads.
Redacted thinking has empty neutral text and replays as `redacted_thinking`.
Unknown blocks and their deltas/stops, and unknown delta types, are skipped.
A delta for an index never started is still an error. Initial `fallback` blocks
are skipped; fallback after any recognized content block starts is an error.
Cumulative `Usage` events are emitted as soon as wire usage arrives, including
usage accompanying an error. `Finish` repeats the final snapshot.
Usage counts are separate:
uncached input, output, cache reads, cache writes. Reasoning is a subset of output;
one-hour cache writes are a subset of cache writes. Later usage fields replace only
the counts actually supplied. Missing initial usage defaults to zero for proxy
compatibility.
`Finish` requires `message_stop`, completed known blocks,
and a recognized stop reason. Cancellation produces one `Cancelled` error and
ends the stream; EOF or malformed/incomplete data produces a stream error.

## Request behavior

`ModelRequest.reasoning` accepts the `minimal`, `low`, `medium`, `high`, `xhigh`,
and `max` levels. Model configuration `thinking` defaults to `budget`; `adaptive`
always requests adaptive thinking. Adaptive thinking uses
summarized display and efforts `low` for minimal/low, `medium` for medium, and
`high` for high/xhigh/max. It sends no interleaved-thinking beta. Budget thinking
uses 1024/2048/8192/16384 tokens (xhigh/max use high), adds the budget to an
explicit output cap within the model ceiling, and reserves 1024 answer tokens,
falling back to a 1024-token budget when the fitted budget is zero.
It retains the interleaved-thinking beta. Absent reasoning explicitly disables
thinking on reasoning models unless `thinking_levels.off = false`;
non-reasoning models reject a reasoning level.

The optional model `thinking_levels` table maps `off`, `minimal`, `low`, `medium`,
`high`, `xhigh`, and `max` to wire strings or `false` (unsupported). Missing keys
retain built-in mappings. All wires reject explicitly unsupported requested
levels as `InvalidRequest` instead of clamping them. Adaptive
Anthropic efforts use mapped strings before the default mapping; budget token
counts retain their defaults. For always-thinking models, `off = false` means
an absent reasoning request leaves the thinking control to the server.

Default short-lived prompt caching places one breakpoint on the
collapsed system prompt, one on the last tool definition, and one on the final
user message's last block. Consecutive tool results form one user message. An
assistant prefill receives no history breakpoint, and the encoder does not scan
backward to an earlier user message. Thinking signatures are replayed verbatim,
including when their associated thinking text is empty.

Error classification uses HTTP status and structured error type or context-overflow
code. Quota/billing exhaustion markers in the error type, code or message
are terminal even on HTTP 429. Native context overflow shares the generic
`invalid_request_error` type, so HTTP 400 with that type and a message containing
`prompt is too long` or `prompt too long` (case insensitive) is also classified
as overflow. The status/type guard avoids classifying unrelated messages by
wording. Retry hints support `retry-after-ms`, seconds, and
HTTP dates, with milliseconds taking precedence. No sleeping or retrying occurs.
String-valued `error` fields are accepted; bodies without a usable message use
`HTTP <status>`.

Gateway errors are classified as ambiguous only for HTTP 400 (or a structured
stream error), with both `type: upstream_error` and `code: upstream_400`, an empty
or null `param`, and exactly the generic `Request contains an invalid argument`
message (optional `400 ·`/`400:` prefix and final period). The structured envelope
is limited to 16 KiB. Named parameters, malformed bodies and text-only matches
are excluded; the actor owns the single recovery attempt and shared retry budget.

All wires clamp output to the context window minus estimated input and a
4096-token safety reserve, with a one-token minimum. The actor supplies
`ModelRequest.input_tokens`: the latest successful assistant's non-zero usage
(input + output + cache read + cache write, the same counters shown by the context
battery), plus estimates for messages after it. Failed/aborted assistants cannot
be anchors. With no valid usage, the actor estimates the whole request, including
system text and tool definitions. Direct adapter calls without an estimate use
that same fallback. Trailing messages and the fallback use a 3.5 UTF-16
characters/token and 4800 characters/image heuristic.
Anthropic budget thinking is fitted again after clamping, preserving 1024 answer
tokens where possible and the 1024-token fallback budget when no budget fits.

Anthropic models may set `fallback_models = ["other-model-id"]`, referring to
distinct other models configured under the same provider. This sends a
`fallbacks` payload and the `server-side-fallback-2026-07-01` beta. A returned model is
reported before usage; the actor records it on the assistant and each usage entry,
including failed/interrupted streams, without changing the selected session model.
Configured prices follow the returned model. The OpenAI wires reject fallback configuration.

Model `cache_retention = "none" | "short" | "long"` controls Anthropic breakpoints;
the default is `short`, and `long` sends a one-hour TTL. Set `long` only on an
endpoint that accepts it. Requests marked `short_cache`, such as subagent turns,
send `long` as `short`. The OpenAI wires send no cache controls or cache keys, so
this setting leaves its payload unchanged and cannot disable server-managed caching.

Optional model `prices = { input = 2.0, output = 10.0, cache_read = 0.2,
cache_write = 2.5 }` supplies USD per million tokens (illustrative rates only).
All four rates must be finite and non-negative. One-hour writes use twice the
configured input rate; remaining writes use `cache_write`. Reasoning
is already included in output and is not charged twice. Cost is absent without
configured prices, including for unconfigured fallback identities. Session usage
stores estimated USD in billionths (`cost_nano_usd`), rounded once per cumulative
snapshot; resume retains those estimates without repricing old entries. The TUI's
optional `estimated-thread-cost` item shows the sum of available estimates.

Session message entries retain the requested reasoning level through reload;
absence means off or legacy metadata unavailable. All configured wires change
request-level effort on a switch. Historical metadata does not alter wire
payloads.

## OpenAI-compatible completions

Provider and model `compat` tables accept exactly these snake_case fields.
There is no provider-name or URL detection.

| Field | Default | Effect |
| --- | --- | --- |
| `supports_store` | `true` | Send `store: false`. |
| `supports_developer_role` | `true` | Use `developer` for a reasoning model's system prompt. |
| `supports_strict_mode` | `false` | Include `strict: false` on ordinary function tools. |
| `supports_reasoning_effort` | `true` | Send the mapped or default effort. |
| `supports_usage_in_streaming` | `true` | Request a trailing usage chunk. |
| `max_tokens_field` | `"max_completion_tokens"` | Choose this field or `"max_tokens"`. |
| `thinking_format` | `"openai"` | Choose OpenAI effort or DeepSeek thinking toggle plus effort. |
| `requires_reasoning_content_on_assistant_messages` | `false` | Replay thinking text, or an empty string when absent, on assistant messages. |

Configuration rejects unknown fields and invalid types/enum values. Fields
omitted from a provider table use the defaults above. Each field in a model's
optional table replaces the provider's value for that model; omitted fields keep
the provider's values. This allows DeepSeek and OpenAI thinking formats and
different instruction roles on one gateway. Providers, and with them compat, are
defined only in the user's `providers.toml`.

OpenAI format sends `reasoning_effort` for a requested level, and only sends an
off effort when the map explicitly supplies an off string. DeepSeek format sends
`thinking.type: enabled` with a requested level and independently sends effort
when supported. With no level, it sends `thinking.type: disabled` unless off is
unsupported; it does not send an off effort. Neither format changes output caps
to add a thinking budget.

User images use data URLs. Assistant text is a plain string; tool arguments are
JSON strings. Tool results are text, with placeholders for empty/image-only
results; consecutive tool-result images are hoisted into one following user
message, including error-result images. Models without image support reject
images. History containing tool calls sends `tools: []` when no tools are supplied.
System messages fold into the initial prompt. Calls/results must pair correctly.
The session replay transform removes failed/aborted assistants, closes missing
results, drops orphan results and moves Anthropic error images after all results.
Adapters continue to reject malformed histories passed directly to them.

Tool deltas resolve by server index first, then by id when no indexed block is
found. Indices are optional; a call initially identified by id can acquire an
index later. When different ids share an index, the first id/name remain
authoritative and later ids point to the same block.
Neutral indices follow first arrival, not the server's tool index.
If identity arrives late, argument events
wait until `ToolCallStart` can carry a complete id and name. Completed calls must
have unique identities and object arguments. Reasoning uses the first non-empty
field among `reasoning_content`, `reasoning`, and `reasoning_text` per delta,
before text from the same chunk. The original field name stays in opaque replay
data. Stream decoding records the last non-empty
`extra_content.google.thought_signature` from assistant text deltas (including
empty-content chunks) and tool-call deltas as
`{"google":{"thought_signature":"..."}}` in block `opaque` metadata without
synthesizing an empty text block. Request encoding replays valid signature
`opaque` metadata on `assistant.extra_content` and `tool_calls[i].extra_content`,
and leaves unsigned messages unchanged. All blocks complete in first-arrival
order at `[DONE]` or EOF.

A recognized, non-empty `finish_reason` is required; empty strings are absent.
A missing `[DONE]` is accepted at clean
EOF after a finish reason; a finish reason alone does not end a still-open socket
because a usage-only chunk may follow. Empty-choice usage chunks, choice-level
usage fallback, null deltas/tool-call lists, and absent usage are accepted.
Cache reads use this precedence: nested cached tokens, DeepSeek cache hits, then
top-level cached tokens. Cache writes are separate, and both subtract from prompt
tokens with saturation. Completion tokens already include reasoning tokens.
HTTP 200 JSON error bodies and non-null SSE error envelopes use the shared classifier;
HTTP error bodies are bounded at 64 KiB, and exposed messages at 2048 characters.

## OpenAI Responses

`openai-responses` posts to `/responses` with `stream: true` and `max_output_tokens`.
Of the compat fields it reads `supports_store` (send `store: false`),
`supports_developer_role` and `supports_reasoning_effort`; tools always carry
`strict: false`, since Responses tools are otherwise strict. System messages fold
into one leading `developer` (reasoning models) or `system` message. User content
is `input_text`/`input_image`; assistant text replays as a string message, tool
calls as `function_call` items and results as `function_call_output`, whose output
is the text or, with images, `input_text` and `input_image` parts. Reasoning models
always send `reasoning.summary: auto` and include `reasoning.encrypted_content`;
the effort follows the completions OpenAI format.

The stream keeps no server state. A reasoning item with encrypted content is
stored, without its `id` and `status`, as the thinking block's opaque data and
replayed verbatim; reasoning without it is not replayed. No item ids are sent, so
tool calls are identified by `call_id` alone. Summary parts read as paragraphs,
and raw `reasoning_text` deltas are accepted too. Neutral indices follow the
first arrival of message, reasoning and function-call items; other item types
are skipped. `response.output_item.done` is authoritative and also starts an item
never announced. `response.completed` ends the stream (tool use when any call
arrived), `response.incomplete` with `max_output_tokens` is a length stop and
with `content_filter` an `InvalidRequest`; `response.failed` and `error` events
use the shared classifier. Usage reads input, cached and gateway cache-write
input tokens, output and reasoning tokens. EOF before a terminal event is a
stream error.

## Limitations

- Authentication uses configured API keys only; there is no OAuth or
  federated/managed-provider authentication.
- Only the three HTTP wire formats above are supported; there is no Responses
  WebSocket mode, `previous_response_id` chaining or built-in tools.
- Adapters never retry. The session actor owns retries, backoff and
  overflow/length recovery through compaction.
- Incomplete tool arguments are not repaired; invalid arguments never become
  executable calls.
- There is no section-aware system caching, mid-conversation system message,
  dynamic tool set, or strict-schema negotiation and pruning.
- There are no custom thinking budgets, sampling options, tool choice, custom
  headers, service tiers or session affinity.
- Anthropic mid-conversation effort markers and thinking `block_binding` are not
  sent, although each message's reasoning level is persisted.
- There are no vendor presets or other thinking formats; configure the eight
  compat fields explicitly.
- Aside from `extra_content.google.thought_signature` on `openai-completions`,
  other `extra_content` fields, encrypted or structured `reasoning_details`, and
  custom/grammar tools are not supported; unrecognized `opaque` payloads are
  rejected.
- Prices are flat per model; there is no tiered pricing.

## Verification

All fixtures in `tests/fixtures/anthropic`, `tests/fixtures/openai-completions` and
`tests/fixtures/openai-responses`
are handwritten and sanitized.
Tests serve them through loopback wiremock servers and assert exact event
sequences. Four insta snapshots cover first-turn, tool-result, signed thinking,
and redacted thinking replay requests. Captured requests check all six reasoning
levels, disabled thinking, and beta-header selection. A separate loopback socket
server holds the connection open to test cancellation within 100 ms, an injected
short read timeout before headers and during the body, and termination without EOF.
Three OpenAI request snapshots cover first turn, tool-result images, and streamed
reasoning replay on a later tool turn. Exact event and round-trip tests cover
text, reasoning, interleaved tools, `thought_signature` replay, errors,
truncation, usage variants and missing `[DONE]`.
Two Responses request snapshots cover the first turn and encrypted reasoning,
tool-result image and assistant replay; event tests cover summaries, calls
completed without deltas, incomplete, failed and error events and truncation.
A shared suite sends identical neutral conversations through all three wires and
asserts identical content/final usage, cancellation, read-timeout and error-body
behavior; partial usage timing follows each wire.
Factory tests use a
subprocess with a cleared environment and a fake key. No live API or real
credential is used.
