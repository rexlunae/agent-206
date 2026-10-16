# RustyClaw → Agent 206 feature parity gaps

This is the working gap list for reaching RustyClaw parity. Update a row when its
status changes; `docs/status.md` remains the authoritative implementation ledger.

Scope decisions (2026-10-16): gateway communication keeps RustyClaw's SSH-based
protocol, and parity work targets desktop and TUI first. Web-client parity is
deferred; existing web support is kept but new gateway features need not reach it.

Reference: [RustyClaw `481a428c`](https://github.com/rexlunae/RustyClaw/tree/481a428cb7a3b82d1a5e556ffef40e3dd9f52ce3) (`rc/` below).
Agent 206 paths are prefixed `a206/`; the survey was taken at `1666303`. Parity = equivalent user-facing
capability under Agent 206's architecture (gateway-owned policy, separate vault/extension processes,
WASM tools, MessagePack APIs, API-only model facts, fc-dioxus shared UI). Status words:
**present** (works end to end in a206), **partial** (structure or subset exists), **missing**.

Key a206 facts that drive most rows: the native engine is text-only, no tool-call loop
(`a206/crates/runtime/src/provider.rs:342` sends `"stream": false`, no `tools`); credentialed providers
now go through the vault broker (survey time: refused); the agent-facing surface is the MCP harness with
one tool, `rename_thread` (`a206/crates/runtime/src/mcp.rs`); the `Command` enum
(`a206/crates/core/src/lib.rs:418`) is the entire gateway API; WASM ABI v1 has no inputs or host calls
(`a206/docs/plugin-api.md`).

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
| SSRF validator (`security/ssrf.rs`) | partial: public-only fetch for downloads (`a206/crates/runtime/src/network.rs`) | Reuse `network.rs` for every network-capable host call |
| Credential-guard override tool (`tools/guard_override.rs`) | missing (intentionally) | Only a user-approved, scoped exception in the approval surface |
| Skill security audit (`skill_audit.rs`) | missing | Run on extension/skill packages pre-install |
| SSH client pairing, Ed25519 keys, QR (`pairing/*`, `rustyclaw-gateway/src/ssh/server.rs`) | partial: OpenSSH stdio with strict host checks (`a206/crates/runtime/src/transport.rs`); no in-app pairing | Keep RustyClaw's SSH-based gateway protocol (decided 2026-10-16); in-app host-key pairing |

## Agent loop, tool calling and streaming

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Multi-round tool loop, rate-paced rounds (`rustyclaw-gateway/src/dispatch.rs`, `tool_executor.rs`, `tool_limits.rs`) | missing for native engine; external Codex/Claude Agent backends run their own loops (`a206/crates/runtime/src/backends/`) | Gateway run engine: model → tool request → policy check vs. `Grant` → WASM/host exec → result; budgets per thread |
| Tool approval Allow/Ask/Deny per tool (`tools/mod.rs:249` ToolPermission, desktop `components/tool_approval.rs`) | missing (grants are static capability sets, `core/src/lib.rs` Grant) | Approval requests as gateway events; fc-dioxus approval component; agent cannot self-approve |
| `ask_user` structured prompts (`user_prompt_types.rs`, desktop `user_prompt.rs`) | missing | Typed prompt event + UI response command |
| Tool-call healing: malformed JSON, dupes, leaked markup (`tool_healing.rs`) | missing | Part of run engine's parse stage |
| Tool-output compression rules (`rc/crates/tokenjuice`, `tool_pipeline.rs`) | missing | Reusable crate (MIT attribution); TOML rule overlays |
| Provider SSE streaming OpenAI/Anthropic (`streaming.rs`, `gateway/providers/mod.rs`) | partial: streamed text only from external backends; native `stream:false` (`provider.rs:342`) | Stream deltas as durable events with cursors |
| Context compaction at 75% window + pre-compaction memory flush (`gateway/helpers.rs`, `memory_flush.rs`) | missing (context used/limit widget only) | Use API-reported context limit; unknown limit ⇒ no auto-compaction, user control |
| Retry policy + model/auth-profile failover (`retry/policy.rs`, `models/failover.rs`) | missing | Retries with explicit external-effect semantics; **no silent model fallback** (thread allowlist) |
| System prompt + workspace files SOUL/AGENTS/TOOLS/MEMORY (`soul.rs`, `workspace_context.rs`, `gateway/system_prompt.rs`) | partial: editable per-mode prompts (`ui/src/settings.rs` PromptEditor) | Project-scoped instruction files read via ReadWorkspace grant |
| Receipt/freshness protocols (`protocols/*`) | missing | Optional; fold into task lifecycle receipts |
| Todo planning tool (`tools/todo_tool.rs`) | missing | Builtin WASM tool with thread-scoped state |

