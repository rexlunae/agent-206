# RustyClaw → Agent 206 feature parity gaps

This is the working gap list for reaching RustyClaw parity. Update a row when its
status changes; `docs/status.md` remains the authoritative implementation ledger.

Scope decisions (2026-10-16): gateway communication keeps RustyClaw's SSH-based
protocol, and parity work targets desktop and TUI first. The web client reaches
gateways, local or SSH, through its server-side bridge (2026-10-17), not through
SSH in the browser. Its pages are the desktop app's shared fc-dioxus components
calling the gateway's command API through the bridge; two cards that the desktop
runs against local processes use their own bridge routes instead: the Gateway
page's health checks (`/api/health`) and the Security page's vault card
(`/api/vault`). Chat streaming and tool approvals use the same snapshot polling
as the desktop but have not been exercised in a browser against a live model
provider. By design the browser only reads and locks
the vault: creating, unlocking, pairing and enrolling it need the desktop or
terminal app, so the vault password never passes through a browser.

Reference: [RustyClaw `481a428c`](https://github.com/rexlunae/RustyClaw/tree/481a428cb7a3b82d1a5e556ffef40e3dd9f52ce3) (`rc/` below).
Agent 206 paths are prefixed `a206/`; the survey was taken at `1666303`. Parity = equivalent user-facing
capability under Agent 206's architecture (gateway-owned policy, separate vault/extension processes,
WASM tools, MessagePack APIs, API-only model facts, fc-dioxus shared UI). Status words:
**present** (works end to end in a206), **partial** (structure or subset exists), **missing**.

Key a206 facts that drive most rows: the native engine streams text and runs a gateway-owned
tool-call loop with user approvals (`a206/crates/runtime/src/gateway/run.rs`) whose only default
tool source is the WASM extension adapter (`a206/crates/runtime/src/extension_tools.rs`); MCP tools
are not connected to it yet; credentialed providers
now go through the vault broker (survey time: refused); the agent-facing surface is the MCP harness with
one tool, `rename_thread` (`a206/crates/runtime/src/mcp.rs`); the `Command` enum
(`a206/crates/core/src/lib.rs:418`) is the entire gateway API; WASM ABI v2 (survey time: v1, no inputs or host calls) takes MessagePack
invocations and relays gateway-authorized host calls (`a206/docs/plugin-api.md`).

## Security, vault and authentication

