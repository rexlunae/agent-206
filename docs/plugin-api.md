# Plugin API

## Implemented: ABI v2

Extensions are WebAssembly modules (`wasm32-unknown-unknown`) with a TOML
manifest. They contribute **tools** (for agents and the CLI) and **panels**
(typed UI descriptions rendered by trusted fc-dioxus code). Every invocation
runs in a fresh instance inside a separate `agent206 extension-worker` process
with a cleared environment, and every host call is authorized by the gateway.
On Linux that process is also confined by bubblewrap and Landlock, and on macOS
by a Seatbelt policy, with no network, no project files and nothing to execute
but itself; host calls are its only reach. Without the platform's sandbox, calls
fail rather than run unconfined. Windows workers are not yet OS-confined.

Agent 206 bundles `agent206.workspace` (`plugins/workspace`), written in Rust
against the guest SDK and compiled to WASM by `crates/extensions/build.rs`. It
offers the `list_directory`, `read_text_file`, `search_text`,
`write_text_file` and `edit_text_file` tools and an `overview` panel. The
write tools need a separate `write_workspace` grant. It also bundles
`agent206.git` (`plugins/git`) with `git_status`, `git_log` and `git_diff`
tools under the `git_read` grant and a `git_commit` tool under its own
`git_write` grant, and `agent206.web` (`plugins/web`) with a
`fetch_url` tool under the `network` grant. Bundled tools cut text past
192 KiB with a notice (the SDK's `excerpt`), so a long result never exceeds
the outcome limit. Users can also install third-party packages (see
[Installed packages](#installed-packages)), including packages agents prepare
for review ([Agent-prepared packages](#agent-prepared-packages)).

The wire types live in `a206-extension-abi`; the guest SDK is
`a206-extension-sdk`; manifests, the interpreter and the worker are
`a206-extensions`; policy and host calls are `a206-runtime::extension_host`.

### Installed packages

A package is a `plugin.toml` and its compiled `wasm32-unknown-unknown` module.
The user installs it with `agent206 extension install <plugin.toml> <module.wasm>`
or by choosing both files on the Extensions page; both send
`Command::InstallExtension`. Installing is a user operation, never an agent one.

- Before anything is stored the gateway validates the manifest, refuses ids
  starting with `agent206.` (reserved for bundled extensions), and limits the
  manifest to 64 KiB and the module to 3 MiB with a WebAssembly 1.0 header. The
  module itself is compiled only inside the confined worker.
- The package is stored under `<data dir>/extensions/<digest>/`, where the
  digest is SHA-256 over the manifest text and the module. The snapshot records
  `PluginSource::Installed { digest }`, and every load checks the stored files
  against it, so a package altered on disk fails with `Conflict` and never runs.
  The worker receives the verified module bytes in its `Start` frame; it has no
  access to the gateway's files.
- A new package arrives disabled with nothing granted. Installing a different
  package with the same id is an update: it is disabled again, keeps only the
  grants its new manifest still requests, and replaces the earlier copy.
  Installing the identical package again changes nothing, except restoring a
  stored copy that was altered.
- `agent206 extension uninstall <id>` (or Uninstall on its card) removes the
  package and the extension's stored state. Bundled extensions can be disabled
  but not uninstalled.
- The Extensions page marks installed packages as "Installed by you" with their
  id and digest, so an installed package cannot pass for a bundled one by name.

### Agent-prepared packages

An agent that built an extension in a project offers it with the native
`extension_propose` tool, naming the project's `plugin.toml` and `.wasm` files.
Like every tool it is denied until the user permits it in the conversation, and
it reads the two files under `read_workspace`, confined to the project root.

- The package is validated exactly as for an install and stored under its
  digest, but recorded in `Snapshot.extension_proposals` with the conversation
  that prepared it, not among the extensions. A proposal is never offered,
  loaded or granted anything. The tool can neither install, enable nor grant.
- At most 8 proposals wait at once. A package with the id of a waiting proposal
  replaces it, and proposing the package already installed is refused.
- The user reviews it on the Extensions page ("Prepared by an agent", with its
  digest, conversation, requested permissions and tools) or with
  `agent206 extension list`. `Command::InstallProposal { digest }` (Install, or
  `extension install-proposal <digest>`) verifies the stored files and installs
  it disabled with nothing granted. An update an agent prepared also drops every
  grant of the version it replaces, so the user reviews each permission again.
  `Command::DismissProposal` (Dismiss, or `extension dismiss <digest>`) deletes it.
- Unrecorded package files are pruned when the gateway starts; recorded
  proposals are kept.

Not yet: registry or URL installs, signatures, and a pre-install audit.

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

A manifest that requests `network` must list where it connects, and `hosts`
without `network` is refused:

```toml
capabilities = ["network"]
hosts = ["api.example.org"]   # exact lowercase DNS names, at most 32
# hosts = ["*"]               # or `*` alone: any public HTTPS host
```

IP literals, ports, wildcards inside a name, single-label names and
duplicates are refused. The Extensions page shows the hosts beside the
permission.

### Authority

A call runs only when the effective grant holds every capability the tool or
panel declares. The effective grant is the intersection of:

1. what the user granted this extension (`GrantExtension`, the Extensions page
   or `agent206 extension grant`); nothing is granted by default,
2. the caller's ceiling: a thread's effective grant when an agent calls a tool,
   none for a user acting directly,
3. what the tool or panel declares.

The gateway then checks each relayed host call against that set again, and
against the stored grant at the moment of the call. Revoking stops an
invocation already running, and disabling refuses every host call, state and
logs included. A grant
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
| `skill_list` | bundled `agent206.skills` only + enabled builtin/current approval pins | complete index as UTF-8 file bytes | 64 KiB, no partial success |
| `skill_read { name, path }` | same builtin/pin checks; no workspace authority | approved instructions or supporting UTF-8 file bytes | 64 KiB result, relative paths only, no traversal or symlinks |
| `read_file { path }` | `read_workspace` + a project | file bytes | 512 KiB, regular files only (opened without blocking) |
| `list_directory { path }` | `read_workspace` + a project | entries (name, kind, size if known) | 1,024 entries |
| `search { path, query, max_matches }` | `read_workspace` + a project | matches (path, line, text) and whether it stopped early | literal, case-sensitive query of 1 KiB; 500 matches, 5,000 files, 32 MiB read; 10,000 directories, 64 levels; skips `.git`, symlinks, files over 512 KiB and non-UTF-8 files; results in path order |
| `write_file { path, contents, overwrite }` | `write_workspace` + a project | done | 512 KiB; written to a temporary file first, so readers never see a partial file; a new file is published with a hard link that refuses one created meanwhile, an existing regular file is replaced by rename keeping its permissions; without `overwrite` an existing file is `exists` |
| `edit_file { path, old, new }` | `write_workspace` + a project | done | UTF-8, 512 KiB; `old` must occur exactly once, else `conflict`; also `conflict` when the file changed or was removed after it was read |
| `create_directory { path }` | `write_workspace` + a project | done, also when it exists | missing parents are created |
| `git_status` | `git_read` + a project | branch, `HEAD` id, changed paths (staged, unstaged, untracked) | 1,000 entries; 5 s per git call; renames not tracked; submodules ignored |
| `git_log { max }` | `git_read` + a project | commits from `HEAD`: id, summary, author name (no email), author time | 200 commits |
| `git_diff { path, staged }` | `git_read` + a project | unified diff of one file (`HEAD`→index when `staged`, else index→working tree; a symbolic link compares its target text), or `binary` | 512 KiB per side (checked before a blob is read), 192 KiB of text, 3 context lines; at most 8,192 input lines combined and 1,000,000 line pairs, otherwise a budget failure |
| `git_commit { message, paths }` | `git_write` + a project | the new commit: id, summary, author name, time | 1 to 100 paths, 16 KiB message, 512 KiB per file and 8 MiB together; 5 s, but once the commit is written it is reported, even past the deadline; see [Commits](#commits) |
| `fetch { url }` | `network` + a declared host | final status (any, not only 2xx), `Content-Type`, body, whether it was cut | 512 KiB body; 8 s including redirects; 5 redirects |
| `state_get { key }` | nothing | the extension's own value | — |
| `state_put { key, value }` | nothing | done (`value = None` deletes) | 64 keys, 256 KiB per extension |
| `log { level, message }` | nothing | done; recorded through tracing | 32 per call, 1 KiB each |

The skill imports are additive ABI v2 requests used by the bundled skills guest.
Other extensions receive `denied`, even when they hold workspace permissions.
The gateway rechecks approval after a read and rejects results from changed pins.
Skill filesystem operations run in a bounded blocking pool; cancelling the call
kills the WASM worker and discards the read result. A stalled OS read retains a
pool permit until it exits, so it cannot exhaust async executor workers.

Git calls use gitoxide on the repository at exactly the project root (never a
parent directory): a `.git` directory there, or a bare repository that is the
root itself. All repository metadata must be inside the project, so a `.git`
file or symlink (linked worktrees, submodule checkouts), a `commondir`,
`objects/info/alternates` or a symbolic link anywhere in the metadata is refused as `unsupported`,
and `core.worktree` is ignored. They read no system or global git configuration and no
`GIT_*` environment, and nothing the repository configures can run: no hooks,
filter, diff, textconv, fsmonitor or credential programs. The working-tree side
of a diff is the raw file, without line-ending or filter conversion.

### Commits

`git_commit` works like `git commit --only <paths>`: it commits the
working-tree content of exactly the named paths on top of `HEAD` (or as the
first commit of an unborn branch), records a named path that no longer exists
as removed, and updates those paths in the index so `git status` agrees.
Other staged changes stay staged. Files are read like workspace writes are
made, without following a symbolic link on the way; a symbolic link itself is
committed as a link. A new path ignored by the repository's or the user's
excludes is refused, as `git add` would without `--force`.

Where `git commit` would run something or add a protection gitoxide cannot,
the commit is refused as `unsupported` rather than made without it: signed
commits (`commit.gpgSign`), commit hooks (`pre-commit`, `prepare-commit-msg`,
`commit-msg`, `post-commit` in `.git/hooks` or `core.hooksPath`), files whose
attributes ask for a `filter`, `ident` or `working-tree-encoding`, carriage
returns in files git would convert (`text`, `eol`, `crlf` or `core.autocrlf`),
a merge, rebase or similar in progress, conflicts, and split or sparse
indexes. To decide this, and for `user.name` and `user.email` (required),
commits read the user's system and global git configuration and excludes file
as data, with repository settings taking precedence; nothing they name runs.
The index lock is held throughout, so a concurrent git command makes the call
fail with `conflict`, and so does `HEAD` moving underneath it; in both cases
nothing is committed. If the index cannot be written after the commit, the
failure names the commit that was made.

A fetch is a `GET` through the same guard as downloads
(`crates/runtime/src/network.rs`). The URL and every redirect must be
credential-free HTTPS on port 443 without a fragment, on a host the manifest
declares, and every address the host resolves to must be public. The
connection is pinned to those checked addresses, so a second DNS answer cannot
redirect it. It sends no cookies, credentials or proxy settings. A refusal
never echoes the URL, because links can carry tokens.

Workspace paths are relative and resolved with cap-std: absolute paths, `..`
escapes and symlinks that leave the root are refused. Writes also refuse any
path through `.git` (in any letter case) or through a symlink, and the
gateway serializes extension writes so an edit cannot interleave with another. A refusal is a typed
`failed { code, message }` reply (`denied`, `no_workspace`, `not_found`,
`invalid`, `too_large`, `exists`, `conflict`, `budget`, `unsupported`, `io`,
`timeout`, `network`;
a host that predates a code sends it as `other`); messages never contain
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
| Host calls | 256 per invocation; requests and replies 1 MiB |

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

Native runs call extension tools through the default `ExtensionToolHost`
(see `docs/run-engine.md`): enabled extensions' granted tools are offered as
`ext__<extension id>__<tool>` with their `ParamSpec`s as a JSON Schema object,
and each call runs `call_tool(extension, tool, args, Scope { workspace,
ceiling: Some(thread grant) })` after the conversation permits the tool.
Provider JSON arguments convert kind for kind to `a206_core::extension::Args`.
Tool text is extension output, never instructions.

## Planned

| Domain | Operations | Authority |
| --- | --- | --- |
| workspace | Multi-hunk patch | `write_workspace` |
| git | Stage without committing, branches, push | `git_write` (push also needs a forge credential handle) |
| forge | PRs, reviews, checks, logs | adapter-specific scoped credential handle |
| network | Requests other than `GET`, headers, brokered credentials | `network` + destination + secret handle |
| downloads | Enqueue, cancel, subscribe | destination and origin grants |
| documents | Open, read, edit, render | document handle + format permissions |
| browser | Navigate, inspect, test | isolated profile + origin grant |
| agents | Delegate, steer, await | parent task + model/provider allowlist |
| secrets | Request brokered use | user approval + destination + expiry |

No host call will accept raw secret values; a secret handle is not authority to
export its contents. Execution host calls will run their commands in a separate
sandboxed process, never in the worker, and wait for the run engine's native exec.

Agent-created extensions will remain disabled until the user reviews their source or
artifact digest, manifest, requested capabilities and UI contributions.
