# Subagents

Enabled by default; `[modules] subagents = false` removes the tool, model catalog
context, and viewer. `subagent` accepts exactly one of `task` (with optional
`agent`, `cwd`, `model`, `thinking`), `tasks` (at most eight), or `chain`
(substituting the complete final assistant text for `{previous}` and marking
remaining steps Skipped after failure), plus optional `agentScope` (`user`,
`project`, or `both`). `subagent` runs with `Concurrency::Shared` and a
session-wide semaphore of eight slots shared across parallel batches and
concurrent tool calls; extra tasks queue without failing. Model text never
determines a terminal state.

There are no built-in role templates. Omitting `agent` runs the unnamed `default`
general-purpose subagent with the base coding assistant prompt, all child tools
enabled, and `ChildRegistry::build_default` (ignoring any `default.md` on disk).
Passing an explicit `agent` looks up Markdown definitions in `~/.tiechui/agents`
and the nearest ancestor `.agents/agents` and `.claude/agents` with YAML
frontmatter: `name`, `description`, optional `tools` (comma-separated or a YAML
sequence), optional `model` (`<provider>/<id>` or `inherit`), and optional
`thinking` (`off`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, or
`inherit`). User definitions win by name, and unknown names fail preflight with
the available custom agents in scope. Each dispatch reads definitions afresh;
`definitions::render` rewrites `name`, `description`, `tools`, `model`, `thinking`
and prompt while keeping other frontmatter and comments for `agents/save`.
Untrusted project definitions require explicit confirmation through the parent's
interaction queue, including when a model supplies an unknown confirmation-bypass
parameter; headless execution refuses them. Confirmation authorizes the
definitions, not new project policy sources.

Model overrides resolve in order `task.model > agent.model > parent`, and thinking
overrides resolve in order `task.thinking > agent.thinking > parent requested
thinking`, where `inherit` forces fallback to the parent session. `ChildRunner`
validates that the target `<provider>/<id>` is loaded and ready
(`context_window > 0`) before creating a child session, clamping the requested
thinking level to the target model's supported levels. Parent sessions receive a
`<subagent_models>` system prompt fragment listing the active `<provider>/<id>`
and any ready candidate models with their thinking levels; child sessions omit
this fragment, and no `providers.toml` paths or credentials are read. Child
requests cap the target model's prompt cache at `short`: `long` (one hour) becomes
the five-minute TTL, and `none` stays uncached.
`ChildRegistry` builds a fresh registry per task with editing (including the
native child `ls` tool), foreground shell execution, tool search, PTC, fresh
permissions, parent-anchored rewind tracking, and existing MCP clients.
Allowlisted definitions restrict both advertised tools and actual dispatch. No
parent hooks, skills, goals, compaction, plugins, `ask`, or nested `subagent`
tool are inherited. Child sessions are private temporary SessionActors, removed
only after actor settlement. MCP connection ownership remains with the parent.

Children use the parent's loaded policy and trust, including user and trusted
project agent `permission` frontmatter. Their `cwd` must resolve inside the
parent workspace; changing it never reloads policy. Each child has fresh session
grants. Approval prompts identify the child and scope grants to that child;
all requests use the parent's serialized queue. This is policy enforcement,
not OS containment for arbitrary programs.

Parent cancellation and viewer Ctrl+X stop the entire dispatch, start no further
actors, await active tool and actor cleanup, and retain finished results. Actor
task boundaries isolate provider panics; `catch_unwind` around tool execution
still runs settlement, releasing rewind writer leases after partial writes.
Rewind uses the parent's user/control entry and storage identity. Navigation
waits for the active turn, so restore cannot race live children. Epoch/session
checks reject stale publication. Per-task actual usage is reported through
`runtime.usage`; goal counts input + cache writes + output once, excluding cache
reads.

`AgentTask.model` records the full `<provider>/<id>`, and parallel or incomplete
dispatch summaries format headers as `### [<agent> · <provider>/<id>] <State>`
(falling back to `### [<agent>] <State>` before a session model event arrives).
The main transcript shows Waiting / Finished waiting and task results with model
attribution. `/subagents` or Alt+Left/Right with an empty composer opens the
viewer. Arrows select Main or a task, Enter views it, Ctrl+F searches (including
by model), Ctrl+S groups states, Ctrl+T shows thinking, Ctrl+X cancels the
dispatch, and Esc returns. Wide screens preview beside the list; narrow screens
use one column. Pending approvals wait behind the viewer and are shown on return,
using the same foreground ownership as context and BTW. Branch records restore on
resume and navigation.

Live previews and parallel model summaries are grapheme-safe and retain at most
50 KiB of each output/thinking stream. Overflow is written to mode-0600 files under `state/subagents/output`;
`read` can retrieve their paths from results. Each archive is capped at 64 MiB;
live authoritative content is capped at 8 MiB / 256 blocks and cancels with an
explicit diagnostic on overflow. Completed output files are retained for resume;
there is no automatic archive cleanup. Chains read the complete final output
from the archive. Successful records retain the full final text and thinking
within the live-content bound for expanded views, plus usage and archive paths.

Tests cover modes and limits, default general-purpose dispatch and custom agent
discovery, cross-provider model and thinking precedence with preflight errors,
parent-only model catalog injection, 8-slot session-wide queueing and
cancellation, chain substitution/skip, trust and headless refusal, private grants
and parent cwd policy, real child edits and rewind, actor/tool panic isolation,
stale completion, output spill, record restoration and goal usage. TUI snapshots
cover 40/80 columns and both palettes. The offline PTY case exercises parallel
children, forwarded edit approval, Alt+Right, dispatch cancellation and retained
finished output.