| RustyClaw capability (source) | Agent 206 status | How it should land |
| --- | --- | --- |
| SecureStore vault, password or key file (`rc/crates/rustyclaw-core/src/secrets/vault.rs`) | partial: separate broker process (`a206/crates/broker`) with CLI surface (`a206/crates/app/src/vault_cli.rs`); key file and modern envelope pending | Separate vault-broker process with encrypted UI channel; key-file reader; modern envelope for new vaults (`a206/docs/security-setup.md`) |
| Typed credentials: API key, login, SSH key, passkey, form, payment, note (`secrets/types.rs`) | partial: typed metadata/values in RustyClaw's key layout (`a206/crates/vault/src/credential.rs`); served by the broker | Typed metadata in broker; unknown kinds stay locked |
| Per-credential policy Always/WithApproval/WithAuth/SkillOnly/TriggerOnly, disabled flag (`secrets/types.rs`) | partial: all variants evaluated deny-by-default in the vault library (`a206/crates/vault/src/policy.rs`); approvals not yet wired to a UI | Broker-enforced; maps onto `Capability::UseSecret` + destination-bound handles |
| TOTP 2FA, ±1 step, enrolled-state authoritative (`secrets/vault_ext.rs`) | partial: verified enrollment, replay protection, drift diagnosis in the library (`a206/crates/vault/src/totp.rs`); CLI enrollment and unlock through the broker; GUI enrollment pending | Broker generates seed; enrollment verified in trusted UI; enforced on connect |
| Auth rate limit/lockout 3 fails/30 s (`rc/crates/rustyclaw-gateway/src/auth.rs`) | present: one broker-wide throttle shared across connections (`a206/crates/broker/src/server.rs`) | Broker-wide limits that survive reconnect |
| Step-up reveal window (`rustyclaw-gateway/src/secrets_handler.rs`) | present: per-connection 120 s window, voided by any lock | Separate reveal capability, expiry on lock/reconnect |
| Agent secret tools `secrets_list/get/store/set_policy/link_trigger` (`tools/secrets_tools.rs`) | missing by design (no secret-bearing agent tool, `a206/docs/status.md`) | Replace with opaque handles + brokered operations; agents may request, never read/approve |
| PromptGuard / InputValidator / SafetyLayer (`security/prompt_guard.rs`, `validator.rs`, `safety_layer.rs`) | missing | Gateway-side scanning stage on inbound content and tool results |
| LeakDetector on outputs/requests (`security/leak_detector.rs`) | missing | Boundary scanner at gateway egress + extension output |
| SSRF validator (`security/ssrf.rs`) | partial: public-only, pinned-address fetch for downloads and the extension `fetch` host call, with per-manifest destination lists (`a206/crates/runtime/src/network.rs`) | Reuse `network.rs` for every other network-capable host call |
| Credential-guard override tool (`tools/guard_override.rs`) | missing (intentionally) | Only a user-approved, scoped exception in the approval surface |
| Skill security audit (`skill_audit.rs`) | missing | Run on extension/skill packages pre-install |
| SSH client pairing, Ed25519 keys, QR (`pairing/*`, `rustyclaw-gateway/src/ssh/server.rs`) | partial: OpenSSH stdio with strict host checks (`a206/crates/runtime/src/transport.rs`); no in-app pairing | Keep RustyClaw's SSH-based gateway protocol (decided 2026-10-16); in-app host-key pairing |

