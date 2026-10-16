# MCP client

Agent 206 can use tools from MCP servers the user installs. This is separate from
`agent206 mcp THREAD`, the harness *server* that exposes one conversation's
introspection tools to an external runtime.

Status: **experimental**. Stdio servers run under macOS `sandbox-exec`; other
platforms refuse to start them until an equivalent OS sandbox exists (Linux
Landlock/bwrap is tracked in `docs/parity.md`). The run engine that offers MCP
tools to models is not merged yet, so no model can call them today.

## Configuration

Servers are entries in `settings.toml` (`agent206 settings --apply FILE`, or the
Extensions page in the desktop and terminal clients):

```toml
[[mcp_servers]]
name = "files"                      # lowercase letters, digits, single hyphens
enabled = false                     # off until the user turns it on
executable = "/opt/mcp-files/bin/server"
runtime_root = "/opt/mcp-files"     # everything the server loads; readable by it
args = ["--stdio"]
capabilities = ["read_workspace"]   # authority the server process holds
project = "6c1f…"                   # required with workspace access
default_policy = "ask"              # allow | ask | deny for unlisted tools
timeout_seconds = 120               # per call, 1..=1800

[mcp_servers.env]                   # non-secret only; nothing is inherited
LOG_LEVEL = "warn"

[mcp_servers.tools]
read_file = "allow"
write_file = "deny"
```

Imported MCP servers are never enabled implicitly (`docs/imports.md`).

## Isolation

Each server is its own process group with a cleared environment, a private
temporary home and working directory, and stderr discarded. The sandbox allows
reading and executing only `runtime_root` and system libraries, writing only the
private scratch directory, and nothing else unless the entry holds:

| Capability | Sandbox effect |
| --- | --- |
| `read_workspace` | read the bound project's directory (also the working directory) |
| `write_workspace` | also write it; requires `read_workspace` |
| `execute` | run `/bin` and `/usr/bin` programs |
| `network` | outbound network, DNS and system trust store (untested on CI) |

Private application state and common credential stores (`~/.ssh`, `~/.aws`,
`~/.agent206`, keychains and agent configuration) are denied even inside a
project. Any other capability (secrets, git, agents, browser) is rejected:
those stay gateway-brokered. The JSON-RPC stream is bounded per line (8 MiB), the
tool listing by count (256) and size (512 KiB) and pages, and each result by size
(1 MiB) and time.

## Who may call a tool

The gateway authorizes every call against current state
(`a206_mcp::authorize`): the server is enabled; the tool's policy is not `deny`;
the calling thread's grant includes **every** capability the server holds; a
server with workspace access serves only threads in its bound project; and an
`ask` tool needs the user's approval for that call. An MCP server never adds
authority to a thread, and a model's request is never an approval.

## Lifecycle

`ConnectMcpServer` starts an enabled server, initializes the session and records
its tools; `DisconnectMcpServer` stops it until the user connects it again.
Enabled servers also start on first use by the run engine. Changing, disabling or
removing an entry stops its session. A gateway restart leaves every server
disconnected. Status (state, server-reported name/version, tools, last failure as
an `ErrorReport`) is part of the snapshot and announced with
`EventKind::McpServerChanged`.

CLI: `agent206 mcp-servers list | enable NAME | disable NAME | connect NAME |
disconnect NAME`.

## Run engine interface

The run engine calls two gateway methods (`crates/runtime/src/gateway/mcp_servers.rs`):

- `Gateway::mcp_offers(thread)` returns the tools the thread may be offered, each
  with its server and policy (`ask` tools need approval per call), plus servers
  that failed to start as `(name, ErrorReport)`. Tool names, descriptions and
  JSON Schemas are untrusted server text; a model-facing name such as
  `mcp__<server>__<tool>` is unambiguous because server names never contain `_`.
- `Gateway::call_mcp_tool(thread, server, tool, arguments, approval)` authorizes
  the call again and returns `a206_mcp::ToolOutput` (`is_error`, content blocks,
  structured JSON). Pass `Approval::Approved` only after the user approved this
  call in the trusted approval surface. Failures are `GatewayError::Mcp(McpError)`
  with stable kinds (permission denied, unavailable, protocol, timeout).

## Not yet implemented

- Streamable HTTP servers, and credentials for any server (vault-brokered auth).
- Linux and Windows isolation.
- Resources, prompts, sampling, elicitation and list-change notifications.
- Approval UI and model access (run engine).