## Tools (all should become WASM builtins with manifest-declared capabilities)

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| read/write/edit/list/search/find files (`tools/file.rs`) | partial: read-only text `ReadDocument` scoped by cap-std (`gateway.rs` `scoped_path`) | `workspace` host domain: read/patch/search under ReadWorkspace/WriteWorkspace |
| `apply_patch` multi-hunk (`tools/patch.rs`), `ast_grep_manage` (`ast_grep.rs`) | missing | Workspace patch host call; ast-grep as extension |
| `execute_command`, `process` bg/poll/send_keys/kill (`tools/runtime.rs`, `process_manager.rs`) | missing natively (Execute grant only for external backends, macOS) | Brokered exec in OS sandbox, bounded output, process-group teardown (reuse `runtime/src/process.rs`) |
| `web_fetch`, `web_extract` (`tools/web.rs`, `web_extract.rs`) | missing (only download fetch) | Network host call through SSRF guard; readability in WASM |
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
| Git ops | partial: read-only `GitStatus` (`gateway.rs`) | Forge-neutral git/forge host domains (diff/stage/commit/PR/CI) |

## Memory

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| `memory_search` BM25 / `memory_get` over MEMORY.md + memory/ (`memory.rs`, `tools/memory_tools.rs`) | missing | Memory extension with scoped store |
| Two-layer MEMORY/HISTORY consolidation (`memory_consolidation.rs`) | missing | Background task with user-visible budget |
| Semantic vector memory, knowledge graph (`steel_memory.rs`, feature `semantic-memory`) | missing | Optional extension/service process |
| Hierarchical ingest summary tree (`rc/crates/memory-tree`) | missing | Reusable crate behind memory extension |
| `session_search`/`search_history` (`tools/session_search.rs`) | missing (UI conversation list search only, `ui/src/pages.rs`) | Gateway query over SQLite journal |
| Memory vault / protected memory (`memory_vault.rs`) | missing | Through vault broker |

## Sessions, threads, subagents, swarm, tasks

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Threads, projects (`threads/*`, `projects/*`) | present: threads, projects, parent-linked subagents (`core/src/lib.rs` Thread/Project) | — |
| `sessions_spawn/send/history/kill`, steer (`tools/sessions_tools.rs`) | partial: user `CreateSubagent` with model allowlist + narrowed grants; no agent-initiated spawn, steering or hierarchical cancel | Agent *requests* spawn via SpawnAgent; gateway enforces intersection |
| `subagent_create/run/list/delete` (`tools/subagent_tools.rs`, `gateway/subagent_runner.rs`) | partial (as above) | Same run engine as root threads |
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
| Telegram, Discord, Slack, Matrix, IRC, Signal/signal-cli, WhatsApp, Google Chat, Teams, iMessage, webhook, console (`messengers/kind.rs`, `gateway/messenger_handler/*`) | missing | Channel extensions in separate processes; inbound messages untrusted; per-channel grants |
| `message` send/broadcast tool (`tools/gateway_tools`) | missing | Channel host call requiring approval |
| Beeper/claw-me-maybe onboarding (`rustyclaw-onboard/src/messaging.rs`) | missing | Setup wizard step after extension system |

