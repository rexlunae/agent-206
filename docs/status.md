# Implementation ledger

Agent 206 is a working first foundation. It is not yet a complete successor to
RustyClaw. This ledger prevents the UI, docs or future contributors from mistaking
implemented structure for finished agent execution or security parity.

## Current implementation

| Area | Available now | Remaining work |
| --- | --- | --- |
| Rust architecture | Seven small crates, typed shared commands, forbidden application unsafe, pinned dependencies | Further extraction as capabilities grow; measured build/runtime budgets |
| Desktop | Native macOS app bundle, shared fc-dioxus interface, automatic local gateway startup, shared lifecycle controls | Windows named pipes; Linux desktop packaging and hardware validation |
| Gateway lifecycle | Shared Gateway page and controller, status from the actual process, start/stop/restart/reconnect, durable deliberate-stop marker, cleanup before acknowledgement, bounded control receipts; web uses an authenticated user-only binary endpoint | Remote SSH acceptance, stronger cross-process lifecycle coordination, encrypted enrollment and authentication; running is not security readiness |
| Initial setup/import | Source-backed security parity and reviewed import contracts; RustyClaw, Cline, Claude Code, Codex, DeepSeek Harness identified | Key-file vault setup, policy migration, a guided setup wizard and versioned import adapters are not implemented; vault creation, unlock and TOTP enrollment exist in the CLI and the desktop/TUI Security page (see Vault) |
| CLI | Headless build, system status, nested gateway lifecycle/status, chat, projects, settings, catalog refresh, downloads, text documents, Git status, vault, ACP/MCP, renderer entry points and ui alias | Full RustyClaw-compatible command families in docs/cli.md; secure setup/import/auth/doctor and remaining parity |
| Gateway setup | Clients connect only after first-time setup (desktop/TUI screen or `gateway setup`) saves a local or SSH target in `connection.toml`; unknown SSH hosts require fingerprint confirmation into the profile's own known_hosts; changed keys are refused; typed `ConnectionError`s are traced and shown | Web clients use the bridge's server-side setup; gateway enrollment (vault/TOTP) is a separate gate |
| SSH | Ordinary OpenSSH, strict host verification, configured identities, keepalives, remote stdio bridge |  remote installers/updaters, remote acceptance test |
| Wire protocol | Versioned bounded MessagePack, fragmented/coalesced reads, strict decode, durable request receipts | Negotiated feature sets, pushed subscriptions, full reconnect/resume UX |
| Errors/diagnostics | Per-module `thiserror` enums with source chains and explicit kinds up to each handling point (gateway validation, conflicts and missing resources are classified, tested for gateway replies); structured `ErrorReport` (kind, message, causes) in replies, lifecycle reports, downloads and failed runs, compatible with message-only payloads; `tracing` events in gateway, controller and clients; desktop/TUI error notices with expandable causes; CLI prints the full chain; URL redaction in transport errors; `let _ =` discards denied by clippy | Typed errors in the app binary below its top level, localized messages, error telemetry/export, retry guidance per kind |
| Persistence | One gateway writer, SQLite WAL snapshots/journal, revision checks, restart recovery, explicit replay gaps | Paginated/archived history, migration tooling; current snapshot is bounded below 4 MiB |
| Chat | API-discovered model selection, native/compatible text generation, cancellation, persisted text attachments (8 files/128 KiB), API-advertised reasoning effort; streamed text from external backends | Native provider streaming, image/PDF/audio attachments, native tool-call loop, retries with explicit external-effect semantics |
| Providers/models | Configurable hosted, local and custom preset library with GUI editing and TOML overrides; compatible, Anthropic and Gemini text APIs; bounded catalog pagination, provenance/expiry and refresh; unknown facts remain unknown | Isolated authenticated provider broker, accounting integrations and real vendor acceptance; model facts stay API-only |
| Coding-agent backends | Optional Codex app-server and Claude Agent ACP client adapters; per-thread selection/grants, macOS OS isolation, restricted model proxy, streamed text, operation summaries, bounded I/O, cancellation/process-group teardown; exercised with independent mock peer executables | Authenticated broker integration, real vendor-runtime acceptance, Linux/Windows isolation, installation/updating, session resume, interactive approvals and external subagent policy; runtimes are not bundled |
| Usage/context | Compact composer widgets for last-request context used/limit, reported response cost, API prices and reported account/token balances; detail popover; usage clears when model selection changes | Compaction/context controls, cumulative accounting, richer history graphs and provider currency semantics |
| Modes | Chat, Plan, Code, Review, Research; editable system prompts; mode ceilings | Mode-specific run engines, tools and approval UX |
| Projects/subagents | Project coordinator conversations, parent-linked delegated threads, explicit model allowlists, narrowed grants, revocation checks | Autonomous project management, task assignment, scheduler/budgets, steering and hierarchical cancellation |
| Concurrent UI | Resizable navigation/chat/inspector, two visible conversations, independently scoped controls, conversation title editing, searchable complete conversation list, compact integrated composer controls, themed raised/feature surfaces | More than two visible threads, saved layouts and comprehensive terminal interaction QA |
| Navigation | Typed Dioxus Router routes, persistent shell, fc-dioxus links, Back/Forward controls, conversation and split-view URLs, deep-link reloads, loading/missing recovery | Cross-profile deep-link handling, draft/form preservation across route unmounts, terminal interaction acceptance |
| Vault | SecureStore-compatible encryption, locked state, redacted nonserializable secret type, private atomic writes, interactive standalone CLI; library-level typed credentials in RustyClaw's layout, all five access policies plus disabled/agent switch, verified TOTP enrollment with replay protection, auth throttle and reveal step-up window; the CLI challenges enrolled vaults ; a separate broker process owns the vault, serves the CLI and performs destination-bound provider requests for a gateway paired per unlock; the desktop/TUI Security page creates, unlocks (with the authenticator challenge), locks and pairs a local profile's vault and enrolls TOTP through the same broker requests (`a206_runtime::vault`, shared with the CLI; runtime tests against a real broker and gateway, UI tests of offered actions and the remote notice only) | Vault controls for a remote (SSH) gateway, desktop/TUI credential and policy management and TOTP removal (CLI only), QR display of the enrollment key, agent approval UI, recovery codes, key-file migration, remote broker over SSH, credentialed external backends and browser injection |
| Security model | Deny-by-default grants, no secret-bearing agent tool, restricted MCP, bounded worker, SSRF-resistant public downloads, capability-scoped text reads | Full RustyClaw defense parity, OS sandbox profiles, leak/injection boundary scanning, adversarial review |
| WASM | Genuine compiled bundled inspector, separate child, no imports/WASI, fuel/memory/output/time bounds | General SDK inputs/host calls, user/agent package creation and reviewed installation, most tools as extensions |
| Plugin UI/API | Versioned manifest and executable v1 documented; trusted text-only extension panel | Rich validated component descriptions, panel/viewer/command contributions and subscriptions |
| Downloads | HTTPS monitor, progress events, cancellation and partial-file cleanup, SHA-256 verification, atomic no-clobber publication, transfer library or ~/models | Range resume, persisted ETags, retries, remote model transfer, agent-scoped subscriptions |
| Joshua | Owned local/SSH child start/stop, loopback forwarding, user-selected file/port, API model discovery, owned provider retirement on stop/observed exit/restart | Installation/updating, verified remote supervision/readiness, one-step model switching, real model/remote-host acceptance tests |
| Documents | UTF-8 text/Markdown source preview scoped by cap-std to a project | WASM format plugins, editing/rendering, PDF/office viewers, ArtCraft evaluation |
| Git/PR/CI | Scoped read-only Git status with bounded output and disabled fsmonitor | Diff/stage/commit workflows, forge-neutral PR/review/check APIs, multiple CI/CD adapters |
| Browser | fc-dioxus sandboxed preview with opaque origin, browser chrome and navigation | Isolated automation profile/driver, test tooling, downloads integration and approved vault injection |
| Theme/branding | New vector mark and generated PNG/ICO/ICNS bundle icons, future-css ThemePicker, designs/palettes/scheme/contrast saved as TOML | User theme import/editor after toolkit model support |
| Standards/docs | AGENTS.md, architecture, executable plugin ABI, provenance/attribution, README, CI workflow, RustyClaw parity gap list (docs/parity.md) | Keep this ledger current as each gate lands |

