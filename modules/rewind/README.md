# Rewind

Enabled by default; `[modules] rewind = false` removes the command and tracking.
`/rewind` and idle double Esc open the same chronological list, with the latest
user message or control checkpoint selected. Enter checks a fresh plan before
offering code and conversation, conversation only, code only, or Never mind.
Conflicts and unavailable backups leave conversation restore available.

Conversation restore uses `Op::Branch` to move before the selected entry, keeps
the old branch, and refills the composer without a model request. Only refills
with images are persisted once; ordinary text edits and text-only refills are
not saved. Branch navigation persists independently of draft entries. Saved
attachments restore only into an empty editor. Sending, deleting attachments,
clearing the draft, or `/rewind detach` records their removal. Commands require
an interactive terminal.

The tool interceptor checkpoints edit/write (and later registered replace,
insert, and undo tools), using editing's shared plugin-api path resolver. It
settles successful, failed, and cancelled writes. Backups live under
`<profile>/state/rewind/<workspace hash>/<session hash>` and are never cleaned
automatically. Permission-approved files outside the workspace are tracked by
absolute path; workspace files keep relative keys. Version 2 manifests read
existing version 1 checkpoints and upgrade on the next write. macOS system
directory aliases (`/tmp`, `/var`, `/etc`) resolve to their canonical locations;
other linked parents remain refused. Files over 20 MiB, protected metadata paths,
symlinks, hard links, external conflicts, corrupt backups, and active writer leases
are refused. A failed checkpoint still blocks the edit. Outside managed session
worktrees, bash and external changes are not captured in patch checkpoints;
rewinding turns that ran commands keeps patch or conversation restore and reports
the un-reverted commands. Statistics describe
`checkpoint → current`; binary, oversized, and expensive diffs are uncounted.

In managed session worktrees (`$TIECHUI_HOME/worktrees/<bucket>/<name>`), a
`BeforeRequest` hook also captures a per-turn hidden Git tree snapshot via a
temporary index (`GIT_INDEX_FILE`) under `refs/tiechui/checkpoints/<session>/<anchor>`
without touching user staging or `git log --all`. Snapshot capture timeouts or
failures are recorded in `manifest.json` without blocking the turn. Whole-directory
snapshot restore verifies directory isolation across live and persisted non-deleted
sessions first, refuses when another session shares or nests the directory (leaving
conversation-only rewind available), and commits the conversation branch before
materializing files. Single-turn `turn/undo` always uses patch checkpoints.

Private 0700 directories, 0600 content-hashed blobs, atomic manifests, and an
explicit restore journal protect retained versions. The workspace lock uses
OS locking so a crashed process cannot strand the recovery lock. File leases
remain after an unresolved edit. `/rewind` asks directly before recovering an
interrupted restore's pre-operation files; it does not move the conversation.
File and conversation outcomes are reported separately. Forks copy retained
checkpoints and blobs independently; reload reads the same session store.
Native compaction retains raw branch entries, so conversation restore can cross a
checkpoint and recover its original history and usage anchor without summarizing.

The host supplies generic branch views, serialized interactive commands,
identity validation, navigation, composer persistence, and extension events.
Rewind emits `rewind.open`, `rewind.committed`, `rewind.closed`, and
`workspace.files_changed`; editing invalidates its observations on file changes.

Goal adds control anchors before create, edit and resume; it freezes continuation
while Rewind is open, pauses the old branch on commitment, and reads the restored
branch state.

Subagents receive isolated trackers anchored to the parent's user/control entry
and workspace. Tool settlement releases writer leases even after a tool panic;
dispatch cancellation joins children before navigation or restore can proceed.
The actor readiness boundary replaces process-exit reconciliation.

Tests cover storage protection, cancellation settlement, all restore modes,
first-message and image-only refill, restart recovery, fork inheritance,
permission-gated external edits and recovery, legacy manifests, UI keys and
snapshots, and the offline mock-backed PTY flow.
