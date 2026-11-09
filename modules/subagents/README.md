# Subagents

Enabled by default; `[modules] subagents = false` removes the tool and viewer.
`subagent` accepts exactly one of `agent` + `task`, `tasks`, or `chain`, with
optional `cwd` and `agentScope` (`user`, `project`, or `both`). Parallel dispatches
accept eight tasks and run four at once. Chains substitute the complete final
assistant text for every `{previous}` and mark remaining steps Skipped after
failure. Model text never determines a terminal state.

Built-in scout, planner, reviewer and worker agents ship with their own prompts
and no fixed model. Markdown definitions in `~/.tiechui/agents` and the nearest
ancestor `.agents/agents` and `.claude/agents` use YAML frontmatter: `name`, `description`, optional
`tools` (comma-separated or a YAML sequence), and optional `model` (`provider/id`).
User definitions win by name. Each dispatch reads the definitions afresh;
`definitions::render` rewrites a file's `name`, `description`, `tools`, `model`
and prompt and keeps its other frontmatter, for the app-server's `agents/save`. Untrusted project definitions require explicit
confirmation through the parent's interaction queue, including when a model
supplies an unknown confirmation-bypass parameter; headless execution refuses
them. Confirmation authorizes the definitions, not new project policy sources.

The plugin API's `ActorClient`/`ChildRunner` binds the current provider instances,
model and thinking at dispatch. `ChildRegistry` builds a fresh registry per task.
The CLI supplies editing (including the native child `ls` tool), shell execution,
fresh permissions, parent-anchored rewind tracking, and the existing MCP clients.
Allowlisted definitions restrict both advertised tools and actual dispatch.
No parent hooks, skills, goals, compaction, plugins, Ask or nested subagent tool
are inherited. Child sessions are private temporary SessionActors, removed only
after actor settlement. MCP connection ownership remains with the parent.

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

The main transcript shows Waiting / Finished waiting and task results.
`/subagents` or Alt+Left/Right with an empty composer opens the viewer. Arrows
select Main or a task, Enter views it, Ctrl+F searches, Ctrl+S groups states,
Ctrl+T shows thinking, Ctrl+X cancels the dispatch, and Esc returns. Wide screens
preview beside the list; narrow screens use one column. Pending approvals wait
behind the viewer and are shown on return, using the same foreground ownership
as context and BTW. Branch records restore on resume and navigation.

Live previews and parallel model summaries are grapheme-safe and retain at most
50 KiB of each output/thinking stream. Overflow is written to mode-0600 files under `state/subagents/output`;
`read` can retrieve their paths from results. Each archive is capped at 64 MiB;
live authoritative content is capped at 8 MiB / 256 blocks and cancels with an
explicit diagnostic on overflow. Completed output files are retained for resume;
there is no automatic archive cleanup. Chains read the complete final output
from the archive. Successful records retain the full final text and thinking
within the live-content bound for expanded views, plus usage and archive paths.

Tests cover modes and limits, chain substitution/skip, trust and headless refusal,
private grants and parent cwd policy, real child edits and rewind, actor/tool
panic isolation, parallel cancellation/cleanup, stale completion, output spill,
record restoration and goal usage. TUI snapshots cover 40/80 columns and both
palettes. The offline PTY case exercises parallel children, forwarded edit
approval, Alt+Right, dispatch cancellation and retained finished output.
