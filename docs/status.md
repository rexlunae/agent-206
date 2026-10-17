# Implementation ledger

Agent 206 is a working first foundation. It is not yet a complete successor to
RustyClaw. This ledger prevents the UI, docs or future contributors from mistaking
implemented structure for finished agent execution or security parity.

## Current implementation

| Area | Available now | Remaining work |
| --- | --- | --- |
| Rust architecture | Fourteen small crates, typed shared commands, forbidden application unsafe, pinned dependencies | Further extraction as capabilities grow; measured build/runtime budgets |
| Desktop | Native macOS app bundle, shared fc-dioxus interface, automatic local gateway startup, shared lifecycle controls | Windows named pipes; Linux desktop packaging and hardware validation |
| Gateway lifecycle | Shared Gateway page and controller, status from the actual process, start/stop/restart/reconnect, durable deliberate-stop marker, cleanup before acknowledgement, bounded control receipts; web uses an authenticated user-only binary endpoint | Remote SSH acceptance, stronger cross-process lifecycle coordination, encrypted enrollment and authentication; running is not security readiness |
| Initial setup/import | `agent206 setup` resumes first-run setup on the existing flows (gateway target, vault create/unlock, optional TOTP enrollment, pairing) and ends with the health report; `agent206 import rustyclaw` previews by default and with `--apply` adds missing provider accounts and disabled messenger entries through the revision-checked settings API, copies a password vault in through the broker (`ImportVault`: encrypted copy, password and enrolled TOTP verified on the copy, never merged or replacing a vault, source untouched) and writes an owner-only receipt; MCP servers, tool permissions, skills and plaintext tokens (messenger tokens included) are counted or reported and left inactive; tested in the import crate, the broker and through the real binary against a real gateway | Desktop/TUI setup wizard and import screen, key-file vaults, conversations/workspace instructions/memory, rollback command, adapters for Cline, Claude Code, Codex, DeepSeek Harness and others, isolated reader process; interactive vault import is exercised only in broker tests (it needs a terminal) |
| Messengers | Telegram through the Bot API and Matrix through a homeserver's client API (no end-to-end encryption; never sends to an encrypted room), run by the gateway while it is paired with an unlocked vault; the broker spends the bot or access token for the bound origin only; entries start disabled and allow no chat, and groups need listed senders; refused chats and senders, account and structured failures are in the status; allowed chats get one thread each and ordinary `Send` turns; `messenger` CLI, TOML settings, a shared desktop/web/TUI Messengers page and RustyClaw import; tested against a stand-in Bot API and a stand-in homeserver with a real broker and through the gateway's settings API (docs/messengers.md) | Messengers page render-tested only (not clicked through in a running app), other services, attachments, persisted poll offset (Telegram delivery is at least once across restarts; Matrix answers nothing sent while the gateway was stopped), Matrix encryption, not tested against the real Telegram or Matrix services |
| CLI | Headless build, system status, nested gateway lifecycle/status, setup, doctor, import (RustyClaw), messengers, chat, projects, settings, catalog refresh, downloads, text documents, Git status, vault, ACP/MCP, renderer entry points and ui alias | Full RustyClaw-compatible command families in docs/cli.md; doctor repairs, more import adapters and remaining parity |
| Gateway setup | Clients connect only after first-time setup (the desktop, TUI and web setup screen or `gateway setup`) saves a local or SSH target in `connection.toml`; unknown SSH hosts require fingerprint confirmation into the profile's own known_hosts; changed keys are refused; typed `ConnectionError`s are traced and shown; the web bridge's setup endpoint is tested for bearer checks, unsafe endpoints and damaged saved setups, and web SSH setup was checked in headless Chromium against a local OpenSSH server | Gateway enrollment (vault/TOTP) is a separate gate; the web bridge's sign-in for other devices is single-user (the vault owner's authenticator), with in-memory sessions |
| Web access from other devices | Opt-in `agent206 web --listen ADDR --name HOST…`: HTTPS only (rustls), a generated self-signed certificate per name set kept in the profile with its fingerprint printed, or the user's PEM files; requests for unlisted hosts or other origins are refused; browsers sign in with a code from the vault's enrolled authenticator, checked by the broker (throttle, one-use codes, unlocked vault only); 12-hour in-memory sessions, at most 32, ended by sign-out or restart. Tested: host/origin/session checks, sign-in against a real broker with an enrolled authenticator (wrong, replayed and locked cases), certificate reuse and renewal; checked in headless Chromium over HTTPS (sign-in, gateway setup, reload, sign-out) | Certificate rotation without restart, ACME, per-device session list, multiple users |
| SSH | Ordinary OpenSSH, strict host verification, configured identities, keepalives, remote stdio bridge |  remote installers/updaters, remote acceptance test |
| Wire protocol | Versioned bounded MessagePack, fragmented/coalesced reads, strict decode, durable request receipts | Negotiated feature sets, pushed subscriptions, full reconnect/resume UX |
| Health checks | `agent206 doctor` (text or `--toml`) and the Gateway page's Health checks card share typed findings from `a206_core::doctor`, gathered by `a206_runtime::doctor`; the web client runs the same checks on its bridge (`/api/health`, bearer-gated, for the gateway the bridge uses): profile privacy, gateway setup and process, vault broker/lock/TOTP, gateway vault pairing (reported by the gateway, checked only while unlocked), each provider's credential against the broker's destination rule and its API catalog age; nothing is started or changed; unknown stays unknown; evaluation unit-tested, the CLI tested through the real binary, the bridge route tested in the runtime crate and the web card clicked through in Chromium | `--repair`, remote-host broker checks (run doctor on that host) |
| Errors/diagnostics | Per-module `thiserror` enums with source chains and explicit kinds up to each handling point (gateway validation, conflicts and missing resources are classified, tested for gateway replies); structured `ErrorReport` (kind, message, causes) in replies, lifecycle reports, downloads and failed runs, compatible with message-only payloads; `tracing` events in gateway, controller and clients; desktop/TUI error notices with expandable causes; CLI prints the full chain; URL redaction in transport errors; `let _ =` discards denied by clippy | Typed errors in the app binary below its top level, localized messages, error telemetry/export, retry guidance per kind |
| Persistence | One gateway writer, SQLite WAL snapshots/journal, revision checks, restart recovery, explicit replay gaps | Paginated/archived history, migration tooling; current snapshot is bounded below 4 MiB |
| Chat | API-discovered model selection, native/compatible text generation, cancellation, persisted text attachments (8 files/128 KiB), API-advertised reasoning effort; streamed text from external backends and from native OpenAI-compatible, Anthropic and Gemini SSE replies (direct and through the vault broker's bounded stream frames), published to the event journal while the reply is open; malformed, truncated and in-stream provider errors are typed and leave the run Failed with partial text (runtime and broker tests against local mock APIs and a real broker; not tested against real vendor APIs) | Image/PDF/audio attachments, streamed reasoning display, retries with explicit external-effect semantics, an idle (rather than total 180 s) stream timeout |
| Providers/models | Configurable hosted, local and custom preset library with GUI editing and TOML overrides; compatible, Anthropic and Gemini text APIs; bounded catalog pagination, provenance/expiry and refresh; unknown facts remain unknown | Isolated authenticated provider broker, accounting integrations and real vendor acceptance; model facts stay API-only |
| Coding-agent backends | Optional Codex app-server and Claude Agent ACP client adapters; per-thread selection/grants, macOS (Seatbelt) and Linux (bubblewrap namespaces + Landlock, loopback-only proxy relay) OS isolation, restricted model proxy, streamed text, operation summaries, bounded I/O, cancellation/process-group teardown; exercised with independent mock peer executables | Authenticated broker integration, real vendor-runtime acceptance, Windows isolation, installation/updating, session resume, interactive approvals and external subagent policy; runtimes are not bundled |
| Native tool loop | Gateway-owned run engine (`runtime/src/gateway/run.rs`, see docs/run-engine.md): streamed tool calls parsed for OpenAI-compatible, Anthropic and Gemini (incremental argument deltas, Gemini thought signatures returned); meaning-preserving argument repair and in-response de-duplication, truncated arguments refused; per-tool Allow/Ask/Deny (unset = deny) combined with the host's policy, every ancestor's setting and the grants of the conversation and every ancestor, re-checked for each call, with a running call or pending approval stopped when that authority is withdrawn; Ask publishes a journaled approval request answered only by `ResolveApproval` with the call's argument digest, single-use and expired on cancel or restart; per-conversation round/tool-call budgets, each bounded by every ancestor's; recorded arguments and outputs bounded by the remaining conversation storage; last-request usage for context kept apart from summed run usage for cost; cancellation during streaming, a tool call or an approval wait; tool failures and refusals reach the model as result text and clients as `ErrorReport`s; desktop/TUI tool traces, approval cards and per-tool permission selects; CLI `tools` and `approvals`; a native `ask_user` tool whose questions (suggested answers, optional free text) wait for `AnswerPrompt` from the conversation's question card or `agent206 prompts`, bound to the asking conversation, expired on cancel, revoke or restart; Native execute_command offered on Linux with sandbox isolation and approval; WASM extension tools offered by default as `ext__<extension>__<tool>` once the user enables and grants the extension, still denied per conversation until permitted (runtime tests against local mock APIs with a test-only tool host, and with the bundled workspace extension through the real worker: permit/approve/run, argument errors, capability ceiling, disabled extension, cancellation killing the worker, which is confined on Linux; UI render tests of traces) | The MCP adapter is pending; the bundled extension's tools read, list, search and (with a separate write grant) write and edit workspace files; approval card interaction and the permission panel are not render-tested; the question card is render-tested but not clicked through in a running app; budget editing is CLI-only; Anthropic thinking blocks are not replayed with tool results; no real vendor acceptance |
| Usage/context | Compact composer widgets for last-request context used/limit, reported response cost, API prices and reported account/token balances; detail popover; usage clears when model selection changes | Compaction/context controls, cumulative accounting, richer history graphs and provider currency semantics |
| Modes | Chat, Plan, Code, Review, Research; editable system prompts; mode ceilings | Mode-specific run engines, tools and approval UX |
| Projects/subagents | Project coordinator conversations, parent-linked delegated threads, explicit model allowlists, narrowed grants, revocation checks | Autonomous project management, task assignment, scheduler/budgets, steering and hierarchical cancellation |
| Concurrent UI | Resizable navigation/chat/inspector, two visible conversations, independently scoped controls, conversation title editing, searchable complete conversation list, compact integrated composer controls, themed raised/feature surfaces | More than two visible threads, saved layouts and comprehensive terminal interaction QA |
| Navigation | Typed Dioxus Router routes, persistent shell, fc-dioxus links, Back/Forward controls, conversation and split-view URLs, deep-link reloads, loading/missing recovery | Cross-profile deep-link handling, draft/form preservation across route unmounts, terminal interaction acceptance |
| Vault | SecureStore-compatible encryption, locked state, redacted nonserializable secret type, private atomic writes, interactive standalone CLI; library-level typed credentials in RustyClaw's layout, all five access policies plus disabled/agent switch, verified TOTP enrollment with replay protection, auth throttle and reveal step-up window; the CLI challenges enrolled vaults ; a separate broker process owns the vault, serves the CLI and performs destination-bound provider requests for a gateway paired per unlock; the desktop/TUI Security page creates, unlocks (with the authenticator challenge), locks and pairs a local profile's vault and enrolls TOTP through the same broker requests (`a206_runtime::vault`, shared with the CLI; runtime tests against a real broker and gateway, UI tests of offered actions and the remote notice only); the web client's Security page shows the bridge's local vault state and locks it through `/api/vault`, never handling the password (bridge-tested against a real broker and clicked through in Chromium) | Unlock, creation, pairing and enrollment from the web client (by design for now), vault controls for a remote (SSH) gateway, desktop/TUI credential and policy management and TOTP removal (CLI only), QR display of the enrollment key, agent approval UI, recovery codes, key-file migration, remote broker over SSH, credentialed external backends and browser injection |
| Security model | Deny-by-default grants, no secret-bearing agent tool, restricted MCP, bounded worker, SSRF-resistant public downloads, capability-scoped text reads | Full RustyClaw defense parity, Seatbelt profiles for macOS native exec and the macOS WASM worker, leak/injection boundary scanning, adversarial review |
| MCP client | Experimental: user-configured stdio servers, disabled by default, run under macOS sandbox-exec or Linux bubblewrap + Landlock with only declared authority (project read/write, commands, network) and a cleared environment; per-tool Allow/Ask/Deny; calls authorized against the thread's current grant and project; bounded lines, listings, results and time; status, tools and typed failures in the snapshot; Extensions page and `mcp-servers` CLI; gateway API for the run engine (`docs/mcp.md`). Tested against a Rust fixture server as a child process, through the real macOS and Linux sandboxes on CI (files, symlinks, network and program grants), and gateway policy denials | Model access through the run engine with approvals, streamable HTTP, vault-brokered server credentials, Windows isolation, resources/prompts, network sandbox acceptance |
| Skills | Experimental: `SKILL.md` folders in the profile (AgentSkills/OpenClaw frontmatter), listed with parse failures and unmet requirements, off until the user approves the exact content (SHA-256 pinned; edits withdraw approval), reviewable text, bounded supporting files without traversal or symlinks; CLI and Extensions page; gateway API for the run engine (`docs/skills.md`). Gateway tests cover approval, edits, failures and path escapes | Model access through the run engine, ClawHub install with audit, agent-prepared skills, SkillOnly credentials, project skills |
| WASM | ABI v2: Rust guest SDK; bundled `agent206.workspace` compiled to WASM by `build.rs`; separate worker child per call relaying MessagePack host calls, confined on Linux by bubblewrap + Landlock with no network, no project files and nothing executable but itself (tested by an in-sandbox probe; a missing layer fails the call); only the two ABI imports, no WASI; fuel/memory/input/output/host-call/time bounds; workspace read/list/search and write/edit/create-directory (cap-std scoped, atomic writes, separate write grant), git status/log/diff (gitoxide, no repository-configured programs; bundled `agent206.git`) and per-extension state host calls checked against user grant ∩ caller ceiling ∩ declaration; tools callable from native runs within the conversation's grant (tested against real WASM and adversarial modules; end-to-end smoke test through the worker; native-run tests through the worker) | Git write, network and exec host calls, OS sandbox profile for the macOS worker, user/agent package creation and reviewed installation |
| Plugin UI/API | ABI v2 documented (`docs/plugin-api.md`); manifests declare typed tools and panels; validated panel node trees rendered with fc-dioxus on the Extensions page (desktop/TUI, and the web client through its bridge: a bundled panel opened in Chromium after granting its permission) and as text in the CLI; button events; per-capability grant and enable switches in app chrome | Viewer/command contributions, subscriptions, richer components |
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
web client talks to the authenticated loopback web bridge, which is itself the
gateway client: it reaches a local or SSH gateway server-side, and its setup
screen runs SSH checks and host key review in the bridge, so keys never reach
the browser. There is no SSH-over-WebSocket transport. With `--listen` the
bridge serves other devices over HTTPS only, for listed host names, and browsers
sign in with the vault's enrolled authenticator; it is off by default and is a
single-user sign-in, not multi-user accounts.

## Explicitly disabled

Credential-backed providers work only through the vault broker: the vault must be
unlocked, the gateway paired for that unlock, and the credential bound to the
provider's exact origin, or the request fails with an explanation. The gateway never reads a credential value, and agents
still have no credential use. External coding backends still refuse credentialed
providers. Browser
preview has no credential injection. Third-party extension installation is unavailable.
Bundled extensions reach the workspace and their own state only through
gateway-authorized host calls; no extension links WASI or any other
capability-bearing WASM import. New coding conversations have no
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
macOS sandbox, and on Linux inside bubblewrap with Landlock, verifying restricted network/file access, scoped read/write, symlink
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
2. Connect MCP tools to the native run engine (WASM extension tools are connected), then add the project
   scheduler and delegated task lifecycle, including hierarchical cancellation.
3. Grow the extension SDK's host domains and builtins, and add reviewed
   agent-generated extension packages.
4. Add verified local/SSH installers, remote supervision, browser/vault integration,
   document format plugins and forge-neutral PR/CI workflows.
5. Complete context/accounting controls and API integrations without introducing a
   static model catalog, fabricated prices or default account balances.
