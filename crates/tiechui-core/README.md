# Session execution

`SessionActor::spawn` takes ownership of a writable `SessionStore`, provider
registry, tools, stable system fragments and a `RetryPolicy`. Open or create the
store in `spawn_blocking` before transferring it. Submit protocol `Submission`s
through the returned sender and receive `Event`s through its broadcast receiver.
Clients must handle broadcast lag by rebuilding their view. Dropping all
submission senders or cancelling the supplied shutdown token settles active
work and releases the store's writer lock.

The actor drives provider and scheduler futures itself. Session writes and reloads
move the owned store into `spawn_blocking`; the actor waits for ownership to
return before changing state. Tools receive the current execution scope, working
directory, cancellation token and epoch. Tools must obey their cancellation
contract and settle promptly, including cleaning up any child processes.

Steering is FIFO, one message at each response/tool-batch boundary. Follow-up is
FIFO and starts a new protocol turn after the active turn ends. Images retain
their bytes in both queues. Interrupt only affects the named active turn.
Queued input survives interrupt. Branch and reload first cancel and settle active
work, then increment the epoch and discard queues from the previous branch.
Submissions and tool updates from old epochs are ignored. `SessionConfig.epoch`
is the first epoch: the TUI's host starts each replacement session (`/new`,
`/resume`, `/reload`) above every epoch its client has seen.

Model and thinking changes are session entries, restored along the selected
branch. Every assistant message and usage entry records the request's model,
or the returned model when the provider reports server-side fallback, including
when settings change while it streams. Assistant stop reasons and
tool presentation details survive resume; details never enter model requests.
Non-zero usage snapshots are persisted as they arrive and replace earlier
accounting for the same request. Clients receive the last snapshot once when
the request ends, including on interruption or failure. All-zero snapshots
are ignored. Totals include all branches. Resume only restores state; it never
starts a request automatically.

Assistant entries also retain their request's final usage for context estimation;
zero usage explicitly prevents an earlier failed request's snapshot from being
mistaken for that assistant's usage. Older entries use the preceding snapshots.
The actor anchors output fitting on the latest successful non-zero assistant usage
on the selected branch and estimates only trailing messages. Compaction invalidates
earlier anchors; without an anchor, the whole outgoing request is estimated.

Read-only command contributions can request a measured `SessionView`. The actor
retains bounded request-time display observations and usage-prefix fingerprints
in memory, invalidates observations on navigation/model/catalog changes, and
returns structured command panels through the protocol without changing request
or session bytes. Category estimates use the provider's shared estimator.

Background module commands receive an epoch-scoped host for tool-free provider
streams and explicit reference-message appends. These use independent cancellation;
reference messages enter the next request without starting or steering a turn.

Failed/incomplete assistant messages and matching error tool results remain in
the append-only transcript. A `request_omission` custom entry records their entry
IDs. Request context excludes those audit entries, including after resume.
Replay also filters persisted failed/aborted assistants, synthesizes missing tool
results, defers interleaved system messages and drops orphaned results, so a crash
before the audit marker or tool completion remains resumable. Anthropic error
images follow all tool results in their user message.
Successful tool batches remain in context during retries and are never replayed
by the retry loop. Each model request gets its own bounded retry budget.
Rate-limit, overload and transient-network errors are eligible. An ambiguous
request rejection classified by the provider gets one automatic recovery after
two seconds within the same request budget; repeating it stops recovery. Backoff
respects `retry_after`; a requested delay above the policy's maximum ends the
turn immediately. Context overflow compacts and retries once when automatic
compaction is enabled; a second overflow ends recovery.
Quota/billing exhaustion is terminal. `[retry]` config defaults to `enabled = true`,
`max_retries = 3`, `base_delay_ms = 2000`, and `max_delay_ms = 60000`; a supplied
table replaces the earlier layer's table, with omitted fields using defaults.
Delays must be positive and the maximum must be at least the base delay.
Each scheduled wait and subsequent attempt emits `request_retry`; turn completion
ends retry activity. The TUI enters waiting status titled `Retrying request` with
Esc and pauses the activity timer until the retry starts, just as for prompts.
`chui exec` reports one-line retry messages on stderr and events in `--json`.

