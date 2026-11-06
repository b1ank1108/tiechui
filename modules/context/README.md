# Context inspection

`/context` opens a frozen, read-only snapshot of the current branch. Optional
`system`, `tools`, `injections`, `messages`, and `cache` arguments open those views.
The module is enabled by default; `[modules] context = false` disables it.

Overview shows category estimates, the model window and free space. At 52 columns
with sufficient height it also shows a 16-column allocation matrix; `z` toggles
Fit when the window exceeds the fitted scale. Category percentages always use
the full window. Arrow keys or Tab switch views, up/down or j/k select, Enter
opens a category or preview, `/` searches names and sources, and PgUp/PgDn,
Ctrl+U/D and Home/End navigate. Mouse clicks select, double clicks open, and the
wheel scrolls. `i` opens usage details, `c` opens cache diagnostics, Escape returns
one level, and Ctrl+C closes immediately. Closing restores the composer; pending
main-thread questions remain queued and show a notice over the panel.

System retains effective fragment categories and instruction-file references;
tools group by declared provenance and use the existing JSON syntax highlighter.
Messages stay chronological. Images are placeholders, and signatures, opaque
replay data and tool details never become preview text. Injections are references
or request-time additions/removals, excluded from category totals. Plain messages
that resemble JSON remain plain text. Sources that do not fit in the header stay
in the scrollable preview. Existing pager scrolling and status-line colors are
shared with the rest of the TUI.

The `Command` contribution requests a generic `SessionView` through
`tiechui-plugin-api` and returns a `CommandPanel` protocol event with the snapshot,
initial view and a body-free text report. The TUI consumes only protocol data;
the kernel does not recognize the command name. Inspection does not call the
model, write the session, or change outgoing requests. Reopening refreshes the
snapshot. Last-request observations and usage-prefix fingerprints stay in actor
memory. Branch/reload and model changes invalidate observations, and changed tool
declarations make the old observation unavailable.

Measurements use `tiechui_providers::estimate` (UTF-16 characters / 3.5 and image
allowances). The session estimate uses the same successful-response usage anchor
and trailing-message estimate as the actor's output clamp; provider response
usage remains separate. The matrix
uses category allocations without scaling them to provider usage. Diagnostic
previews have a 64 KiB item limit and 1 MiB text budgets for request differences,
request prompt metadata and branch inspection; clipping never affects requests.

Cache diagnostics use the last 128 valid assistant usages on the selected branch,
including cache writes in reported input. A preceding response supplies the first
row's baseline. Native compaction, model/system changes, unavailable usage, and observed prompt/tool
prefix changes reset attribution. The bounded prefix metadata is not persisted;
older records use recorded branch boundaries. Fresh input and unattributed misses
are estimates, not billing or proof of TTL expiry. No cache probes are sent.
After compaction, stale provider usage and request previews are cleared; the
projected checkpoint and retained messages use the shared fallback estimate.

Range compaction contributes recorded event counts, estimated net reclaimed tokens,
and compression-associated cache misses. Attribution uses the saved before/after/
prefix observations, ends after four cold responses or a warm response, and resets
with the existing baseline boundaries. Sessions without fold records retain the
previous report.

BTW shared-message references, side-thread notices and subagent contributions
have no dedicated rows; no attribution or placeholder data is invented for them.

Verification covers request/session byte invariance, both provider HTTP bodies,
bounded and opaque previews, cache reconciliation and window limits, layered
navigation and resize bounds, and 40/80-column dark/light snapshots per view.
`just pty context` exercises every view and message search against a local mock
while verifying that inspection sends no additional request. Run `just check`
and `just pty` for the complete gates.