## Agent loop, tool calling and streaming

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Multi-round tool loop, rate-paced rounds (`rustyclaw-gateway/src/dispatch.rs`, `tool_executor.rs`, `tool_limits.rs`) | partial: gateway run engine for native providers (`a206/crates/runtime/src/gateway/run.rs`): model → tool calls → policy (per-tool permission ∩ host policy ∩ ancestors ∩ every ancestor's current `Grant` within its mode ceiling, re-checked per call and for running calls and approval waits when a permission or grant changes) → approval if Ask → `ToolHost` execution → results fed back, until the model stops; per-thread round/tool-call budgets (`ToolBudget`, bounded by every ancestor's), cancellation during streaming, tool calls and approval waits; tools come from native execute_command (Linux and macOS, sandboxed and approval required) and enabled, user-granted WASM extensions (`ext__<extension>__<tool>`, registered by default; `a206/crates/runtime/src/extension_tools.rs`), the bundled set being `agent206.workspace` read/list/search and write/edit; the MCP adapter is pending, no rate pacing; external Codex/Claude Agent backends run their own loops | Connect the MCP client as a `ToolHost` (`a206/docs/run-engine.md`); more builtin extensions; rate pacing if a provider needs it |
| Tool approval Allow/Ask/Deny per tool (`tools/mod.rs:249` ToolPermission, desktop `components/tool_approval.rs`) | partial: `ToolPermission` per thread (unset = deny), set by `SetToolPermission` from the desktop/TUI inspector or `agent206 tools permit`; Ask publishes a journaled `ApprovalRequested` event and pending request, answered only by `ResolveApproval` bound to the call's argument digest (single-use, expires on cancel/restart; the model never sees approval IDs); fc-dioxus `Approval` card with Deny/Allow once buttons and `agent206 approvals`; delegated threads cannot exceed their parent | "Always allow" shortcuts and per-argument scopes, approval-card interaction tests, mobile/web parity |
| `ask_user` structured prompts (`user_prompt_types.rs`, desktop `user_prompt.rs`) | partial: native `ask_user` tool (question, up to 8 choices, optional free text), journaled `PromptRequested`/`PromptResolved`, `AnswerPrompt` bound to the asking conversation and checked against the question, expiry on cancel/revoke/restart; question card in the shared conversation view and `agent206 prompts` (`a206/crates/runtime/src/prompt_tool.rs`, `a206/crates/ui/src/prompts.rs`, `a206/docs/run-engine.md`); gateway-tested in all three protocols, card render-tested; not exercised against a live model; external backends' own prompts are not mapped | Map ACP/Codex permission-less questions onto the same prompt; messenger answers |
| Tool-call healing: malformed JSON, dupes, leaked markup (`tool_healing.rs`) | partial: meaning-preserving repair of fenced/prose-wrapped arguments and trailing commas, in-response duplicate calls run once, missing/repeated call IDs replaced (`a206/crates/runtime/src/tool_calls.rs`); truncated arguments are refused back to the model rather than completed; leaked-markup stripping, schema type coercion and repeat-call warnings are not implemented | Leaked tool markup filter on streamed text; schema coercion once tools carry typed parameters |
| Tool-output compression rules (`rc/crates/tokenjuice`, `tool_pipeline.rs`) | missing | Reusable crate (MIT attribution); TOML rule overlays |
| Provider SSE streaming OpenAI/Anthropic (`streaming.rs`, `gateway/providers/mod.rs`) | present for text: native OpenAI-compatible, Anthropic and Gemini replies stream; the broker relays credentialed bodies in bounded, ordered frames (protocol v4, negotiated down to v3 brokers, which answer buffered); bounded incremental SSE decoding (`a206/crates/runtime/src/sse.rs`); deltas published as journaled `ThreadChanged` events at most every 100 ms; lock, pairing revocation, cancellation and client disconnect stop the upstream request; mid-stream failures keep partial text in a Failed run with a structured report. Streamed tool calls (with incremental argument deltas) feed the run engine; reasoning/thinking display and retries are not implemented | Per-delta event payloads if snapshot republishing becomes too costly |
| Context compaction at 75% window + pre-compaction memory flush (`gateway/helpers.rs`, `memory_flush.rs`) | missing (context used/limit widget only) | Use API-reported context limit; unknown limit ⇒ no auto-compaction, user control |
| Retry policy + model/auth-profile failover (`retry/policy.rs`, `models/failover.rs`) | missing | Retries with explicit external-effect semantics; **no silent model fallback** (thread allowlist) |
| System prompt + workspace files SOUL/AGENTS/TOOLS/MEMORY (`soul.rs`, `workspace_context.rs`, `gateway/system_prompt.rs`) | partial: editable per-mode prompts (`ui/src/settings.rs` PromptEditor) | Project-scoped instruction files read via ReadWorkspace grant |
| Receipt/freshness protocols (`protocols/*`) | missing | Optional; fold into task lifecycle receipts |
| Todo planning tool (`tools/todo_tool.rs`) | missing | Builtin WASM tool with thread-scoped state |

