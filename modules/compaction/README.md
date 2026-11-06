# Range compaction

Enabled by default through `[modules] compaction = true`. The module contributes
`context_status`, `compact_history`, `search_history`, and `read_history`
exclusively through `tiechui-plugin-api`. Native `/compact` and capacity recovery
remain available. Without `read_history` in the active tool list, compaction,
summary projection and output references pause; raw history remains visible and
saved blocks remain intact.

`context_status` lists replaceable sources as candidates: old user turns, long
turns cut at assistant-step boundaries that keep tool pairs and reasoning whole,
and runs of adjacent summaries. Each candidate carries mechanical first/last
excerpts, a source estimate and a `summary_limit_chars` that still clears the net
savings minimum. Candidate ids derive from the exact source set, so they stay
valid across unrelated steps. `compact_history` accepts 1–8
`{candidate_id,summary,title?}` items, revalidates every source, and applies the
whole batch or nothing; failures are JSON with a code such as
`CANDIDATE_CHANGED`, `INSUFFICIENT_SAVINGS` or `INVALID_ARGUMENT`. Recent
messages/tokens, the latest user request, skill and compaction records, opaque
content, tool pairs and signed/thinking envelopes are protected. Summaries may be
merged again; tiers are kept only as depth. Hints appear only at the request tail
and list at most three recommended candidates. Compaction never makes a separate
model request.

Each maintenance point starts a cooldown of three business responses, ended early
by new user input. Under normal pressure it hides candidates and hints, and
`compact_history` returns `MAINTENANCE_DEFERRED` with the remaining count;
commits in the same batch share one point. Capacity pressure overrides the
cooldown, and `/context cache` counts those overrides.

State is appended as `tiechui.compaction.v1` (version 2; version 1 stays
readable) after successful validation. Original session messages are never
rewritten. Reload, resume, forks and rewind restore branch state; incomplete
ranges and corrupt state cannot conceal source history. A live historical
`compact_history` or legacy `compress` call carries its summary once; a synthetic
summary is used only when that call is absent. `/context cache` attributes bounded
compaction-associated misses and shows recorded events and estimated net reclaim.
These estimates do not establish cache causality or billing savings.

`[compaction]` uses only snake_case keys alongside the native checkpoint settings.
Defaults are a 300000-token scheduling cap, 32768 fallback window, five recent
messages/5000 tokens, 5000 source characters, 50–20000 summary characters and
1000 net saved tokens. Nudge, tiers, compress, search and output subtables reject
unknown fields; ratios are in `(0,1]`. `[compaction.decompress]` is accepted and
ignored. `/reload` applies edits without discarding blocks.

`search_history` is local Chinese-aware BM25 with typo tolerance over raw branch
records and summaries, with a 256-record/1000000-character feature cache. Hits
report role, tool, whether the record is still in view or which summary covers
it, and `read_args` that start reading near the match. `read_history` returns
paged records with Unicode-scalar spans; `view=summary` reads a block itself.
Every query result is bounded by `max_tokens` and pages with opaque cursors.

`[compaction.output]` defaults to `enabled=true`, `min_chars=20000`.
Earlier-turn plain tool results become first/last 400-character previews with
`read_history` arguments. Current turns, history reads, errors, images, signed
text, protected tools and tool-managed output files remain inline. Previews are
recorded on the branch before the request that first shows them, so later
requests and resumes keep the same prefix. New previews wait for the cooldown
unless capacity is pressured or the provider's declared prompt-cache retention
has lapsed since the last cached response (Anthropic Messages: five minutes, one
hour with `cache_retention = "long"`). Private files written by earlier versions
are left in place.

Run `just check` and `just pty`. The mock PTY range-compaction case verifies status,
compaction by candidate id, summary-once requests and reload without an extra
summary request.
