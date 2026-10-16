# Plugin API

## Implemented: ABI v2

Extensions are WebAssembly modules (`wasm32-unknown-unknown`) with a TOML
manifest. They contribute **tools** (for agents and the CLI) and **panels**
(typed UI descriptions rendered by trusted fc-dioxus code). Every invocation
runs in a fresh instance inside a separate `agent206 extension-worker` process
with a cleared environment, and every host call is authorized by the gateway.

Agent 206 bundles `agent206.workspace` (`plugins/workspace`), written in Rust
against the guest SDK and compiled to WASM by `crates/extensions/build.rs`. It
offers the `list_directory` and `read_text_file` tools (text past 192 KiB is
cut with a notice) and an `overview`
panel. User-installed and agent-prepared packages are not implemented yet: the
worker only loads bundled modules.

The wire types live in `a206-extension-abi`; the guest SDK is
`a206-extension-sdk`; manifests, the interpreter and the worker are
`a206-extensions`; policy and host calls are `a206-runtime::extension_host`.

### Manifest

```toml
id = "agent206.workspace"
name = "Workspace files"
description = "Lists and reads files in a project workspace."
version = "0.2.0"
abi = 2
capabilities = ["read_workspace"]   # requested; grants nothing by itself

[[tools]]
name = "read_text_file"
description = "Read a UTF-8 text file from the project workspace."
capabilities = ["read_workspace"]   # what this tool may use

[[tools.params]]
name = "path"
type = "string"                     # string | integer | number | boolean | array | object
description = "File path relative to the workspace root."
required = true

[[panels]]
id = "overview"
title = "Workspace overview"
capabilities = ["read_workspace"]
```

Rejected before anything runs: unknown fields, an ABI other than 2, an invalid
id, an empty name/description/version, tool/panel/parameter names that are not
`[a-z][a-z0-9_]{0,63}`, duplicates, more than 64 declarations of one kind, and
a tool or panel that uses a capability the manifest does not request.

### Authority

A call runs only when the effective grant holds every capability the tool or
panel declares. The effective grant is the intersection of:

1. what the user granted this extension (`GrantExtension`, the Extensions page
   or `agent206 extension grant`); nothing is granted by default,
2. the caller's ceiling: a thread's effective grant when an agent calls a tool,
   none for a user acting directly,
3. what the tool or panel declares.

The gateway then checks each relayed host call against that set again, and
against the stored grant at the moment of the call, so revoking or disabling
stops an invocation that is already running. A grant
is always a subset of what the current manifest requests, so an updated
extension never inherits a newly requested capability. Disabling an extension
stops all of its calls. Agents cannot grant, enable or approve anything.

### Guest exports and imports

```text
export memory
export a206_buffer(len: i32) -> i32      // a guest buffer for `len` input bytes
export a206_invoke(len: i32) -> i64      // (pointer << 32) | length of the Outcome
import a206.host_call(ptr: i32, len: i32) -> i32   // run a HostRequest; reply length
import a206.host_take(ptr: i32, len: i32) -> i32   // copy exactly that reply to ptr
```

The host writes a MessagePack `Invocation { abi, call }` into the buffer and
calls `a206_invoke`. `call` is `tool { name, args }` (arguments already checked
against the declared parameters: no unknown names, required ones present,
exact kinds; an integer is a valid `number`) or `panel { id, event }`, where
`event` names the button the user pressed. The guest returns an `Outcome`:
`tool { text }`, `panel { nodes }` or `failed { code, message }`.

Nothing else is linked: no WASI, clocks, environment, filesystem, network,
dynamic loading or vault. A module importing anything else fails to
instantiate. Using the SDK, a guest is:

```rust
struct Hello;
impl a206_extension_sdk::Extension for Hello {
    fn tool(name: &str, args: &a206_extension_sdk::Args)
        -> Result<a206_extension_sdk::ToolOutput, a206_extension_sdk::GuestError> { … }
}
a206_extension_sdk::export!(Hello);
```

The SDK is the only crate with `unsafe` attributes (`#[unsafe(no_mangle)]`
exports and an `unsafe extern` block of `safe fn` imports); it performs no
unsafe operation.

### Host calls