## Tools (all should become WASM builtins with manifest-declared capabilities)

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| read/write/edit/list/search/find files (`tools/file.rs`) | partial: read-only text `ReadDocument` scoped by cap-std; the bundled `agent206.workspace` WASM extension (`a206/plugins/workspace`) has `list_directory`, `read_text_file`, `search_text` (literal, bounded) under ReadWorkspace and `write_text_file`/`edit_text_file` (atomic, single-occurrence edit) under WriteWorkspace; glob/find and regex search missing; agents cannot call them until the run engine's tool loop lands | Remaining: glob/find, regex search, multi-hunk patch as workspace host calls |
| `apply_patch` multi-hunk (`tools/patch.rs`), `ast_grep_manage` (`ast_grep.rs`) | missing | Workspace patch host call; ast-grep as extension |
| `execute_command`, `process` bg/poll/send_keys/kill (`tools/runtime.rs`, `process_manager.rs`) | partial: native `execute_command` `ToolHost` (`a206/crates/runtime/src/exec_tool.rs`), registered by default: `sh -c` in bubblewrap + Landlock on Linux or Seatbelt on macOS (one shared `a206-sandbox` profile) with only the conversation's grants (project read/write, host network), gateway state and credential stores hidden, empty environment, Ask on every call, Execute required, 1–600 s timeout, bounded stdout/stderr, process-group teardown on cancel and at shell exit; not offered on Windows (reported unavailable); no background processes; on macOS a process that calls `setsid` can outlive the call, still under the same policy | `process` background sessions with owner-bound poll/send/kill; user-chosen extra program directories (Homebrew, toolchains) |
| `web_fetch`, `web_extract` (`tools/web.rs`, `web_extract.rs`) | partial: bundled `agent206.web` `fetch_url` (raw text, status and type) through the `fetch` host call and SSRF guard (`a206/plugins/web`), callable from native runs once the user grants `network` | Readable-text extraction in WASM |
| `web_search` Brave/Exa (`tools/web.rs`, `exa.rs`, `search_provider.rs`) | missing | Search connector presets + brokered key |
| `http_request` with scanning (`tools/http.rs`) | missing | Destination-bound brokered HTTP |
| `browser` CDP automation (`tools/browser/real.rs`) | partial: sandboxed preview, no automation (`ui/src/pages.rs` Browser) | Isolated profile/driver process; exact-origin vault injection |
| `pdf`, `document` DOCX/XLSX/PPTX (`tools/pdf.rs`, `document.rs`) | partial: UTF-8 text/Markdown only | Format-viewer WASM extensions |
| `image` vision, `image_generate`, `tts` (`tools/image_gen.rs`, `gateway_tools`) | missing; attachments text-only (8 × 128 KiB) | Image/audio attachments; capability from API metadata only |
| `chart` SVG (`tools/chart.rs`) | missing | Typed UI contribution rendered by fc-dioxus (no raw SVG/HTML injection) |
| `canvas` / A2UI (`canvas/*`, `tools/devices/canvas.rs`) | missing | Plugin typed UI descriptions (`docs/plugin-api.md` target) |
| `nodes` SSH/ADB/VNC devices (`tools/devices/nodes.rs`) | missing | Extension over ordinary SSH; host verification |
| sysadmin/system tools: firewall, pkg, service, user, net scan, disk, monitor, clipboard, screenshot (`tools/sysadmin/*`, `tools/system_tools/*`, `kernel_tools.rs`) | missing | Opt-in extensions; high-risk ops behind approval |
| package managers `npm_manage`, `uv_manage` (`tools/npm.rs`, `uv.rs`) | missing | Extensions using Execute grant |
| `freenet`/`river`/`atlas` (`tools/freenet/*`) | missing | Optional extension |
| Git ops | partial: read-only `GitStatus` (`gateway.rs`); the bundled `agent206.git` WASM extension has `git_status`, `git_log` and per-file `git_diff` through gitoxide under GitRead, with no repository-configured programs; agents cannot call them until the run engine's tool loop lands | Stage/commit under GitWrite; forge-neutral PR/CI host domain |

## Memory

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| `memory_search` BM25 / `memory_get` over MEMORY.md + memory/ (`memory.rs`, `tools/memory_tools.rs`) | missing | Memory extension with scoped store |
| Two-layer MEMORY/HISTORY consolidation (`memory_consolidation.rs`) | missing | Background task with user-visible budget |
| Semantic vector memory, knowledge graph (`steel_memory.rs`, feature `semantic-memory`) | missing | Optional extension/service process |
| Hierarchical ingest summary tree (`rc/crates/memory-tree`) | missing | Reusable crate behind memory extension |
| `session_search`/`search_history` (`tools/session_search.rs`) | partial (owner: web client thread): gateway `SearchMessages` over every conversation's message text (all words, case-insensitive, newest first, bounded query and hits, never journaled) with a snippet around the first match (`a206/crates/core/src/search.rs`); a Search messages card on the shared Conversations page and `agent206 search` (`a206/crates/ui/src/search.rs`); tested in core, the gateway and a card render; no agent-facing tool yet, attachments and tool output are not searched | Agent `session_search` tool scoped to conversations the agent may already read (its own lineage), decided before it is offered; jump to the matching message |
| Memory vault / protected memory (`memory_vault.rs`) | missing | Through vault broker |

