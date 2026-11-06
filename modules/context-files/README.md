# Context files

The only runtime contribution is a plugin-api `ContextProvider`. The only
runtime dependency is `tiechui-plugin-api`; test dependencies are `tempfile` and
`tokio`. JSON types and the async trait come through plugin-api reexports.

Discovery loads one global file under the config home, then ancestors from the
filesystem root to cwd, including directories above Git roots. Each directory
prefers `AGENTS.override.md`, `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, then
`CLAUDE.MD`; directories are ignored. Global paths are deduplicated, UTF-8 BOMs
are stripped, and other whitespace is preserved. Global and project discovery
paths are resolved lexically without realpath; rendered paths and audit paths
keep the same spelling, including symlinked directory prefixes. A nested linked
worktree shadows the same-named main-worktree file, including when the local
file is a symlink. Different filenames and bare layouts retain ordinary ancestor
inheritance. Global, project and ancestor context files load regardless of
project trust. Trust still applies to other project resources in the host.

The aggregate retained content budget is 32 KiB. The last retained file is cut
at a UTF-8 boundary; remaining files are omitted and a notice is appended inside
`project_context`. The budget excludes tags, paths and the notice. Reads use
bounded buffers, validate the entire selected file, and check cancellation
between reads. Invalid UTF-8 files are skipped with a path diagnostic, including
invalid bytes beyond the retained prefix. Read errors are also reported as
diagnostics.

Rendering wraps the files in one `project_context` section, each in a
`project_instructions` tag carrying its path. The `cwd` section follows the
context section, including when there are no files. There is no current-date line.

The CLI opens storage and initializes the module on blocking workers. Context
files are rediscovered on every process start, including resume, so edits,
deletions and newly preferred files take effect immediately. Session data is
never used to restore context.

The `context_files` custom entry is an audit record: version 2 contains the
ordered paths and retained UTF-8 byte counts sent in the context contribution,
plus truncation state. It contains no file contents. Before starting the actor,
the CLI compares this record with the last one on the active session branch and
appends only if they differ. Same-size content edits still reach the next
process's request without creating a duplicate audit entry.

The TUI's `/reload` ends the session and prepares it again, so files are
rediscovered and changed metadata audited as at process start. Protocol
`Op::Reload` reloads session storage only; it has no module reload hook.
`chui exec` has no interactive reload command. Disable discovery and the cwd
contribution with `[modules] context-files = false`.

Golden tests recreate the fixture trees in `tests/fixtures/golden.json` and
compare the complete context/cwd fragment byte for byte.
