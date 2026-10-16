# Plugin API

## Implemented ABI v1

This is the deliberately small executable bootstrap ABI, not a claim that the
complete proposed extension system is available. Agent 206 bundles the inspector
as compiled WASM. It follows the same interpreter limits as future packages.

A plugin manifest is TOML:

```toml
id = "agent206.inspector"
name = "Workspace inspector"
description = "A bounded workspace information panel."
version = "0.1.0"
abi = 1
entry = "run"
capabilities = []
```

Unknown manifest fields, unsupported ABI versions, unsupported entry points and
invalid identifiers are rejected. `capabilities` names the `a206-core::Capability`
vocabulary. Declaring a capability does not grant it. The current gateway launches
only the bundled extension and requires its capability set to be empty.

The module exports:

```wat
(memory (export "memory") 1 16)
(func (export "run") (result i64) ...)
```

`run()` returns `(pointer << 32) | byte_length`, interpreted as unsigned bits.
The referenced bytes encode `Vec<(String, String)>` in MessagePack. The host
renders values as text, never as HTML/JS/CSS. ABI v1 has no input buffer, imports,
WASI, filesystem, network, clocks, environment, dynamic module loading or vault.
The manifest's `entry` is `run`. The output is at most 256 KiB and 64 fields.

Limits currently enforced:

| Resource | Limit |
| --- | --- |
| Module bytes | 16 MiB |
| Linear memory | 16 MiB |
| Instances / memories / tables | 1 each |
| Table elements | 4,096 |
| Instruction fuel | 1,000,000 per gateway invocation |
| Parent process timeout | 5 seconds |
| Output | 256 KiB, 64 string pairs |

The gateway runs an extension worker with a cleared environment. A trap, missing
export, prohibited import, invalid output or exhausted budget returns an error.
The parent does not silently rerun a failed extension with fewer restrictions.
The worker boundary is implemented; OS sandbox profiles and user package install
are not. Do not enable third-party packages by bypassing this restriction.

## Build and inspect

`cargo build -p agent206` builds the bundled WASM in `a206-extensions/build.rs`.
The source payload and generated WAT are deterministic. The `.wasm` artifact is
embedded in the app. `agent206 extension agent206.inspector` executes it through
the gateway's child-process path; the Extensions page uses the same command.

Tests exercise real WASM execution, denied ambient imports and fuel exhaustion.
The public `execute` function is useful for testing a future SDK, but is not an
installation or approval API.

## Target extension API (not implemented)

A later ABI adds versioned MessagePack inputs and host-call responses. Do not
assume the ABI-v1 output pairs are a stable representation for richer views.
The intended host domains are:

| Domain | Operations | Authority |
| --- | --- | --- |
| workspace | Read, scoped patch, search | canonical workspace + granted paths |
| git | Status, diff, stage, commit | separate read/write capabilities |
| forge | PRs, reviews, checks, logs | adapter-specific scoped credential handle |
| downloads | Enqueue, cancel, subscribe | destination and origin grants |
| documents | Open, read, edit, render | document handle + format permissions |
| browser | Navigate, inspect, test | isolated profile + origin grant |
| agents | Delegate, steer, await | parent task + model/provider allowlist |
| harness | Own task state, safe UI navigation | scoped actor identity |
| secrets | Request brokered use | user approval + destination + expiry |
| ui | Panels, commands, badges, viewers | validated declarative fc-dioxus tree |

No host call accepts raw secret values. A secret handle is not authority to export
its contents. Brokered requests bind actor, thread, tool, destination, use, expiry
and revocation generation. Approval UI belongs to trusted app chrome.

UI extensions will describe approved component kinds and typed properties. They
cannot override the security UI, capture arbitrary app input, execute scripts,
inject CSS or read another plugin's state. The same description will render through
the shared desktop/web/TUI tree, with documented capability fallbacks.

Download events will be an ordered typed stream, filtered by actor scope. Consumers
acknowledge cursors; replay gaps require a snapshot. Completion means verified bytes
were published, not merely that a network request ended.

Agent-created extensions remain disabled until the user reviews their source or
artifact digest, manifest, requested capabilities and UI contributions. Updating
an enabled extension cannot inherit newly requested capabilities automatically.
