# Editing tools

The module registers the existing read, edit, write, grep and find tool names;
registration preserves the registry's active tool selection. Each main or child
actor owns independent read observations. Exact edits do not require a prior
read, but an observed stale version is rejected. Read supports the Unicode
`column`/`maxChars` character window described in [CONTRACT.md](CONTRACT.md).
`preview` gives the file a write or edit call would change, before and after, so
approval prompts can show the diff; it applies exact edits only and skips files over 256 KiB.
