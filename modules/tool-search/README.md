# Tool search

Enabled by default; set `[modules] tool-search = false` to disable it. MCP and
plugin tool definitions are deferred when their estimated size exceeds 10% of
the model window (20,000 tokens when the window is unknown). Built-in tools and
MCP resource tools stay visible. Registries that exclude either discovery tool
through the allowed-tools filter retain their direct tool definitions.

`tool_search` lists the deferred catalog by source in its description. Exact
`names` retrieve up to 20 definitions; `query` searches names, descriptions and
parameters with local BM25, ASCII word splitting and CJK bigrams. Results include
complete descriptions and compact JSON schemas. Query `limit` defaults to 5 and
is capped at 10. Catalogs above 12 KiB show source counts instead of names.
Large results use the common output truncation and full-output file mechanism.

`call_tool` takes an exact `name` and optional object `arguments`. The child call
uses the normal validation, coercion, permission, hook, scheduling and settlement
path. Approval names the real tool. Updates retain the wrapper's event ID, and
the TUI renders the inner tool. Validation errors include its parameter schema.
Other result fields pass through unchanged.

Searching and invoking never add definitions to the request's tools array.
Presentation depends only on the current callable catalog and model window.
Each registry owns its snapshot; no discovery history or session entries are
written. The same module registration serves interactive sessions, `chui exec`,
subagents and side chats.

MCP calls use exclusive scheduling until read-only classification is available;
the wrapper itself holds no scheduler lock. Large-catalog status refreshes
precompute names so periodic resource updates do not block tool execution.

Verification covers threshold boundaries, unknown windows, catalog ordering and
size fallback, English/CJK/parameter search, complete definitions, invalid input
and output truncation, isolated registration, allowed-tool filtering, invocation
identity, updates and cancellation. Core tests use the fake MCP server to check
1,000 tools across repeated search/call rounds and both provider HTTP formats,
projection and context accounting, directory changes, strict approvals, denials,
validation and coercion, hooks, settlement, and edit/MCP exclusion. TUI tests
compare direct and delegated cards and fold controls. Run `just check` for the
workspace gates.
