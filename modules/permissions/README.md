# Permissions

The enabled-by-default, mandatory module registers only a `ToolInterceptor`.
All local tools and the live MCP `ToolSource` pass through it. This is a policy
gate, not an OS sandbox.

```toml
[permissions]
mode = "auto"                     # rules | auto | yolo; user config only

[[permissions.rules]]
surface = "bash"
pattern = "cargo test *"
action = "allow"

[[permissions.rules]]
surface = "path"
pattern = "*.env"
action = "deny"
reason = "Keep environment files private"

[[permissions.rules]]
surface = "mcp"
pattern = "mcp__example__erase"
action = "deny"
```

Omission allows read-only and session tools (`read`, `grep`, `find`, `ls`,
`context`, `tool_search`, the goal, range-compaction, cron and shell task tools
and similar) and MCP metadata calls; everything else asks. Explicit `rules = []` replaces these
defaults. Read-only shell commands (`cat`, `ls`, `grep`, `jq`, `diff`, …)
without write redirects pass whenever the fallback would ask. Only deny accepts a reason.
The last matching rule wins within a surface, including repeated patterns.
The universal `*` supplies the fallback below explicit surfaces. Wildcards
support `*`, `?`, optional trailing ` *` arguments, and leading home expansion.
Path rules match lexical, relative, and canonical aliases together, preserving
rule order across aliases. `path_read`, `path_write`,
`external_directory_read`, and `external_directory_write` override bare-family
sugar. `edit` and MCP or custom tool `path` arguments check both directions. Across gates,
deny wins before prompting; a tool allow cannot bypass path/boundary rules.

Native tree-sitter-bash (`bash::inspect`) checks command units, variable literals,
substitutions, redirects, literal executable spellings, and static wrapper
payloads. Bounds are 32 nested wrappers, 256 parsed payloads, and 256 KiB per
program; exceeding them fails closed, even under yolo. A bash catch-all allow
suppresses synthetic parse/wrapper asks while retaining explicit asks/denies.
Inspection does not prove arbitrary dynamic scripts. Path extraction follows
command semantics: read-only commands and pure input redirects mark path operands
`read`; explicit file-writing commands (`touch`, `mkdir`, `rm`, `cp`, `mv`, `ln`,
`tee`, `chmod`, `dd`, `sed -i`, …) and output redirects mark destinations `write`
(with copy/move/link sources marked `read`); `git` extracts external and output
path arguments (`-C`, `--output`, `diff --no-index`, `worktree add`, `clone`);
other non-file commands rely on `bash` command rules and redirects instead of
treating arbitrary words as paths.

MCP rules match `server:tool`, `server_tool`, server, tool, `mcp_call`, and the
full `mcp__<server>__<tool>` name as aliases. Configured servers retain original
and sanitized spellings. A catch-all surface such as `mcp__example__*` also
applies. MCP path arguments receive cross-cutting path checks. Truncated/hashed
tools can always be matched by their exact exposed name.

## Modes

The mode settles asks that remain after explicit `deny` rules, session grants,
and approval reviewers. It comes from user `permissions.mode`, then `--mode` or
`--yolo` on `chui` and `chui exec` (`--yolo` conflicts with `--mode`, and on
`chui exec` with an explicit `--approval`; neither flag modifies `config.toml`).
`/permissions <mode>` switches it for the session, and the TUI saves it to user
`permissions.mode`. Project config cannot set it.

| Mode | Asks |
| --- | --- |
| `rules` (default) | Prompt the user. |
| `auto` | `write`/`edit` asks from the fallback rule become allows. Other asks go to a one-off review on the session's model, which sees the transcript and the call; allowed calls run, blocked calls return the reason as a tool error. After 3 consecutive or 20 total blocks, or when review fails or model access is unavailable, the user decides. Broad bash allows (catch-alls, interpreters, `npm run` and similar) count as asks. |
| `yolo` | Approve remaining asks (`outcome = "approved"`, `source = "yolo"`), including explicit `ask` rules and `protected_path`. Deny `fail-closed` rules without prompting, and decline MCP elicitations in the CLI host. |

Writes to protected paths ask in `rules` and `auto` unless an explicit user
`protected_path` rule allows them (any scope may deny): `.git`, `.agents`,
`.claude`, `.tiechui`, `.vscode`, `.idea`, `.gitconfig`, `.gitmodules`, shell
profiles, `.mcp.json`, `.ripgreprc`, and `.env` / `.env.*` (not `.env.example`,
`.env.sample`, `.env.template`). The `worktrees`, `plans`, and `state`
subdirectories under `.claude`, `.agents`, and `.tiechui` are exempt, and
workspace-interior accesses ignore ancestor directory names above `root` unless
`root` itself is inside `.git`, `.vscode`, or `.idea`, or is a protected
directory root. In `yolo`,
remaining `ask` gates (including explicit `ask` rules and `protected_path`) are
approved (`outcome = "approved"`, `source = "yolo"`), `fail-closed` rules from
invalid project config are denied without prompting (`Invalid project permission
configuration.`), and the CLI host declines MCP elicitations while preserving
`request_user_input` and folder, hook, subagent, and plugin trust/capability
prompts.

