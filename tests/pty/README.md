# PTY tests

Run `just pty`, or select independent cases with `just pty resume approval`.
`python3 -B tests/pty/smoke.py --list` lists the names. The harness uses Python
3.9+ standard library facilities only. `TIECHUI_BIN` selects an already built
executable (default `target/debug/tc`); the just recipe builds `tc` first, then
runs emulator unit tests and the selected PTY cases. No Python/Node packages,
production test switches, real credentials or external provider are needed.

Each case owns a temporary home, generated provider config, fake environment
key, Git working directory and scripted `http.server` on `127.0.0.1`. The mock's
two-model catalog generates the configured model list. It sends actual
openai-completions SSE and records requests, including tool results. Held
streams use explicit release events rather than racing a provider delay.
The child receives an isolated environment, including temporary `HOME` and
`TIECHUI_HOME`. All generated files stay under `.local/pty/`.

The PTY answers OSC 10/11, primary device attributes and CSI `? u` queries.
The incremental UTF-8 screen emulator handles cursor movement, wrapping,
scrolling, line/display erase (including 2J and scrollback-only 3J), wide CJK
cells, private-use font glyphs and 1049 alternate-screen save/restore. Assertions
inspect the current screen, not escape-stripped output history. Each wait has a
deadline and failures dump the screen. Successful cases delete their temporary
directories; failures print and retain the directory, raw terminal streams,
screen dumps and provider requests. Cases can be rerun by name.

This is a text/cell emulator, not a font or pixel renderer. Colors and styles
are covered by Rust snapshots. It does not emulate native terminal width
reflow, so `resize` proves SIGWINCH/TIOCSWINSZ handling and usable composer/status
layout, not the absence of terminal reflow artifacts. The harness uses
POSIX interfaces shared by macOS and Linux; Linux execution and real terminal
fonts remain reviewer checks. Windows ConPTY is not covered.

## Executed cases

| Case | Screen assertions and additional evidence |
| --- | --- |
| `startup` | Composer/status, all terminal query replies, Ctrl+D clean exit and alternate-screen restoration. Guards the interactive startup argument panic found by this gate. |
| `setup` | First run without providers: OpenAI format, address, a missing then a set key variable, the mock's `/models` list searched and two models checked with Space, edited token limits, one check request for the first, the exact `providers.toml` written and the first model selected beside a preserved comment, then the session opens. |
| `consent` | First open of an untrusted project with a hook: quitting saves nothing, trust and denial are saved, skipped hooks stay unapproved and are asked about again, approval enables them, and a settled project asks nothing. |
| `sdk-rust` / `sdk-typescript` | Tagged marketplace installation from a temporary copy of the sibling `tiechui-plugins` repository, capability approval, tool result, interceptor denial, and opening/dismissing the panel. Both skip when that checkout is absent; TypeScript also skips when Node is missing. |
| `streaming` | Partial reply visible while SSE is held; final chunk appears after release. |
| `tools` | Two real reads coalesce under Explored, bash renders Ran, and actual tool results reach the next request. |
| `rewind` | Mock write, double Esc, code/conversation restore, then draft clear and reload: restored file, no draft writes from typing, no history replay or extra request. |
| `interrupt` | Esc interrupts a held stream, prints the interruption banner and leaves the composer usable. |
| `ask` / `ask-killed` / `ask-cancelled` | Multi-question choices, layered Escape, notes, free text and transcript replay; a killed pending Ask resumes with a missing result, while a cancelled Ask replays its interrupted record. |
| `retry-manual` | A mock 500 and resume retain identical request context; `/retry` adds no input, rejects arguments and cannot rerun success. |
| `retry-interrupt` | A 503 enters retry backoff with a paused activity timer. Esc cancels promptly without another request; resume continues without repeating the failed attempt. |
| `queue` | Follow-up preview and steering; no premature request, then steering followed by two FIFO follow-ups in recorded requests. |
| `paste` | Folded multiline paste expands to the exact original provider payload. |
| `cjk` | Individually typed Chinese input, Chinese reply, exact submitted Unicode text. |
| `resize` | TIOCSWINSZ and SIGWINCH at 50/120/80 columns; draft, rules and status survive. |
| `resume` | Real sessions seeded through `tc exec`; startup picker, search, Ctrl+E metadata/preview, Ctrl+T conversation, layered Esc starting a new session, in-session `/resume`, retained history in the next request, and startup Enter resume. Browsing leaves old bytes unchanged; the storage unit test also checks mtime. |
| `model` | `/model` shows both mock models, switches to beta and back to alpha; request model IDs agree. |
| `approval` | Trusted project rule asks for bash. Allow once executes, a second call asks again, deny-with-reason blocks its write and reaches the mock as a tool result. |
| `statusline` | Icon preview, cancel, saved battery and actual 90% remaining usage, reload persistence, preserved TOML comment, and host-backed `/trust` status/allow. |

`test_screen.py` separately guards incremental queries/Unicode/private-use
glyphs, cursor/erase behavior, alternate-screen restoration, wrapping, scrolling
and resize. In particular, erased text must not satisfy later assertions.

The `t2-plugins` case installs the local Rust test plugin, approves its declared
capabilities, invokes its tool, verifies interceptor denial, opens a declarative
panel and checks that the process group is gone after exit. `just pty` builds the
fixture binary as well as `tc`.
