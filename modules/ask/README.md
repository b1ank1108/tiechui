# Ask

Enabled by default, `request_user_input` asks one to three questions with two or
three choices or free text. The module registers through `tiechui-plugin-api`;
the CLI supplies a `QuestionRequester` that shares the actor's interaction queue
with permissions and MCP elicitation. Headless calls return
`request_user_input requires interactive TUI mode.`

The TUI highlights the first choice without answering it. Up/down or j/k select;
digits commit ordinary choices, while None of the above opens text entry. Tab
adds or clears notes. Left/right, h/l and Ctrl+H/L switch questions in option
focus; Ctrl+P/N and PgUp/PgDn also work in text focus, where arrows edit text.
Each question retains its draft. Enter commits and advances; on the last question
it submits all answers or asks Proceed / Go back for unanswered questions.
Only Proceed sends uncommitted answers as empty arrays. In confirmation, Esc or
Backspace returns to the first unanswered question; Ctrl+C interrupts. With option
notes visible, Esc clears and hides notes (as Tab does in notes). Otherwise Esc
interrupts the turn.
No highlighted default supplies an answer. With `ask.auto_continue_after_sec`
set, an unanswered question is withdrawn after that many seconds and the result,
status `timed_out` with no answers, tells the model to continue on its own judgment.

Results contain `answers: { id: { answers: string[] } }`: a selected label and,
when present, `user_note: ...`. Question text and answers persist as presentation
details and replay in the transcript; only the response enters model context.
Cancelled and failed results retain question records, status and diagnostics;
older results fall back to their original tool arguments for display.
A crash while waiting leaves an unfinished tool call, repaired on resume using
the normal `No result provided` result.
