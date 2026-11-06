# Goal

Enabled by default; `[modules] goal = false` removes its commands, tools and
runtime hooks. `/goal` shows the current objective and accounting without a
model request. `/goal <objective>` starts a goal, confirming replacement of an
unfinished one. `/goal edit [objective]` preserves identity and usage; without
text it opens the composer editor and Esc keeps the original. Editing a complete
goal reactivates it; editing a paused goal leaves it paused. `/goal pause`,
`/goal resume` and `/goal clear` are user controls. Complete goals need editing
or replacement before resuming.

The tools are `create_goal({objective})`, `get_goal({})` and
`update_goal({status: "complete" | "blocked"})`. Creation requires an explicit
request and cannot occur during automatic continuation. Completion evidence and
the same-blocker audit over three consecutive goal turns are model instructions;
the schema does not pretend to encode evidence or blocker identity. User resume
starts that audit again. Tools cannot pause, resume or clear, and updates must
belong to the current goal's run.

Each state change appends `tiechui.goal.state`. Only the last entry on the
current branch is read; clear and corrupt state never revive an older goal.
Every request, including retry, gets the current goal as a temporary message.
System instructions and user input history are unchanged. Reload and restart
retain usage and defer active goals until new input or resume. Fork inherits
the parent's current state and defers. Branch navigation pauses active goals;
Rewind preview only freezes scheduling, and confirmation pauses the old branch.
Create, edit and resume write rewind control anchors before changing state.

Continuation runs only after the whole turn settles and the actor is idle,
without queued input, a main Ask/approval, blocking picker, composer draft or
Bash work or native compaction. BTW and context viewing do not block it. The actor checks session,
branch and queue again before consuming the module's generation and goal ID.
Automatic turns without tools defer. Esc/Ctrl+C pauses; terminal model errors
block, and real quota exhaustion sets `usage_limited`. Ordinary tool failures,
high usage and missing usage do not stop an otherwise useful turn. Automatic
turns started by other modules are invisible to the goal: no binding, accounting,
deferral or status change, no goal creation, and the request carries only the
goal status line. Idle work from several modules takes turns.

Accounting adds actual assistant `input + cache_write + output` once per
message. Cache reads and reasoning are not added. Missing required usage fields
mark accounting partial. Time accrues only inside associated goal turns and is
settled on pause/completion; time outside the process is excluded. No token or
time budgets exist. The status is magenta and refreshes during execution.

The generic plugin API runtime hooks supply branch restoration, request
messages, turn settlement and guarded hidden input; the kernel has no goal
policy. Native compaction shares the actor readiness boundary; summary usage is
session accounting, not a goal assistant message. Subagent task totals arrive
through generic `runtime.usage` entries and are counted once in the parent's
associated run, including finished work from cancelled dispatches.