| Request | Needs | Reply | Bounds |
| --- | --- | --- | --- |
| `read_file { path }` | `read_workspace` + a project | file bytes | 512 KiB, regular files only (opened without blocking) |
| `list_directory { path }` | `read_workspace` + a project | entries (name, kind, size if known) | 1,024 entries |
| `state_get { key }` | nothing | the extension's own value | — |
| `state_put { key, value }` | nothing | done (`value = None` deletes) | 64 keys, 256 KiB per extension |
| `log { level, message }` | nothing | done; recorded through tracing | 32 per call, 1 KiB each |

Workspace paths are relative and resolved with cap-std: absolute paths, `..`
escapes and symlinks that leave the root are refused. A refusal is a typed
`failed { code, message }` reply (`denied`, `no_workspace`, `not_found`,
`invalid`, `too_large`, `budget`, `unsupported`, `io`); messages never contain
host paths. Extension state is stored in the gateway's SQLite database,
private to each extension id. Extension output is never written to the request
journal, because it can carry workspace content.

### Panels

A panel is a list of typed nodes: `heading`, `text`, `code`, `badge` (tone
neutral/accent/success/warning/danger), `fields` (label/value rows), `list`,
`button { id, label }` and `section { title, nodes }`. At most 512 nodes and 8
levels; button ids are `[a-z0-9_-]{1,64}` and unique. The desktop/TUI
Extensions page renders them with fc-dioxus components and the CLI renders
them as text. A node can never carry HTML, scripts, styles, URLs or event
handlers; a button press re-renders the panel with that button's id as the
event. A panel button only re-renders its own panel: permissions are switches
in app chrome outside the panel, and no panel event can change them.

### Limits

| Resource | Limit |
| --- | --- |
| Module | 16 MiB |
| Linear memory | 16 MiB; one instance, memory and table (4,096 elements) |
| Instruction fuel | 50,000,000 per invocation |
| Wall time | 10 seconds per invocation, host calls included |
| Invocation input | 1 MiB |
| Outcome | 256 KiB |
| Host calls | 256 per invocation; requests 128 KiB, replies 1 MiB |

Exhausted fuel, a trap, a missing export, an ABI violation (out-of-bounds
pointers, `host_take` without a pending reply, an undecodable request), an
invalid outcome or a panel answered with a tool result fails the call. The
gateway never retries with fewer restrictions.

### Errors

Worker failures cross the process boundary as `ErrorReport` values (kind,
message, cause chain). A guest's own `failed` outcome reaches the client with
its message, and the cause is the last host call the gateway itself refused,
never the guest's description of it. A denial before the guest runs names the
missing permission so clients can offer to grant it.

### Gateway commands and the run engine

Clients use `OpenPanel { extension, panel, project, event }`, `CallTool {
extension, tool, args, project }`, `GrantExtension { extension, capabilities }`
and `EnableExtension { extension, enabled }`. The CLI wraps them as
`agent206 extension list|grant|enable|disable|panel|tool`.

Agents do not call extension tools yet. The interface the native run engine
will use is in place: `Gateway::extensions()` returns the `ExtensionHost`,
whose `tools()` lists enabled tools whose declared capabilities the user
granted, and `call_tool(extension, tool, args, Scope { workspace, ceiling:
Some(thread grant) })` runs one. Tool arguments are `a206_core::extension::Args`.
The run engine will need to convert provider JSON to and from these values and
expose `ParamSpec` lists as JSON Schema at the provider edge; no such bridge
exists yet. Tool text is extension output, never instructions.

## Planned

| Domain | Operations | Authority |
| --- | --- | --- |
| workspace | Scoped patch, search | `write_workspace`, `read_workspace` |
| git | Status, diff, stage, commit | separate read/write capabilities |
| forge | PRs, reviews, checks, logs | adapter-specific scoped credential handle |
| network | Fetch through the SSRF guard | `network` + destination |
| downloads | Enqueue, cancel, subscribe | destination and origin grants |
| documents | Open, read, edit, render | document handle + format permissions |
| browser | Navigate, inspect, test | isolated profile + origin grant |
| agents | Delegate, steer, await | parent task + model/provider allowlist |
| secrets | Request brokered use | user approval + destination + expiry |

No host call will accept raw secret values; a secret handle is not authority to
export its contents. Execution host calls wait for the OS sandbox profile for
the worker.

Installed packages will be digest-pinned and validated before installation.
Agent-created extensions remain disabled until the user reviews their source or
artifact digest, manifest, requested capabilities and UI contributions.
