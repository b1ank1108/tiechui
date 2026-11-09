# Skills

The host loads `SkillsModule` on a blocking worker, passing its project trust
decision, user home, config home (`TIECHUI_HOME` or `~/.tiechui`), and registered
`SkillRoot` contributions. The module depends on no Tiechui runtime crate except
`tiechui-plugin-api` and registers its catalog through `ContextProvider`.

Roots, in discovery order:

1. `~/.agents/skills`, `~/.claude/skills`, `<config home>/skills`.
2. `<cwd>/.agents/skills`, `<cwd>/.claude/skills` (trusted projects only).
3. Contributions in registry and path order. The host must trust project plugins
   before activating them or accepting their root contributions.

Discovery needs no configuration. Use `modules.skills` to disable catalog
injection. Skills named in the user config's `skills.disabled` stay out of the
catalog, `$name` references, the resource picker and `/skills`. A disabled name
excludes every skill with that name.

`chui skills list --cwd <directory> [--json]` shows resolved metadata, root paths,
source scopes, disabled model invocation, skills disabled in the config, and
diagnostics. `modules.skills = false`
disables catalog injection in `exec`; listing remains available for inspection.

Within a root, a directory containing `SKILL.md` is a skill and ends recursion,
direct root Markdown files can also declare skills, hidden entries and
`node_modules` are skipped, ignore files are honored, and directory symlinks are
followed. Canonical files are deduplicated, keeping the first discovery path.
Different files with the same name coexist without collision diagnostics. A
symlink cycle is skipped with a diagnostic instead of recursing indefinitely.

Name and description validation warnings count UTF-16 code units, with limits of
64 and 1024. Missing descriptions exclude a skill; other validation warnings
retain it. YAML is parsed with [Saphyr](https://docs.rs/saphyr/0.3.3/saphyr/), a
maintained pure-Rust YAML 1.2 library with no `unsafe-libyaml` dependency. The
parser retries failed YAML with Codex-compatible quoting of unquoted scalar prose
such as `description: Build for AWS: ECS`. Quoted values and block scalar bodies
are preserved. Repair happens only in memory, and a failed retry reports the
original YAML error. Successfully parsed metadata is not whitespace-normalized.
`disable-model-invocation` must be a boolean `true` to hide a skill from the prompt.
Automatic discovery contributes metadata only; the model can load a matching file
with `read`. Explicit `$name` loads a skill only when exactly one enabled skill has
that name. A selected path-bound reference identifies a specific skill even when
names repeat. Both load the body on submission, including manual-only skills. Reads are
cancellable and limited to 128 KiB per skill. Unknown names, escaped references,
code spans/fences, shell drafts and references bound to another path stay literal.
After a native checkpoint summarizes a `read` of a skill, the module re-reads that
skill from disk within the host's restore budget, unless the retained history
still holds the same content.

`/skills` requests the shared resource picker through a generic UI event when an
interactive session is available. Same-name candidates show their paths and can
be searched by path. Selecting a skill inserts a bound reference and
preserves the draft; it does not submit a turn. Headless `RunCommand` lists
metadata; the CLI equivalent is `chui skills list`. `chui exec` treats its prompt
as ordinary model input, including text that starts with `/skills`.

The catalog lists skills in root order, user roots first. Same-name project and
user skills remain distinct, and ambiguous bare names never select one implicitly.

Golden tests compare catalog bytes, metadata, and validation diagnostics against
`tests/catalog.golden` and `tests/metadata.golden.json`, with the fixture
directory prefix replaced by `/skills-fixtures`.
