# Editing tools

The module registers the existing read, edit, write, grep and find tool names;
registration preserves the registry's active tool selection. Each main or child
actor owns independent read observations. Exact edits do not require a prior
read, but an observed stale version is rejected. Read supports the Unicode
`column`/`maxChars` character window described in [CONTRACT.md](CONTRACT.md).
`preview` gives the file a write or edit call would change, before and after, so
approval prompts can show the diff; it applies exact edits only and skips files over 256 KiB.

After a checkpoint the module restores the working files the summary replaced:
files the summarized history read, wrote or edited successfully, modified files
first and then by latest access, at most five, read from disk as they are now.
Only regular UTF-8 files inside the session directory are restored, up to 1 MiB.
Credentials (`.env*` except templates, key and certificate files, `.netrc`-style
files, `.git`/`.ssh`/`.aws`/`.gnupg` contents) and files the retained history read
or wrote are skipped. Restored files use at most half the restore budget and are
truncated by whole lines with a pointer to read the rest.
