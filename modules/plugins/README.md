# Plugin packages

`chui plugin add <path>` copies a local package to `$TIECHUI_HOME/plugins/<name>`
(default `~/.tiechui/plugins/<name>`). It records the canonical source and SHA-256
content hash in `plugins/.records/<name>.json`. The add command refuses to
overwrite existing installations. Use `chui plugin list [--json]` and `chui plugin remove <name>` to inspect
and remove installations.

`chui plugin marketplace add <git-url-or-path> [--ref REF]` registers a committed
Git snapshot; `marketplace list` lists registrations and `marketplace remove
<name-or-source>` removes one without deleting installed packages or the source.
`chui plugin add <name>@<marketplace>` installs from that snapshot. The catalog is
the first file found among the Codex layout (`.agents/plugins/marketplace.json`,
then `api_marketplace.json`), the Claude and Cursor layouts
(`.claude-plugin/marketplace.json`, `.cursor-plugin/marketplace.json`), and root
`marketplace.json`. Entries accept relative path strings or `source` objects of
type `local`, `url`, or `git-subdir`, including `path`, `ref` and `sha` for Git
sources. Catalog metadata supplies a fallback manifest when the package has none. Local marketplace paths must be Git repos;
uncommitted files are excluded. Git runs as a cancellable subprocess and manages
its own transport; Tiechui supplies no credentials, disables hooks, and isolates
Git's home and global/system configuration from the user's profile.

Installation records pin both marketplace/package commits and the installed
content hash. `chui plugin upgrade [name]` explicitly resolves the current source
again, including moved tags, and reports old/new commits, hashes and added,
modified or deleted files. Changed contents revoke capability grants; the next
interactive start asks again, and headless starts require a new grant. Existing
installs never update during startup. A source `sha` remains pinned on upgrade.
npm packages and npm marketplace entries are not supported; packages are either
declarative (T1) or run out of process (T2).

Each package is a module instance. Disable or enable it in `config.toml`:

```toml
[modules]
my-plugin = false
```

Project packages in `.agents/plugins/<directory>` and `.claude/plugins/<directory>` are discovered from the project
root to the current directory. They require `chui trust allow`; a module override
cannot bypass project trust. User packages take precedence over project packages
with the same name. Built-in module names are reserved.

Manifest selection prefers `.tiechui-plugin/plugin.json`, then a recognized Agent
Plugins root `plugin.json`, then `.codex-plugin/plugin.json`. An unsupported Agent
Plugins schema fails loading rather than falling back to the legacy manifest.
An unrelated root JSON document does not hide a legacy manifest.

For legacy manifests (`.tiechui-plugin/plugin.json` or
`.codex-plugin/plugin.json`), contributions load as follows:

- `skills` accepts a path or path array, with `skills/` as the default.
- `mcpServers` accepts a JSON file path or inline server map. The default is
  `.mcp.json`; wrapped `mcpServers` objects and plain maps are accepted.
- `hooks` accepts a file path, path array, inline hooks object, or array of hook
  objects. The default is `hooks/hooks.json`.
- Explicit paths must start with `./`, contain no parent traversal, and remain
  inside the package. Invalid contributions produce diagnostics.

Agent Plugins require `$schema` equal to
`https://agent-plugins.org/schemas/1.0.0/plugin.schema.json`. Their skills and
MCP contributions use fixed `skills/` and `mcp.json` locations. The MCP
file requires the corresponding `mcp.schema.json` schema and supports `stdio` and
`streamable-http` servers. Hook declarations come from `extensions["com.openai"]`,
or the `.codex-plugin/plugin.json` overlay when that extension is absent, with the
same default hooks path. Legacy MCP relative working directories resolve under
the package; Agent Plugins default to the package directory and support
`${PLUGIN_ROOT}` and `${PLUGIN_DATA}` placeholders.

Prompts and agents directories are retained and listed. They are not executed
or injected into context.

T2 packages declare `tiechui.apiVersion = 1` and an `extension` with a relative
executable `command`, optional `args`, capabilities, and tool/command/interceptor
contributions. See
[the Rust SDK guide](../../crates/tiechui-plugin-sdk/README.md) or
[TypeScript SDK guide](../../sdk/typescript/README.md). Runnable examples live in the
`tiechui-plugins` marketplace, which is not public yet. At first
interactive start, all declared capabilities require approval through the shared
foreground queue. Grants pin the package path and content hash in `$TIECHUI_HOME/plugin-grants/`;
content changes revoke them. Headless starts require an existing grant, and
untrusted projects start no T2 processes (`chui trust allow` is required). Delete
the package's grant file to require approval on its next start.

T2 freshness checks walk package metadata before each tool, command, interceptor,
and host request. The cached fingerprint includes relative paths, file types,
sizes, nanosecond modification times, inode/device and mode on Unix, and symlink
targets. A full content hash runs at authorization, at each process start, and
whenever metadata changes; a hash differing from the grant revokes it before the
call reaches the plugin. The per-call check is metadata-based: changes that
preserve every fingerprint field are detected at the next full hash. Symlinks
remain unsupported by content hashing. Checks are cancellable and run before
the plugin response deadline starts.

T2 tools and commands use `plugin_<package>_<name>` names. Interceptors use the
existing stages and fail closed on errors or their two-second deadline. Panels
and prompts use protocol data and the shared interaction queue. Status items
report startup, readiness, failures and bounded restart attempts. Reload and
exit terminate the plugin process group, including descendants.

Skills, MCP definitions, and hook sources go through the existing contribution
APIs. MCP server names retain their declared names and the existing duplicate
diagnostics. `modules.mcp = false` also disables contributed servers.

Plugin hooks require `chui hooks trust`, including installed user packages.
Approval pins both the normalized handler and the bundle contents, and
execution checks fresh contents before starting the command. Changing a script
invalidates approval even when the hooks JSON is unchanged. Fingerprints ignore
generated Python bytecode, `.venv`, and `.egg-info` directories. Symlinks and
special files are rejected. Installation omits `.git` metadata and
preserves executable permissions. Hook environments include `PLUGIN_ROOT`,
`CLAUDE_PLUGIN_ROOT`, `PLUGIN_DATA`, and `CLAUDE_PLUGIN_DATA`. Plugin data lives
outside the approved bundle in `$TIECHUI_HOME/plugin-data/<name>`.

Tests use all three manifest layouts, the existing MCP test server, and isolated
repository-local homes.