## Current scope

Gateway work targets the desktop and TUI clients first. Like RustyClaw, remote
gateways are reached by SSH process transport; the wire format is Agent 206's own
versioned MessagePack protocol, not RustyClaw's codec (see docs/upstream.md). The
web client controls a local gateway through the authenticated local web bridge.
Remote gateway access from the web client is deferred: there is no
SSH-over-WebSocket bridge, and the web client must not be described as reaching
remote gateways until one exists.

## Explicitly disabled

Credential-backed providers work only through the vault broker: the vault must be
unlocked, the gateway paired for that unlock, and the credential bound to the
provider's exact origin, or the request fails with an explanation. The gateway never reads a credential value, and agents
still have no credential use. External coding backends still refuse credentialed
providers. Browser
preview has no credential injection. Third-party extension installation and
capability-bearing imports are unavailable. New coding conversations have no
execution or filesystem grants. A coding mode label alone does not claim a working
autonomous native coding agent. External coding backends require separately configured
runtimes and providers; authenticated dispatch and external delegation remain
disabled. No subscription login is offered for Claude Agent.

## Validation evidence

Gateway lifecycle tests use the real executable and independent clients: idempotent
start, status/reconnect, stop surviving polling/new clients, stdio bridge exit,
restart/profile persistence, authenticated web controls, rejected cross-origin
requests, denied raw shutdown and duplicate-restart receipts. Regressions cover
offline stop for fresh/existing profiles, profile-lock coordination, shutdown replay
after restart, lost shutdown acknowledgements, and a real provider request held
past the cleanup deadline: repeated Start preserves its reply and failed Stop
restores work admission. Concurrent CLI stops and independent shutdown requests
queued behind held API I/O all receive success, with accepted replies drained before
daemon exit. Native startup uses safe asynchronous stdin readiness so closing the
gateway does not strand a blocking
stdio reader. Live browser checks cover start/stop/reload/restart on a private profile.
The header and workspace home expose gateway navigation. A mismatched older web
bridge reports an actionable restart/authorization message. Legacy gateways that
close on unknown status requests can still be stopped through their existing
Shutdown command; current clients persist stop intent under the released lock.
An independent lifecycle gate excludes automatic startup throughout the legacy
stop handoff. Failed shutdown releases the gate without persisting stop intent.

