# MCP client

Agent 206 can use tools from MCP servers the user installs. This is separate from
`agent206 mcp THREAD`, the harness *server* that exposes one conversation's
introspection tools to an external runtime.

Status: **experimental**. Stdio servers run under macOS `sandbox-exec`, or on
Linux inside bubblewrap namespaces with Landlock applied inside them
(`a206-sandbox`, the same layers as the coding backends). Other platforms refuse
to start them until an equivalent OS sandbox exists. On Linux a missing
bubblewrap or a kernel without Landlock is a startup error, never a weaker
sandbox. The native run engine offers MCP tools to models (see below), so models
reach them wherever servers can be launched: macOS and Linux.

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
LOG_LEVEL = "warn"                  # HOME, TMPDIR, PATH, LANG, NO_COLOR are reserved

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
| `network` | outbound network, DNS and system trust store |

Without `network` a Linux server has its own empty network namespace and
Landlock allows no TCP connection; with it, the server shares the host's network
and sees only the name-resolution files and certificates from `/etc`. On both
platforms `network` currently includes the host's loopback, so grant it only to
servers you would trust with local services. On Linux the PID namespace stays
private either way, so a server cannot inspect the gateway's or other processes. Both
sandboxes check resolved paths, so a symlink inside the project that points
outside it reaches nothing the server could not already read. CI drives a fixture
server through the real sandbox on macOS and on Linux, checking project and
state visibility, symlink escapes, loopback reachability with and without
`network`, and running `/bin/sh` with and without `execute`
(`crates/mcp/tests/sandboxed.rs`).
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
authority to a thread, and a model's request is never an approval. The check
repeats after every gateway state change while a call runs; a call the thread may
no longer make is cancelled (`McpError::Revoked`) and the server receives an MCP
cancellation notice.

## Lifecycle

`ConnectMcpServer` starts an enabled server, initializes the session and records
its tools; `DisconnectMcpServer` stops it until the user connects it again.
Enabled servers also start on first use by the run engine. Changing, disabling or
removing an entry stops its session. A server that exits or closes its pipe is
marked failed (`McpError::Exited`); its next use starts it again. Concurrent
first uses share one start. A gateway restart leaves every server
disconnected. Status (state, server-reported name/version, tools, last failure as
an `ErrorReport`) is part of the snapshot and announced with
`EventKind::McpServerChanged`.

CLI: `agent206 mcp-servers list | enable NAME | disable NAME | connect NAME |
disconnect NAME`.

## Model access through the run engine

`McpToolHost` (`crates/runtime/src/mcp_tools.rs`) is registered by `Gateway::open`
beside the WASM extension host; `docs/run-engine.md` describes its contract. In short:

- A conversation is offered the tools of each enabled, not disconnected server it
  may use: its grant covers the server's capabilities, the scope's capabilities
  (that grant within the mode ceiling, intersected with every ancestor's) do too,
  and a workspace server is bound to its project. Other servers are neither listed
  nor started. Listed tools are still denied until the user permits them in the
  conversation.
- Names are `mcp__<server>__<tool>`, every tool-name character outside
  `[A-Za-z0-9_-]` written as `_`. A name over 64 characters, an empty tool name, or
  a name two tools of the server would share is not offered and is reported in the
  listing's `unavailable` (source `mcp__<server>`); names are never truncated.
- The server's per-tool policy is the host policy, so the enforced permission is
  the most restrictive of it (Ask by default) and the conversation's settings. Deny
  tools are listed, with an effective Deny, and never offered to a model.
- A server that cannot start is listed in `unavailable` with its start failure as
  the cause; the other servers' tools are still offered.
- Each call goes through `call_mcp_tool`, which authorizes it against current state
  and revokes it on a later change, after the host checks the scope's
  capabilities. `Approval::Approved` is passed only when the user approved that
  call. Content blocks become text (binary content is described, not inlined;
  structured JSON is used when there is no content); `isError` results are tool
  failures, not host failures. Cancelling the run drops the call, which sends the
  server an MCP cancellation.

## Gateway interface

The run engine's MCP host calls two gateway operations (`crates/runtime/src/gateway/mcp_servers.rs`):

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
  To cancel a call when its run is cancelled, drop the future; the server
  receives an MCP cancellation notice, and its eventual reply is discarded.

## Not yet implemented

- Streamable HTTP servers, and credentials for any server (vault-brokered auth).
- Windows isolation.
- Network grants that exclude the host's loopback services (see Isolation).
- Resources, prompts, sampling, elicitation and list-change notifications.
- Native-run tests launch no real server: they serve the Rust fixture in process
  (`Gateway::attach_mcp_session`, behind the runtime's `test-support` feature).
  Launching through the macOS and Linux sandboxes is tested in `a206-mcp` only.