The main session reports its mode to the TUI as the `tiechui.permissions.mode`
extension event on restore and whenever it changes; child agents stay quiet.

| `chui exec --approval` | Behavior |
| --- | --- |
| `allow-listed` (default) | Asks the mode leaves open return an unavailable-approval tool error; explicit denies remain. |
| `deny` | Deny every intercepted call. |
| `yolo` | Use `--mode yolo` when `--mode` is absent. |

Denials reach the model as tool errors, for example
`Denied by policy: 'write' for path '/x' (rule '*'). Reason: Read only.`, and
do not end the turn.

`ApprovalRequester::request` receives the original call (including proposed
write/edit contents), matched rule and values, source, cwd, proposed session
scope, and a cancellation token. `chui` implements it through the session's
interaction queue, which shows one prompt at a time alongside MCP elicitations.
Decisions allow once, allow the displayed session scope, optionally widen a
directional scope to both directions, or deny with a reason. A `write` or `edit`
inside the workspace offers the whole workspace for both tools; elsewhere the
scope is the file's directory. Memory-only grants
are keyed by thread and `SessionEpoch`; new instances/epochs clear them. Late
approval results cannot grant into a different epoch; replay restores no grants.

Every evaluated gate queues a `permissions.decision` custom session entry with
version, call ID, outcome, source, surface, pattern, and origin. The actor
persists entries before tool results. Audit data excludes raw commands, file
contents, and tool arguments. Failed/interrupted checks record a denied error.

Trusted project config contributes explicit rules only; empty project config
never injects default grants. Invalid project permission fields produce a
diagnostic and clamp all non-`deny` rules (including default fallbacks and
read-only exemptions) to `origin = "fail-closed"` asks while preserving denies.
A malformed TOML document or unrelated invalid config still fails startup under
the existing loader, rather than loading partial settings.

**Trusted project rules can explicitly override user rules**, including
relaxing them. Project `modules.permissions = false` is ignored with a
diagnostic; only the user-level switch can disable the module.

Subagents inherit the parent policy sources and trust, with user and trusted
project agent `permission` frontmatter. Changing child cwd never loads new
policy; child grants cannot affect the parent or siblings. The CLI constructs a
fresh guard per child, forwarding approvals through the parent queue.

`/permissions` picks the mode and the TUI saves it; `/permissions show` is a read-only,
scrollable foreground panel showing the current mode, effective rules with origins, temporary grants and the latest 20 decisions (from a 100-entry buffer)
from this module instance. It never restores decisions or grants from a session
log and never displays raw tool arguments. `/status` remains available from the
CLI host when the permissions module is disabled.

## Limitations

Rules gate tool calls only; they do not hide skills from the catalogue or block
explicit skill input. The workspace targets Unix: path patterns fold Windows
separators and case, but this is untested.

## Verification

Tests live in `tests/permissions.rs`, the config tests, core extension tests,
the CLI unit and process tests, and `tests/pty/smoke.py`. They cover:

- default and empty policies, TOML order and validation, the mandatory-module
  default, and saving preferences without disturbing rule order or comments;
- last-match ordering, directional sugar order, unmatched tools asking, and
  explicit denials recorded without prompting, with exact denial text;
- lexical, relative, file URL and canonical symlink path aliases, hard path
  denials before external-directory asks, protected-path exemptions, and
  workspace ancestor isolation;
- semantic shell path extraction across read-only commands, explicit file
  writers, redirects, `git` path arguments and non-file commands; `rm` in
  chains, substitutions, absolute paths and wrappers; `command`/`exec` operands;
  wrapped programs keeping path checks; explicit asks/denies surviving
  permissive wrappers; and catch-all allows without synthetic wrapper or parse
  prompts;
- MCP and shell aliases sharing one pipeline, MCP path arguments and sanitized
  server names, MCP elicitation decline under yolo, and a live MCP tool denied
  under yolo;
- child cwd not bypassing the parent boundary, private child grants, and agent
  frontmatter policy with name traversal rejected;
- session scope and direction grants, epoch clearing, late approvals across
  epochs, nested commands after wrapper grants, mixed config/session
  directional grants, and pending-approval cancellation;
- trusted overrides, withheld untrusted rules, and invalid project rules
  clamping all non-`deny` rules to `fail-closed` (asking in `rules`/`auto`,
  denied without prompting in `yolo`);
- `--yolo` flag conflicts and non-persistence, yolo approval of asks and
  protected paths after reviewers, headless modes including end-to-end denial
  and continuation, inspection bounds under yolo, content-free auditing, and
  `/permissions` diagnostics.