## Skills, plugins, MCP

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Skills: markdown/YAML, dependency gating, enable/disable (`skills/mod.rs`, `tools/skills_tools.rs`) | missing | Instruction packages (TOML manifest) separate from executable extensions |
| ClawHub registry install/search (`skills/clawhub.rs`, CLI `clawhub`) | missing | Reviewed install with manifest validation + audit |
| Agent-created skills/curator (`tools/skill_curator.rs`, `skill_create`) | missing | Agent prepares package; user enables |
| Plugin UI panels + state (`plugins/mod.rs`, `tools/plugin.rs`) | partial: one bundled text-only WASM panel (`a206/crates/extensions`, `plugins/inspector`) | ABI v2: MessagePack inputs, host calls, typed UI |
| MCP **client** connect/list/tools (`mcp/*`, `tools/mcp_tools.rs`) | missing (a206 has MCP **server** harness only, `runtime/src/mcp.rs`) | MCP client in isolated process; tools gated by grants; imported servers inactive |
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
| `setup`, `onboard`, `configure` | missing | Same enrollment state machine as GUI (`docs/cli.md`) |
| `import` (OpenClaw), `refresh-token` | missing | Adapters per `docs/imports.md` |
| `config get/set/unset` | partial: `settings [--apply FILE]` TOML | Field-level commands |
| `doctor [--repair]` | missing | Read-only by default |
| `tui`, `desktop` | present | TUI interaction QA pending |
| `command`, `ask` | partial: `new`, `chat THREAD TEXT`, `cancel` | Streaming, stdin/file prompts |
| `status`, `gateway start/stop/restart/status/reload/run` | present (no `reload`; adds `reconnect`, `--stdio`) | — |
| `model defaults` | n/a by design; `providers presets/add`, `refresh-models` | Model inspection/selection commands |
| `skills list/info/check`, `clawhub` | missing (`extension ID` runs bundled WASM) | — |
| `swarm` | missing (`project NAME ROOT`) | — |

## Onboarding, TUI and desktop UX

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Onboard wizard: provider, key, vault, workspace, messaging, skills (`rustyclaw-onboard/src/*`) | missing | GUI-first wizard; CLI uses same API |
| First-run "hatching" SOUL personality (desktop `components/hatching.rs`) | missing | Optional mode-prompt step |
| Panels: secrets, vault unlock, credential request, approvals, cron, memory, MCP, channels, messengers, skills, services, engines, logs, analytics, system info, swarm, tool perms (desktop `components/*`, TUI `components/*_dialog.rs`) | partial: workspace, conversations, projects, connections, downloads, extensions, browser, documents, appearance, modes, security (vault create/unlock/lock, gateway pairing and TOTP enrollment for local profiles; `ui/src/vault.rs`), Joshua, gateway (`ui/src/route.rs`); secrets, credential request and approvals panels are not built | Each lands as routes when backing feature exists; no placeholder controls |
| TUI slash commands + tab completion (`rustyclaw-tui/src/components/command_menu.rs`) | partial: shared Dioxus TUI renders, untested interaction | Command palette in shared fc-dioxus |
| Thread tabs, tool-call rendering (`tui/components/thread_tabs.rs`, `tool_call.rs`) | partial: split view; no tool traces (no tool loop) | fc-dioxus tool trace component |
| Web client over WebSocket/WASM (`rustyclaw-web`) | present: authenticated loopback web (`runtime/src/web.rs`) | — |

## Sandboxing and runtime isolation

| RustyClaw capability | Agent 206 status | How it should land |
| --- | --- | --- |
| Landlock, Bubblewrap, Landlock+Bwrap, Docker, macOS sandbox-exec, path validation (`sandbox/*`, `runtime/docker.rs`) | partial: external backends run under macOS sandbox-exec, or on Linux under bubblewrap namespaces plus Landlock applied inside, with a loopback-only proxy relay (`crates/sandbox`, `runtime/src/backends/isolation.rs`); missing bwrap or Landlock is an error, never a fallback. Docker and path-validation modes are deliberately not ported (they are downgrades); native exec and the WASM worker are not sandboxed yet | Reuse `a206-sandbox` profiles for native exec and the extension worker |
| SSH runtime adapter for remote exec (`runtime/ssh.rs`) | missing (remote gateway via SSH exists) | Execute on remote gateway rather than per-command SSH |
| WASM tool sandbox | present (a206 extra): fuel/memory/time-bounded worker, no WASI (`a206/crates/extensions`) | Add OS profile to worker |

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
   UI contributions, reviewed install). Every RustyClaw tool family is meant to ship as a WASM builtin.
4. **OS sandbox for native exec and the WASM worker.** External backends are isolated on macOS and
   Linux; native Execute/WriteWorkspace grants still need the same profiles (`crates/sandbox`).
5. **Context management**: compaction keyed to API-reported limits, workspace instruction files,
   memory extension. Needed for long-running and scheduled agents.
6. **Delegation/task lifecycle** (agent-requested spawn, steering, hierarchical cancel, budgets) →
   then swarm, cron/triggers/heartbeat on the same scheduler.
7. **MCP client + skills/instruction packages**, then messenger channel extensions.
8. **Setup/onboard, import adapters, doctor, config CLI** on top of 1–3; then installers/packaging.