## Sessions, threads, subagents, swarm, tasks

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Threads, projects (`threads/*`, `projects/*`) | present: threads, projects, parent-linked subagents (`core/src/lib.rs` Thread/Project) | — |
| `sessions_spawn/send/history/kill`, steer (`tools/sessions_tools.rs`) | partial: native runs can `subagent_spawn/send/list/status/wait/cancel` (`runtime/src/delegation_tool.rs`) through `CreateSubagent`, so the model allowlist and grant intersection apply; children inherit the caller's tool permissions minus delegation, depth and concurrency are bounded, and `CancelRun` cascades to descendants | Steering a running subagent, an agent-requested `SpawnAgent` grant flow, per-subagent budgets, desktop tree view |
| `subagent_create/run/list/delete` (`tools/subagent_tools.rs`, `gateway/subagent_runner.rs`) | partial: subagents run on the same native run engine as root threads (see above); no named, reusable subagent profiles | Profiles as TOML presets of mode, model and capabilities |
| Named agent registry, `agents_create` (`agents.rs`) | missing | Profiles as TOML agent definitions |
| Swarm orchestration + templates (`swarm/*`, `tools/swarm_tools.rs`, CLI `swarm`) | missing | Project coordinator thread + delegated task lifecycle |
| Task fg/bg/pause/resume/input (`tasks/*`, `tools/task_tools.rs`) | missing | Task state machine with durable events |
| `session_status` usage/cost (`tools/sessions_tools.rs`) | partial: per-request usage/cost/balance widgets from API (`docs/status.md` Usage) | Cumulative accounting, API-sourced only |
| Concurrent model execution (`gateway/concurrent.rs`) | partial: two visible conversations; runs per thread | >2 views, saved layouts |

## Scheduling and automation

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Cron at/every/cron + agent-turn payloads (`cron.rs`, `tools/cron_tool.rs`, `gateway/cron_runtime.rs`) | missing | Gateway scheduler; jobs run with stored, user-set grants |
| External triggers as child programs (`triggers.rs`, `gateway/trigger_manager.rs`, `tools/trigger_tools.rs`) | missing | WASM/sandboxed trigger extensions; TriggerOnly credentials |
| Heartbeat / subconscious loop (`subconscious.rs`, `workspace_context.rs`) | missing | Scheduled self-check task with budget |
| Auto-fetch sync sources (`auto_fetch.rs`) | missing | Downloads lifecycle + scheduler |

## Messengers / channels

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Telegram (`messengers/telegram.rs`) | partial: Bot API long polling and replies in the gateway (`runtime/src/messengers`), token spent only by the vault broker (`AuthScheme::TelegramBot`, broker v6), disabled by default, per-entry chat and sender allow-lists (groups need listed senders) with refused chats and senders in the status, one thread per chat through ordinary `Send` turns, CLI, TOML and a shared desktop/web/TUI Messengers page (`ui/src/messengers.rs`: add, enable, model and mode, allow-list editing, allowing a refused chat or sender, connect/disconnect, structured failures), RustyClaw import as disabled entries (docs/messengers.md); tested against a stand-in Bot API and a real broker | Attachments, persisted poll offset; the Messengers page is render-tested but not yet clicked through in a running app |
| Matrix (`messengers/matrix.rs`) | partial: client-server API `/sync` long polling on the user's homeserver (`runtime/src/messengers/matrix.rs`), access token spent only by the vault broker as a bearer token for the homeserver origin (`PUT` from broker v7), every room treated as a group (listed senders only), joins only invitations to allowed rooms, never answers history, never sends to an end-to-end encrypted room, CLI, TOML, the shared Messengers page and RustyClaw import (homeserver kept, access token and password left behind); tested against a stand-in homeserver and a real broker | End-to-end encryption, `.well-known` discovery, password sign-in, persisted sync token; not tested against a real homeserver |
| Discord, Slack, IRC, Signal/signal-cli, WhatsApp, Google Chat, Teams, iMessage, webhook, console (`messengers/kind.rs`, `gateway/messenger_handler/*`) | missing | Adapters behind the same `Host` interface; Discord and Slack Socket Mode need brokered WebSockets; inbound messages untrusted; per-channel grants |
| `message` send/broadcast tool (`tools/gateway_tools`) | missing | Channel host call requiring approval |
| Beeper/claw-me-maybe onboarding (`rustyclaw-onboard/src/messaging.rs`) | missing | Setup wizard step after extension system |