The workspace has behavioral tests for permission attenuation, empty startup
catalogs, fragmented/malformed frames, vault encryption/locking/wrong passwords,
worker fuel/import restrictions, credential policy denials, TOTP enrollment/replay/drift, authentication lockout, reveal expiry, durable deduplication/replay, scoped file access,
settings conflicts, model revocation and runtime API discovery/chat/persistence.
Provider tests also check advertised effort enforcement/dispatch, attached text dispatch/persistence and size rejection, and API-sourced balances/prices.
Preset tests cover TOML persistence, overrides, hiding/default opt-out, account
independence and rejected credential-bearing endpoints. Compatible metadata tests
cover API-reported context windows and nested effort choices in API order.
Native protocol tests cover paginated Anthropic/Gemini discovery and request/response
mapping. Backend tests run independent mock agent executables inside the actual
macOS sandbox, verifying restricted network/file access, scoped read/write, symlink
escape and private-metadata denial, foreign-session isolation, malformed/oversized output, cancellation
of the root and descendant processes, and active configuration revocation. These
are protocol/boundary tests, not evidence of real vendor-runtime compatibility.
All model IDs and context limits used by provider tests are generated by a running
local API; there are no copied model/provider fixtures.

Router tests render the shared application with in-memory history, exercise
Back/Forward and initial page selection, and check exact thread identity, split
selection, missing-thread recovery and malformed routes. Web service tests cover
HTML deep-link reloads, HEAD, existing assets and reserved/non-HTML/method errors,
the localhost-to-127.0.0.1 redirect and one-use authorization links. A headless
Chromium check confirmed a second tab, a reload, the localhost URL and a restarted
bridge (stale bearer discarded, new link accepted) all reach a connected gateway.
Browser checks verify router-aware links, app Back/browser Forward, new-conversation
navigation, direct split-view reloads with the selected second thread, and
unavailable/malformed conversation recovery. Desktop and web release builds and
packaged Rust smoke tests pass; terminal navigation interaction remains unverified.

`crates/app/tests/smoke.rs` exercises the actual executable, shared gateway process,
WASM worker, ACP initialize/new-session and scoped MCP tool discovery/calls, denied title editing by default and explicit
per-session authorization. Its Joshua test verifies local owned-process
start/discovery/stop/restart and rejection of repointed connections against an
independent Rust mock API; this is not real Joshua inference acceptance. The
tests run through Cargo with bounded subprocess/protocol I/O and isolated profiles,
and support packaged binaries through `A206_TEST_BINARY`. Windows smoke coverage
awaits gateway named-pipe support. The
macOS native bundle and release WASM web assets have been built. Browser UI checks
cover conversation creation, full-width layout, theme persistence, explicit light/dark schemes, compact split-pane composers, independent mode controls, attachment staging/removal/re-selection, keyboard entry, usage/account popovers, and onscreen bottom-edge selectors. TUI has been
compiled and launched against the pinned Dioxus 0.7 fork. It renders, but its narrow
terminal layout, graphics fallbacks and interaction still need manual QA.
Provider UI checks cover grouped choices, Moonshot endpoint setup, custom preset
creation/hiding/deletion, and restoring a bundled override with refreshed fields.

See `README.md` for reproducible commands. The GitHub Actions workflow checks the
workspace on macOS; check its run results separately from the local evidence above.
An SSH gateway was exercised end to end against a local OpenSSH server: setup
refused the unknown host until its fingerprint was given, then gateway start,
status, snapshot and stop ran over SSH; a changed host key and an unreachable
port were reported as errors. A terminal-client run of an unconfigured profile
showed the setup screen without creating the profile or launching a gateway.
Build success is not evidence of real remote SSH across networks, real Joshua model inference,
vault migration or production security certification.

## Next gates

1. Restore complete secret custody: encrypted broker IPC, explicit destination/use
   approvals, TOTP and lossless legacy vault migration. Enable authenticated model
   dispatch only after those boundary tests pass.
2. Implement the agent execution engine, streaming, permissioned tools, project
   scheduler and delegated task lifecycle, including cancellation and budgets.
3. Ship a useful extension SDK, compiled tool plugins and reviewed agent-generated
   extension packages with validated UI contributions.
4. Add verified local/SSH installers, remote supervision, browser/vault integration,
   document format plugins and forge-neutral PR/CI workflows.
5. Complete context/accounting controls and API integrations without introducing a
   static model catalog, fabricated prices or default account balances.
