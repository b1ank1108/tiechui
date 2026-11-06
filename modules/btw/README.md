# BTW side conversation

`/btw [question]` opens an in-memory side conversation with tools while the main
turn continues. The module registers its command and lifecycle observers through
`tiechui-plugin-api`. Its command host reuses configured providers and credentials,
with an independent cancellation token. Available local and MCP tools use the
host's existing validation, permissions, hooks and scheduler. Tool calls and results
stay in side history; module audit entries still use the host's normal persistence.
Subagents, worktree creation, parent goal/context and compaction tools are excluded.

The inherited history is reference material, not an active task. The side assistant
may read, search and perform non-mutating checks. Its instructions require an
explicit request in the side conversation before changing files, git state or other
workspace state; this is a behavioral constraint, not an additional read-only sandbox.
Files and tool services are shared with main, so requested edits affect the same
workspace and still require normal permissions. Tool progress and bounded result
previews appear in the side transcript. Esc cancels active tools and waits for settlement.
Tool approvals use the existing main approval queue; Ctrl+/ switches to that view
and back without stopping either conversation.

Creation freezes the main system prompt and the newest 64,000 UTF-16 units of main
transcript text, plus separators and an explicit omission marker. Images become
placeholders; provider signatures and tool-result metadata are excluded. Reopening
keeps this snapshot, history, model, draft and running request. A second question
while streaming is refused. `/btw model [provider/id]` changes only the side model
while stopped; replay preserves user/assistant text and paired tool calls/results,
without old thinking or text signatures. Within a tool loop, canonical signed
assistant blocks are preserved for continuation. Effort is clamped to the selected
model's supported levels.

The TUI reuses transcript cells, Markdown, composer, pager and status-line settings.
Ctrl+/ (also Ctrl+7, Ctrl+_ and legacy 0x1f) switches threads. Enter sends, Esc stops
the side reply, Ctrl+T expands thinking, Ctrl+O opens the read-only transcript,
PgUp/PgDn and Ctrl+Home/End scroll, and `/statusline` edits shared preferences.
Empty Ctrl+C/D stops, closes and discards; with a draft, Ctrl+C clears and Ctrl+D
deletes forward. Main Ask and approvals retain their queue position behind the
foreground panel. The footer shows when main needs input, finishes or fails.

Ctrl+S shares the last complete nonempty answer as one model-visible custom message.
This does not start or steer a main turn; the next main request sees it. Nothing else
is persisted. Reload, restart, fork and branch/conversation navigation discard the
side thread; files-only Rewind interrupts its reply and keeps its partial history.

Ctrl+/ uses the kitty protocol or legacy Ctrl+7/0x1f. BTW does not negotiate
xterm modifyOtherKeys; its CSI 27 decoding is left to terminal input work.

Tests cover frozen snapshots, stream refusal, model rebuild, cancellation/epoch
isolation, sharing and persistence, foreground queuing, short composers, reviewed
40/80-column dark/light snapshots, and offline PTY provider requests and key modes.