## Skills, plugins, MCP

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Skills: markdown/YAML, dependency gating, enable/disable (`skills/mod.rs`, `tools/skills_tools.rs`) | partial (experimental): RustyClaw-compatible `SKILL.md` folders in the profile, parse errors reported, OS/program requirements shown, approval pinned to the file's SHA-256 so edits withdraw it, bounded supporting-file reads, CLI `skills` and Extensions page; native runs read approved skills through `skill_list`/`skill_info` (`a206/crates/skills`, `runtime/src/skill_tool.rs`, `docs/skills.md`); install/enable/link_secret/create stay user operations | SkillOnly credentials; project skill folders |
| ClawHub registry install/search (`skills/clawhub.rs`, CLI `clawhub`) | missing (install is copying a folder; approval still required) | Reviewed install with manifest validation + audit |
| Agent-created skills/curator (`tools/skill_curator.rs`, `skill_create`) | missing | Agent prepares package; user enables |
| Plugin UI panels + state (`plugins/mod.rs`, `tools/plugin.rs`) | partial: ABI v2 (`a206/crates/extension-abi`, `extension-sdk`, `extensions`, `runtime/src/extension_host.rs`): declared tools and panels, typed fc-dioxus panel nodes with button events, per-extension persisted state, user grants/enable on the Extensions page and CLI, gateway-authorized workspace, git and network host calls; user-installed packages (`extension install`/`uninstall` and the Extensions page), validated before storing, digest-pinned and verified before every run, arriving disabled with nothing granted (`a206/crates/runtime/src/extension_host/packages.rs`); agents cannot create extensions | Agent `plugin_*` tools via the run engine; agent-prepared packages with review; registry install with audit; more host domains (patch, git write, exec after the OS sandbox) |
| MCP **client** connect/list/tools (`mcp/*`, `tools/mcp_tools.rs`) | partial (experimental): stdio servers in the macOS or Linux sandbox with declared authority, per-tool Allow/Ask/Deny, grant- and project-bound calls, bounded framing/listing/results, status in snapshot, desktop/TUI Extensions page, `mcp-servers` CLI (`a206/crates/mcp`, `runtime/src/gateway/mcp_servers.rs`, `docs/mcp.md`); not reachable by models until the run engine lands; no HTTP transport, credentials or Windows isolation | Run engine offers `mcp_offers` and calls `call_mcp_tool` with approvals; streamable HTTP with vault-brokered auth |
| ACP | present (a206 extra): `agent206 acp`, ACP client for Claude Agent (`runtime/src/acp.rs`, `backends/claude.rs`) | — |

## Providers, models and auth

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| API-key providers OpenAI/Anthropic/Google/xAI/DeepSeek/OpenRouter/custom (`providers/mod.rs`, `custom.rs`) | partial: presets + OpenAI-compat/Anthropic/Gemini protocols (`core/src/providers.rs`), credentialed dispatch now runs through the broker for bound origins (`a206/crates/runtime/src/provider.rs`) | External backends and Copilot device flow still pending |
| GitHub Copilot OAuth device flow + session token (`providers/device_flow.rs`, desktop `device_flow.rs`) | missing | Broker-run device flow, token stored in vault |
| `refresh-token` / OpenClaw OAuth import (`rustyclaw-cli/src/commands/refresh_token.rs`) | missing | Import adapter via broker only |
| Model registry/`model_*` tools, defaults (`models/registry.rs`, `defaults.rs`, `tools/model_tools.rs`) | present in a206's form: API-discovered catalog, provenance/expiry (`provider.rs` catalog) | Static defaults intentionally not ported |
| Local engines Ollama/Exo/llama.cpp/LM Studio/Joshua + HF downloader (`engines/*`) | partial: Joshua local/SSH (`runtime/src/joshua.rs`), compatible endpoints | Engine lifecycle only for owned processes; never kill user services |
| Managed services via MCP (`services/*`, `tools/service_tools.rs`) | missing | Explicit process ownership model |

