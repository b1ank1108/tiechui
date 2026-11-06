# File tool contract

Tool descriptions and input schemas are declared in `src/specs.json`.

| Tool | Parameter | Contract |
| --- | --- | --- |
| read | path | Required string, relative to execution cwd or absolute; `@`, tilde and read filename variants |
| read | offset | Optional Number, 1-based line, default 1; fractional values truncate toward zero and negative values count from the end |
| read | limit | Optional Number, maximum lines, applied before display truncation |
| read | column | Optional integer >=1, Unicode code points in selected line |
| read | maxChars | Optional integer 1–8192, default 8192; window requires limit omitted or 1 |
| write | path | Required string, relative or absolute |
| write | content | Required string, literal UTF-8 bytes |
| edit | path | Required string, relative or absolute |
| edit | edits | Required array, nonempty at execution; original-file matching, all or nothing |
| edit | edits[].oldText | Required string, unique, nonempty, no overlapping ranges |
| edit | edits[].newText | Required string, literal replacement; only EOL normalization |
| edit | legacy oldText/newText | Prepared into an appended edits[] entry, not declared in schema |
| edit | string/object edits | JSON string or single edit object prepared into array |
| grep | pattern | Required regex string |
| grep | path | Optional string, default execution cwd |
| grep | glob | Optional rg glob |
| grep | ignoreCase | Optional boolean, default false |
| grep | literal | Optional boolean, default false |
| grep | context | Optional Number, symmetric context; nonpositive means 0 |
| grep | limit | Optional Number, default 100, clamped to >=1, safe integer |
| grep | outputMode | Optional `content` (default), `files_with_matches`, or `count` (occurrences per file) |
| grep | offset | Optional integer >=0; matching blocks in content, files in other modes |
| grep | beforeContext | Optional integer >=0; overrides context |
| grep | afterContext | Optional integer >=0; overrides context |
| grep | multiline | Optional boolean, default false; dot matches newline |
| grep | onlyMatching | Optional boolean, default false; ignores context in content mode |
| grep | type | Optional rg file type string, intersects glob |
| find | pattern | Required glob; bare patterns match basenames, slash patterns match paths relative to the search root |
| find | path | Optional string, default execution cwd |
| find | limit | Optional integer >=0, default 1000; 0 means unlimited |

## Results and execution

`read` returns raw text, line-range notices, and the same model-visible
structured text. Display is limited to 2000 lines/50 KiB. A first line over
the byte limit automatically becomes an 8192-code-point window. Windows scan
to EOF for exact totals and return explicit next parameters without splitting
Unicode characters. Ordinary reads retain BOM and CR bytes.
Images return image content plus a `Read image file [mime]` note and a
structured image object. The host can select the auto-resize setting with
`register_with_image_resize`; `register` enables it. Image conversion and
resizing use the `image` crate's codecs.

`grep` preserves separate, potentially overlapping match/context blocks,
`file:line: text` and `file-line- text`, and 500 UTF-16-unit line clipping.
Explicit offset and file modes sort by path. Pages rescan; a full page only
means more results might exist. Byte-truncated pages never advertise a next
offset. `find` uses `ignore::WalkBuilder` and `globset`, includes hidden entries,
honors `.gitignore` and `.ignore` even outside Git, and skips `.git`. Directory
symlinks are listed but never traversed. Patterns containing `/` are relative to
the search root; use `**/src/**/*.rs` to include nested `src` directories. Results
are globally sorted by relative path, with `/` appended to directories. The
walker scans all candidates while retaining only the smallest `limit` matches;
the result-limit notice is emitted only when additional matches actually exist.
Traversal errors and matching non-UTF-8/control-character paths fail explicitly.
Cancellation is checked during traversal and output assembly.

Content search uses installed `rg`; it never downloads a binary or falls back.
rg user configuration is disabled. stdout records are bounded to
8 MiB, stderr retention is bounded, and stopped/cancelled producers are reaped.

All output truncation uses `tiechui-tools::output::TextHead` and its existing
`truncate` implementation. The head collector preserves a UTF-16 prefix
bound and exact complete decoded totals. There is no second display
truncator in this module.

Mutations serialize per resolved file even when called directly outside the
scheduler. Reads/searches are Shared; edits/writes are Exclusive. A file read
is optional, but existing observations must remain fresh. Full observations
compare logical SHA-256 content; partial observations compare identity, size,
mtime and ctime. Recovery uses only unique, freshly observed representations
of line prefixes/escapes. Replacement strings are never unescaped. Normal
exact/fuzzy matching and uniqueness checks always take precedence.

Failure diagnostics are bounded suggestions, never match authorization.
Every edit is validated against the original file, overlap is rejected, and
formatting/diffs are computed before publishing any bytes.
Exact replacements preserve untouched bytes; fuzzy replacements rewrite only
touched lines. BOM, mixed EOLs, and absent final newline are retained; lossy
UTF-8/NUL edits are rejected.

Existing files are checked for both content and identity during capture and
again immediately before publication. On macOS (`cp -p`) and GNU Linux
(`cp --preserve=all`), single-link regular files use a private same-directory
stage and rename. Existing hard links and systems without a metadata copier
write in place after revalidation, retaining the inode. Symlinks resolve to
and revalidate their actual target; dangling links and special files fail.
New files use hard-link publication or exclusive creation on unsupported
filesystems. Complete writes finish syncing even after cancellation; failures
after publication explicitly say the file was written. These checks are not
an atomic cross-process compare-and-swap.

The workspace currently targets Unix (`tiechui-exec` rejects non-Unix builds),
so there is no Windows publication path. Extended-attribute and ACL
preservation is tested on macOS only.

## Lifecycle

`register(&mut ToolRegistry)` returns the session's `EditingState`; its
`clear` and `retain_visible_reads` let the host drop or narrow observations.
As a module, editing clears observations on every session start/end and on
`workspace.files_changed`. Generations prevent an in-flight read from restoring
cleared observations. Registry instances isolate their state. The module
contains no checkpoint, permission, compaction or kernel-specific logic. The
caller's CancellationToken controls active work and queue waits.

## Verification

`tests/contract.rs` replays the golden fixtures described in
[`tests/fixtures/README.md`](tests/fixtures/README.md). It and the unit tests
also cover:

- read errors, line/byte limits, invalid UTF-8, long lines, streaming totals,
  Unicode windows, cwd, `@`-prefixed and decomposed filenames;
- grep pagination, count/type/ignore filters, multiline and onlyMatching Unicode
  offsets, byte truncation without a next offset, invalid regex/context,
  overlapping context, CRLF/EOF, missing `rg`, the bounded record decoder and
  SIGTERM-ignoring producers;
- find globs, nested paths, hidden and ignored entries, ordering, limits and
  symlinks;
- edit/write matching and batch errors, mixed EOL/BOM/fuzzy bytes, invalid
  encoding, content and inode races, parent creation, cancellation before and
  after publication, finalization failure, symlinks, hard links, modes, FIFOs,
  dangling links, per-file queue serialization, and macOS xattrs/ACL;
- observed line/escape recovery, literal replacements, unseen ranges, overlap,
  stale reads, retargeted symlinks, state clearing and visible-read projection;
- registration, concurrency classes and per-actor state isolation.
