# tiechui-session

Synchronous JSONL session storage, independent of providers and the agent loop.
Pass the resolved `TIECHUI_HOME` (or its default `~/.tiechui`) to `create`,
`session_dir`, and `list`. The canonical cwd is percent-encoded as one directory
component; unlike separator replacement, this distinguishes `/a-b` from `/a/b`.
Names longer than 120 bytes retain up to 120 encoded prefix bytes, stopping before
any partial `%HH` escape, followed by `-` and the 16-digit hexadecimal FNV-1a 64
hash of the full canonical cwd bytes. The directory name is at most 137 bytes.
Headers preserve the lexically resolved cwd, including symlinks.
New directories use mode 0700 and session files use 0600 on Unix.

The first line is a versioned header. Timestamps are Unix milliseconds. Entries
contain a unique ID, nullable parent ID, timestamp, and a flattened `kind` payload.
A null parent attaches to the header, the implicit root. `path_to_root` returns
entries in root-to-leaf order; the header itself has an empty path. `branch_to`
accepts either an entry ID or the header ID. Navigation changes the in-memory leaf;
the next append records that branch. Reopening selects the last appended entry.
Assistant message entries require a `ModelRef` identifying their originating
provider and model. Replay returns messages unchanged, including opaque thinking
data and redaction flags; it performs no cross-model transformations. Optional
`stop_reason` and `details` fields preserve assistant outcomes and the single tool
result's UI presentation data. Older entries without them still load. Details are
excluded from model context. Usage entries are cumulative snapshots: `request_id`,
when present, identifies the first usage entry whose accounting this snapshot
replaces; legacy usage entries count independently.

`open` acquires a nonblocking advisory file lock before loading and holds it until
the store is dropped, including while idle. `open_read_only` takes an unmodified
snapshot without acquiring a writer lock and can fork that committed history.
The lock protects cooperating local writers, including symlink aliases. It does
not provide live attachment, handoff, or protection from noncooperating writers.
An unexpected file length change rejects an append. After an I/O failure, reopen
the store before writing again.

Each append writes a complete JSON line and syncs it before updating memory.
Loading ignores malformed JSON only at an unterminated final line and reports a
diagnostic; a bad header, complete malformed line, or invalid entry is an error
with a one-based line number. Opening never repairs the file. The next writer
append removes the ignored tail or adds a missing newline after a valid final
entry. `fork` writes a new header with a new session ID and parent session path,
then copies exactly the selected path, retaining original entry lines byte for
byte (adding a final newline if needed). Unknown kinds and additional fields
therefore survive forks unchanged. `list` reads headers only, without locking or
parsing message bodies. It returns a `SessionList` containing sessions and per-file
diagnostics for unreadable or invalid headers, skipping those files. Missing
session directories are empty lists; directory failures and cancellation are errors.

`summaries` and `preview` serve the resume picker through `open_read_only`, so
they never lock, repair or touch a file. `summaries` lists one cwd's directory, or
every directory under `sessions/`, skipping sessions without user or assistant
messages; `modified` is the timestamp of the latest such message and the search
text joins their text. `preview` replays the current branch from the root
as user, assistant, tool and notice parts, including messages a compaction replaced
and its summary; thinking, signatures and image bytes are left out.

Appending a compaction entry captures the current system message. In
`build_context`, the latest compaction on the active path contributes that system
checkpoint and one summary user message, then pre-compaction entries starting at
`first_kept_entry_id` (excluding system messages), then messages after that
compaction. Missing, null, or self anchors keep no earlier messages. Older
compactions in a retained range contribute no summaries. Metadata and custom
module data do not enter model context.
`project` exposes these same messages with their source entries and invisible
metadata for the core's cut-point selection. Compactions optionally retain summary
usage, the pre-compaction token count, file-operation details and contributor origin;
older entries still load. The core generates checkpoints and invalidates older
usage anchors. Raw history remains available to Rewind and branch navigation.

I/O APIs take a `CancellationToken` and check it before mutation and between
entries. A started append finishes its write and sync as one commit boundary.
Tests use disposable directories inside this crate, with no network or real
session files.