## CLI commands (`rc/crates/rustyclaw-cli/src/main.rs` Commands)

| RustyClaw command | Agent 206 status (`a206/crates/app/src/main.rs` Action) | Notes |
| --- | --- | --- |
| `setup`, `onboard`, `configure` | partial: `setup [--local\|--ssh]` resumes gateway, vault, TOTP and pairing on the shared flows, then runs doctor (`a206/crates/app/src/setup_cli.rs`); no `onboard`/`configure` aliases (their provider-key flags do not match) | Desktop/TUI wizard on the same steps; provider step |
| `import` (OpenClaw), `refresh-token` | partial: `import rustyclaw [PATH] [--apply]` with preview, broker-only vault copy and receipts (`a206/crates/import`); OpenClaw and `refresh-token` missing | More adapters per `docs/imports.md`; OAuth import through the broker |
| `config get/set/unset` | partial: `settings [--apply FILE]` TOML | Field-level commands |
| `doctor [--repair]` | partial: read-only `doctor [--toml]` over profile, gateway, broker, pairing and providers (`a206/crates/runtime/src/doctor.rs`), also on the desktop/TUI Gateway page; no `--repair` | Explicit, reviewed repairs |
| `tui`, `desktop` | present | TUI interaction QA pending |
| `command`, `ask` | partial: `new`, `chat THREAD TEXT`, `cancel` | Streaming, stdin/file prompts |
| `status`, `gateway start/stop/restart/status/reload/run` | present (no `reload`; adds `reconnect`, `--stdio`) | — |
| `model defaults` | n/a by design; `providers presets/add`, `refresh-models` | Model inspection/selection commands |
| `skills list/info/check`, `clawhub` | partial: `skills list/show/enable/disable` (requirements shown in list); `clawhub` missing | — |
| `swarm` | missing (`project NAME ROOT`) | — |

## Onboarding, TUI and desktop UX

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Onboard wizard: provider, key, vault, workspace, messaging, skills (`rustyclaw-onboard/src/*`) | partial: CLI `setup` walkthrough (gateway, vault, TOTP, pairing); the desktop/TUI have the gateway setup screen and Security page but no single wizard | GUI-first wizard; CLI uses same API |
| First-run "hatching" SOUL personality (desktop `components/hatching.rs`) | missing | Optional mode-prompt step |
| Panels: secrets, vault unlock, credential request, approvals, cron, memory, MCP (on the extensions page), channels, messengers, skills, services, engines, logs, analytics, system info, swarm, tool perms (desktop `components/*`, TUI `components/*_dialog.rs`) | partial: workspace, conversations, projects, connections, downloads, extensions, browser, documents, messengers, appearance, modes, security (vault create/unlock/lock, gateway pairing and TOTP enrollment for local profiles; `ui/src/vault.rs`), Joshua, gateway (`ui/src/route.rs`); secrets, credential request and approvals panels are not built | Each lands as routes when backing feature exists; no placeholder controls |
| TUI slash commands + tab completion (`rustyclaw-tui/src/components/command_menu.rs`) | partial: shared Dioxus TUI renders, untested interaction | Command palette in shared fc-dioxus |
| Thread tabs, tool-call rendering (`tui/components/thread_tabs.rs`, `tool_call.rs`) | partial: split view; no tool traces (no tool loop) | fc-dioxus tool trace component |
| Web client over WebSocket/WASM (`rustyclaw-web`) | present: authenticated loopback web, and opt-in HTTPS for other devices with sign-in by the vault's authenticator (`runtime/src/web.rs`, 2026-10-17) | ACME certificates; multiple users |

