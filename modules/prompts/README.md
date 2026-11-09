# Prompts

Prompt templates: each Markdown file becomes a slash command whose text is sent
as a user message. Enabled by default; `[modules] prompts = false` removes the
commands.

The host loads `PromptsModule` on a blocking worker with its project trust
decision and config home (`TIECHUI_HOME` or `~/.tiechui`), and registers it after
every other command source, so built-in, module and plugin commands keep their
names.

Roots, in precedence order:

1. `<config home>/prompts`.
2. `<cwd>/.agents/prompts`, `<cwd>/.claude/commands` (trusted projects only;
   otherwise a warning).

Each `*.md` file directly in a root is a command named after the file stem;
stems with whitespace or a leading dot are skipped with a warning, as are names
an earlier root already defines. Frontmatter is read as `key: value` lines, with
JSON-style double-quoted or YAML single-quoted values; `description` (default:
the first line of the body) and `argument-hint` are used and other keys are
ignored. Files are read when the session starts; `/reload` picks up changes.

`/name arguments` replaces each `$ARGUMENTS` in the body with the arguments, or
appends them after a blank line when the body has none. The command applies
`SessionEffect::Submit`, which queues the text as a follow-up user message: it
runs at once when the session is idle and after the current turn otherwise.
Without an interactive session the command prints the text instead. `$skill`
references in the text load like typed ones.

`render` writes the frontmatter `read` parses back; the app-server's
`prompts/save` uses it.