`Op::Retry` (the TUI's argument-free `/retry`) runs one manual recovery attempt,
or starts the scheduled attempt immediately during backoff. It adds no input and
does not re-execute settled tools. Interactions and queued inputs block recovery;
new input cancels a pending wait. Model/thinking changes keep the wait active and
apply to its next attempt. A branch-local `request_recovery` entry preserves
the failed request's system/input context and diagnostic, including empty failures,
so resume offers manual recovery without restarting timers or input hooks.
Subsequent messages or compaction invalidate that point. `RetryAvailable` reports
the manual entry point; the TUI shows its countdown separately from the working
status and clears the failure hint on input.

Native compaction belongs to the actor. `/compact [instructions]` settles active
work first; automatic checks run at response boundaries and before the next
request. `[compaction]` defaults to `enabled = true`, `reserve_tokens = 16384`,
and `keep_recent_tokens = 20000`. Token settings are non-negative safe integers,
including zero; a supplied table replaces the earlier layer. The strict threshold
is context tokens greater than the model window minus the reserve. Preparation
uses the session projection and the existing usage anchor/shared estimator,
including its image allowance, and keeps complete tool call/result groups.
`[compaction.model_overrides."provider/model"]` can override either token setting
for a model; omitted settings inherit the ordinary values.

Summary requests use the current model and reasoning level, the provider stream
and retry policy, separate initial, update and split-turn prompts, and no tools
or explicit cache writes. Read/modified file lists survive successive
checkpoints. Truncated or tool-calling summaries are rejected. Only a complete result appends the existing
`Compaction` entry; failed or cancelled summaries leave the transcript unchanged.
Overflow and recoverable truncated responses omit the failed attempt before one
compact-and-retry recovery. Queued input survives compaction and cancellation;
goal continuations wait until the actor is idle. Compaction resets request display
observations, recovery, cache-prefix attribution and earlier usage anchors.

A summary request estimated above the window minus its output room, or rejected
as a context overflow, omits the oldest whole rounds of its input copy after the
opening request, keeping the last round; at most two reductions per operation,
sharing the retry budget. Such a checkpoint records `degraded` and the omitted
entry range, says so in its summary and renders as degraded. Three consecutive
automatic failures, or a checkpoint that leaves the context above the threshold,
pause automatic compaction until a new model-visible entry or `/compact`. Without a
usage anchor (a new session, the first request after a checkpoint, or a provider
that reports no usage) the threshold estimate covers the whole next request,
including the system prompt and visible tools. When those alone exceed the
threshold no checkpoint can help, so only overflow recovery and `/compact` run.

Ordered `Compactor` contributors can cancel or supply a checkpoint, once per
operation, and restore context it loses. Restored items are budgeted against the
remaining headroom, deduplicated by key, stored in the entry's `details.restored`
and replayed after the summary. Hooks register their existing `PreCompact` callback
through this contribution. Protocol start/progress/end events expose the
prepare, summarize, restore and commit phases, retry and reduction counts, and
failure; summary items render live and after resume. Summary usage contributes to
session totals without becoming a context anchor. Tree-navigation branch
summarization is outside this implementation.

Tool input schemas are validated before scheduling with `jsonschema`; `schemars`
generates schemas but does not validate model-generated arguments. Default features are disabled so schema
validation cannot fetch remote references. Required-property and type errors use
fixed wording (`must have required properties <name>`, `must be <type>`); other
schema constraints retain the validator's detailed diagnosis inside the same
`Validation failed for tool` error result. Optional non-nullable null properties
are removed recursively before primitive coercion and validation; required nulls
remain subject to validation. Tools receive the converted arguments, while error
messages and the persisted call retain the original arguments.
The transitive `borrow-or-share` 0.2.4 dependency uses MIT No Attribution;
`deny.toml` permits MIT-0 only for that crate and version after reviewing its
bundled license text.

`tests/fixtures/tool-errors.json` holds the expected validation, coercion,
missing-tool, truncated-call and failure texts. Replay closes a dangling call
with `No result provided`. Cancellation uses `Operation aborted`; execution
failures preserve the tool's error message.
