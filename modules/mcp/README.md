# MCP client module

`tc exec` starts configured servers concurrently without waiting for initialization.
Ready tools enter the live `ToolSource` catalog and use the ordinary tool scheduler,
validation and interceptors. Failed or disconnected servers retain an error in
`ServerStatus`; their tools leave the catalog. Dropping the session cancels clients
and kills each stdio server's Unix process group, including descendants.

`tc mcp list [--json] [--cwd DIRECTORY]` waits for startup to settle and reports
server states, tool names, origins, diagnostics and failures. Ctrl-C ends the wait.

## Configuration and contributions

User `config.toml` and trusted project `.agents/config.toml` (or `.claude/config.toml`) accept Codex-style
`[mcp_servers.NAME]` blocks. Supported keys are `command`, `args`, `env`, `env_vars`,
`cwd`, `url`, `http_headers`, `bearer_token_env_var`, `startup_timeout_sec`,
`tool_timeout_sec`, `enabled`, `enabled_tools` and `disabled_tools`. Unknown server
keys produce diagnostics. A later layer replaces the entire same-named server.
An untrusted project's blocks never contribute servers.

Each server needs exactly one of `command` or `url`. Timeouts accept positive
fractional seconds, defaulting to 10 seconds for startup and 60 seconds per call.
Progress resets the call timeout. Relative `cwd` resolves against the session
directory. Stdio receives a small default environment, `env_vars` forwarded from
the parent, then explicit `env` overrides. HTTP sends static headers and optionally
a bearer token resolved from the named environment variable. Redirects are disabled.

`McpSource::servers()` contributes parsed `McpServerDefinition` values with a user,
project or extension origin. `FileMcpSource::load` provides a TOML-backed source.
Project contributions are also gated by session trust. The first definition of a
duplicate server wins and records a diagnostic. `ElicitationHandler` is the
interactive boundary: `tc` asks through the session's interaction queue and
declines URL requests; the headless host uses `DeclineElicitation`.

## Tools

Tools are named `mcp__<server>__<tool>`. Every character other than an ASCII
letter, digit or underscore becomes an underscore; names over 64 characters or
colliding after sanitization are cut to 55 characters and receive an eight-digit
SHA-256 suffix. Both members of a collision receive suffixes. Expected names are
checked in at `tests/fixtures/tool-names.json`.

Resource tools are `list_mcp_resources`, `list_mcp_resource_templates` and
`read_mcp_resource`. Listings tag items with their server, paginate, remove metadata
and icons, and omit MCP App resources. Results preserve text and images, decode
text resources, save binary resources, and keep the complete MCP result envelope
without its top-level `_meta` as `ToolOutput.structured_content`. `isError` marks
an error result. Model-facing text beyond 20 KiB is cut in the middle and saved
in full. Tools are exposed directly through the live tool catalog, so MCP calls
use the same scheduler, permissions and cancellation path as other tools.

## OAuth

`tc mcp login <server> [--scopes SCOPE,...]` prints an authorization URL and waits
for a loopback callback. The native public-client flow uses protected-resource
and authorization-server discovery, dynamic registration and PKCE S256, with
state, callback path and issuer validation. Open the printed URL in a browser;
Ctrl-C cancels login. HTTPS is required except for loopback mock/local servers.
Redirects are disabled and OAuth responses are size-bounded.

Tokens are written atomically with mode 0600 under `$TIECHUI_HOME/mcp-oauth/`,
keyed by server name and URL. HTTP requests refresh expiring tokens under a file
lock, preserve/rotate refresh tokens and require the original issuer. Expired
tokens without a refresh token require another login. `tc mcp logout <server>`
removes local credentials; it does not revoke the remote authorization grant.
`/mcp` and `tc mcp list` show OAuth status without token values. Explicit static
Authorization headers and environment bearer tokens take precedence over OAuth.

`tc` is an MCP client only; it does not serve MCP.

## Dependencies and validation

`rmcp` is pinned to 3.5.0. The HTTP adapter uses the shared reqwest 0.12/rustls
client. `sse-stream` 0.2.6 is a direct dependency because rmcp's custom HTTP
backend exchanges its SSE types; it is already an rmcp transitive dependency. Test-only `axum` 0.8 hosts the rmcp loopback HTTP server.
OAuth uses the same reqwest client and hashing/base64 dependencies, not rmcp's
auth feature.

`just check` runs the in-repo stdio fixture, an rmcp HTTP server on loopback,
CLI tests with temporary `TIECHUI_HOME`, and the real MCP/interceptor integration.
Tests use fixture credentials and never contact external services.