## Sandboxing and runtime isolation

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Landlock, Bubblewrap, Landlock+Bwrap, Docker, macOS sandbox-exec, path validation (`sandbox/*`, `runtime/docker.rs`) | partial: external backends, MCP servers (`a206/crates/mcp/src/sandbox.rs`) and native `execute_command` run under one `a206-sandbox` profile: a generated Seatbelt policy through `sandbox-exec` on macOS (`crates/sandbox/src/seatbelt.rs`), bubblewrap namespaces plus Landlock applied inside on Linux (`crates/sandbox`, `runtime/src/backends/isolation.rs`). Backends reach only a loopback proxy relay; MCP servers get no network, or the host network when granted; missing bwrap or Landlock is an error, never a fallback. On Linux the WASM extension worker also runs confined (no network, no project files, nothing executable but itself; `runtime/src/sandbox.rs`). Docker and path-validation modes are deliberately not ported (they are downgrades). The worker is unconfined on macOS | Confine the macOS worker with the shared profile |
| SSH runtime adapter for remote exec (`runtime/ssh.rs`) | missing (remote gateway via SSH exists) | Execute on remote gateway rather than per-command SSH |
| WASM tool sandbox | present (a206 extra): fuel/memory/time-bounded worker process, no WASI, only the two ABI host imports, each call re-authorized by the gateway (`a206/crates/extensions`); on Linux the worker runs under bubblewrap + Landlock with no network | Add the macOS OS profile to the worker |

## Observability, deployment

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Observer trait, stats/log ring, analytics/logs panels (`observability/*`, `logging.rs`) | partial: durable events journal (`Command::Events`, `runtime/src/store.rs`) | Redacted tracing + stats events; logs route |
| Daemon PID/spawn mgmt (`daemon.rs`) | present: profile-locked socket service + persistent stop (`runtime/src/service.rs`) | — |
| Dockerfile/compose, cross ARM builds, install scripts (`Dockerfile`, `Cross.toml`, `website/install.sh`, `scripts/setup.sh`) | missing (macOS bundle only) | Verified installers; Linux packaging; Windows named pipes |
| Gateway TLS/WebSocket listener (`rustyclaw-gateway/src/listen.rs`) | n/a by design (OpenSSH + Unix socket) | — |
| Downloads registry (`downloads.rs`) | present: SHA-256, cancel, atomic publish; no resume | Range resume, retries |

## Most foundational gaps (dependency order)

1. **Isolated vault broker + encrypted UI channel, TOTP enrollment, credential policies.** Blocks
   every credentialed provider, Copilot device flow, web search/HTTP keys, messengers, browser
   injection, setup wizard and imports.
2. **Native agent run engine with streaming and a tool-call loop**, including approval events
   (Allow/Ask/Deny), `ask_user`, retries without silent fallback, tool healing and output budgets.
   Without it no tool, memory, cron or channel feature is reachable by a model.
3. **Extension ABI v2** (MessagePack input, scoped host calls for workspace/exec/network/git, typed
   UI contributions, reviewed install). Partial: the ABI, SDK, workspace read/search/write, git read, network fetch and state host
   calls, typed panels, user grants and user package install are in; exec/git-write host calls and agent-prepared packages remain. Every RustyClaw tool family is meant to ship as a WASM builtin.
4. **OS sandbox for the macOS worker.** External backends, MCP servers and native `execute_command`
   are isolated on macOS and Linux by one `a206-sandbox` profile; the WASM worker on Linux. The
   macOS worker still runs unconfined (`runtime/src/sandbox.rs`).
5. **Context management**: compaction keyed to API-reported limits, workspace instruction files,
   memory extension. Needed for long-running and scheduled agents.
6. **Delegation/task lifecycle** (agent-requested spawn, steering, hierarchical cancel, budgets) →
   then swarm, cron/triggers/heartbeat on the same scheduler.
7. **MCP client + skills/instruction packages**, then messenger channel extensions.
8. **Setup/onboard, import adapters, doctor, config CLI** on top of 1–3; then installers/packaging.
