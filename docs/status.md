# Implementation ledger

Agent 206 is a working first foundation. It is not yet a complete successor to
RustyClaw. This ledger prevents the UI, docs or future contributors from mistaking
implemented structure for finished agent execution or security parity.

## Current implementation

| Area | Available now | Remaining work |
| --- | --- | --- |
| Rust architecture | Small Rust workspace, typed shared commands, forbidden application unsafe, pinned dependencies | Further extraction as capabilities grow; measured build/runtime budgets |
| Desktop | Native macOS app bundle, shared fc-dioxus interface, automatic local gateway startup, shared lifecycle controls | Windows named pipes; Linux desktop packaging and hardware validation |
| Gateway lifecycle | Shared Gateway page and controller, status from the actual process, start/stop/restart/reconnect, durable deliberate-stop marker, cleanup before acknowledgement, bounded control receipts; web uses an authenticated user-only binary endpoint | Remote SSH acceptance, stronger cross-process lifecycle coordination, encrypted enrollment and authentication; running is not security readiness |
| Initial setup/import | Shared Get started and Security use gateway/broker state for setup. Settings import has isolated source reading, per-item GUI/CLI review/apply, source/target guards and explicit receipt partial results. Broker-held password-vault review/apply/cancel is available in Security and `vault import FILE [--apply]`, with native/encrypted requests, password/TOTP checks, source identity guards, no overwrite and private receipts. The older setup import delegates its vault category to the same broker operations. Other grants/hooks/plugins stay inactive; credentials never pass through gateway settings | Native desktop/TUI interaction; remote SSH vault controls; key files; conversations/instructions/memory; rollback; other reviewed adapters |
| Messengers | Telegram through the Bot API, Matrix through a homeserver's client API (no end-to-end encryption; never sends to an encrypted room) Discord through a Gateway WebSocket the vault broker opens and identifies, Slack through Socket Mode the broker opens (broker v8), IRC over TLS with SASL sign-in performed by the broker (broker v9; senders are services accounts), Signal through a signal-cli daemon the user runs on a local socket (it keeps the account keys; senders are ACIs), and Beeper Desktop's local API (WhatsApp, iMessage, Google Chat and other networks Beeper connects, answered as the user; chat list polled, only allowed chats read), run by the gateway; send-only webhook messengers (Slack, Discord, Google Chat, Teams or JSON text) that models post to with the native `send_message` tool, approved call by call, as they can to allowed chats of Telegram, Matrix, Discord, Slack, IRC, Signal and Beeper messengers (IRC and Signal through the running session), whose whole URL only the broker holds and sends to on its bound origin (broker v12) while it is paired with an unlocked vault; the broker spends the bot or access token, or the IRC account password, for the bound origin only; entries start disabled and allow no chat, and groups need listed senders; refused chats and senders, account and structured failures are in the status; Telegram and Matrix save their reading position in gateway state and resume it after a restart, answering each message at most once; allowed chats get one thread each and ordinary `Send` turns; `messenger` CLI, TOML settings, a shared desktop/web/TUI Messengers page, user test sends (`messenger test`, Send test message on a webhook card, Send test beside a connected messenger's allowed chat) and RustyClaw import; tested against a stand-in Bot API, a stand-in homeserver and a stand-in Discord API and Gateway a stand-in Slack Web API and Socket Mode, a stand-in IRC server with a real broker, a stand-in signal-cli daemon and a stand-in Beeper Desktop with a real broker, a stand-in webhook and a stand-in Bot API with a real broker through the gateway's native tool loop (deny, approve, a chat not allowed, disconnect), and through the gateway's settings API (docs/messengers.md) | Messengers page render-tested only (not clicked through in a running app), other services, Discord session resume, Slack thread replies, attachments, Beeper positions across restarts (it answers nothing sent while the gateway was stopped), Matrix encryption, IRC networks without `account-tag`, not tested against the real Telegram, Matrix, Discord or Slack services, a real IRC network, a real signal-cli, a real Beeper Desktop or real webhooks |
| CLI | Headless build, system status, nested gateway lifecycle/status, setup (gateway, vault, TOTP, pairing and a provider account with its key in the vault; the provider step is tested through the real binary without a key, since the key prompt needs a terminal), doctor, import (RustyClaw; scoped Claude Code settings; Codex model providers), messengers, chat, projects, settings, catalog refresh, downloads, text documents, Git status, vault, ACP/MCP, renderer entry points and ui alias | Full RustyClaw-compatible command families in docs/cli.md; more import adapters and remaining parity |
| Gateway setup | Clients connect only after first-time setup (the desktop, TUI and web setup screen or `gateway setup`) saves a local or SSH target in `connection.toml`; unknown SSH hosts require fingerprint confirmation into the profile's own known_hosts; changed keys are refused; typed `ConnectionError`s are traced and shown; the web bridge's setup endpoint is tested for bearer checks, unsafe endpoints and damaged saved setups, and web SSH setup was checked in headless Chromium against a local OpenSSH server | Gateway enrollment (vault/TOTP) is a separate gate; the web bridge's sign-in for other devices is single-user (the vault owner's authenticator), with in-memory sessions |
| Web access from other devices | Opt-in `agent206 web --listen ADDR --name HOST…`: HTTPS only (rustls), a generated self-signed certificate per name set kept in the profile with its fingerprint printed, or the user's PEM files; requests for unlisted hosts or other origins are refused; browsers sign in with a code from the vault's enrolled authenticator, checked by the broker (throttle, one-use codes, unlocked vault only); 12-hour in-memory sessions, at most 32, ended by sign-out or restart. Tested: host/origin/session checks, sign-in against a real broker with an enrolled authenticator (wrong, replayed and locked cases), certificate reuse and renewal; checked in headless Chromium over HTTPS (sign-in, gateway setup, reload, sign-out) | Certificate rotation without restart, ACME, per-device session list, multiple users |
| SSH | Ordinary OpenSSH, strict host verification, configured identities, keepalives, remote stdio bridge |  remote installers/updaters, remote acceptance test |
| Wire protocol | Versioned bounded MessagePack, fragmented/coalesced reads, strict decode, durable request receipts | Negotiated feature sets, pushed subscriptions, full reconnect/resume UX |
| Health checks | `agent206 doctor` (text or `--toml`) and the Gateway page's Health checks card share typed findings from `a206_core::doctor`, gathered by `a206_runtime::doctor`; the web client runs the same checks on its bridge (`/api/health`, bearer-gated, for the gateway the bridge uses): profile privacy, gateway setup and process, vault broker/lock/TOTP, gateway vault pairing (reported by the gateway, checked only while unlocked), each provider's credential against the broker's destination rule and its API catalog age; nothing is started or changed; unknown stays unknown; evaluation unit-tested, the CLI tested through the real binary, the bridge route tested in the runtime crate and the web card clicked through in Chromium. `doctor --repair [--yes]` asks before each shared repair (`a206_core::doctor::Repair`): clearing group/other permission bits on the profile and local vault file/directory, refusing symbolic links, and refreshing a missing or stale model list through the gateway when the credential is usable; unit-tested and tested through the real binary (declined prompt, narrowed mode, failed refresh reported with its cause). The Health checks card offers each repair as a button: a model-list refresh is the Connections page's gateway command, and a permission repair is made by the desktop/TUI client, or on the web by the bridge (`/api/repair`, bearer-gated), which makes only a repair its own checks offer at that moment; the bridge route is tested in the runtime crate, and the card is not yet clicked through | Remote-host broker checks (run doctor on that host) |
| Errors/diagnostics | Per-module `thiserror` enums with source chains and explicit kinds up to each handling point (gateway validation, conflicts and missing resources are classified, tested for gateway replies); structured `ErrorReport` (kind, message, causes) in replies, lifecycle reports, downloads and failed runs, compatible with message-only payloads; `tracing` events in gateway, controller and clients; desktop/TUI error notices with expandable causes; CLI prints the full chain; URL redaction in transport errors; `let _ =` discards denied by clippy | Typed errors in the app binary below its top level, localized messages, error telemetry/export, retry guidance per kind |
| Persistence | One gateway writer, SQLite WAL snapshots/journal, revision checks, restart recovery, explicit replay gaps | Paginated/archived history, migration tooling; current snapshot is bounded below 4 MiB |
| Activity | Activity page (desktop, web, TUI) and `agent206 activity` over the gateway's event journal: notable events newest first in pages of up to 100, including conversations started and renamed and how each reply ended, without conversation or download progress notices, refreshed as new events arrive, each linked to its conversation or page (gateway paging test, CLI test against a real gateway, render and merge tests) | Events say that something changed, not what (a rename keeps no earlier title); no filtering or search; logs and usage analytics views |
| Chat | API-discovered model selection, native/compatible text generation, cancellation, persisted text attachments (8 files/128 KiB), image attachments for native providers (up to 4 PNG/JPEG/GIF/WebP images of 2 MiB per message, stored by digest outside the snapshot, sent in each protocol's form newest first within the broker's request bound, refused when the models API reports no image input; the answered message's images also go to Codex as data URLs and to Claude Agent as ACP image blocks when the adapter advertises image prompts; composer staging, message previews and `chat --image`), API-advertised reasoning effort; streamed text from external backends and from native OpenAI-compatible, Anthropic and Gemini SSE replies (direct and through the vault broker's bounded stream frames), published to the event journal while the reply is open; reasoning streamed by OpenAI-compatible servers (`reasoning_content`/`reasoning`), Anthropic (`thinking_delta`) and Gemini thinking models (thought summaries, requested only for models the API reports as thinking) journaled beside the reply, shown collapsed above it, kept to 128 KiB with the cut marked, and never sent back to a model; malformed, truncated and in-stream provider errors are typed and leave the run Failed with partial text; a request that failed before any response arrived (connection, timeout, 408/429/5xx/529) is retried to the same model with backoff, shown on the activity line and cancellable (runtime and broker tests against local mock APIs and a real broker; not tested against real vendor APIs) | PDF/audio attachments, Anthropic summarized thinking (current Anthropic models stream no thinking text by default), an idle (rather than total 180 s) stream timeout |
| Providers/models | Configurable hosted, local and custom preset library with GUI editing and TOML overrides; compatible, Anthropic and Gemini text APIs; bounded catalog pagination, provenance/expiry and refresh; unknown facts remain unknown | Isolated authenticated provider broker, accounting integrations and real vendor acceptance; model facts stay API-only |
| Coding-agent backends | Optional Codex app-server and Claude Agent ACP client adapters; per-thread selection/grants, macOS (Seatbelt) and Linux (bubblewrap namespaces + Landlock, loopback-only proxy relay) OS isolation, restricted model proxy, streamed text, operation summaries, bounded I/O, cancellation/process-group teardown; exercised with independent mock peer executables | Authenticated broker integration, real vendor-runtime acceptance, Windows isolation, installation/updating, session resume, interactive approvals and external subagent policy; runtimes are not bundled |
| Native tool loop | Gateway-owned run engine (`runtime/src/gateway/run.rs`, see docs/run-engine.md): streamed tool calls parsed for OpenAI-compatible, Anthropic and Gemini (incremental argument deltas, Gemini thought signatures returned); meaning-preserving argument repair, exact conversion of scalar arguments to their declared schema types and in-response de-duplication, with a run's fifth consecutive identical call and later ones noted to the model and shown in the trace, truncated arguments refused; tool-call markup a model writes as reply text removed while streaming (outside code); per-tool Allow/Ask/Deny (unset = deny) combined with the host's policy, every ancestor's setting and the grants of the conversation and every ancestor, re-checked for each call, with a running call or pending approval stopped when that authority is withdrawn; Ask publishes a journaled approval request answered only by `ResolveApproval` with the call's argument digest, single-use and expired on cancel or restart; per-conversation round/tool-call budgets, each bounded by every ancestor's; recorded arguments and outputs bounded by the remaining conversation storage; last-request usage for context kept apart from summed run usage for cost; cancellation during streaming, a tool call or an approval wait; tool failures and refusals reach the model as result text and clients as `ErrorReport`s; desktop/TUI tool traces, approval cards and per-tool permission selects; CLI `tools` and `approvals`; a native `ask_user` tool whose questions (suggested answers, optional free text) wait for `AnswerPrompt` from the conversation's question card or `agent206 prompts`, bound to the asking conversation, expired on cancel, revoke or restart; a native `plan_update` checklist kept on the conversation, shown as a plan card and by `agent206 plan`; a native `session_status` tool reporting the conversation's own model and API-reported limits, permissions, per-response limits, provider-reported usage alone and with subagents, subagent states and the gateway clock, reading no other conversation; tool results shortened before the model sees them by shipped and user TOML output rules (git, cargo, npm, ls; see docs/output-rules.md), with the rule and original size recorded on the call and shown in its trace; credential-shaped text (API keys, forge and messenger tokens, private keys, bearer tokens, JWTs, URL passwords) removed from every tool result before the model or the stored conversation sees it, with the kinds recorded and shown (`leak-scan`); text in tool results addressing the model (instruction override, role reassignment, chat-template markers, prompt or credential requests, tool-call markup) marked for the model as tool data and shown in the trace, with invisible Unicode tag characters removed (`injection-scan`, advisory); Native execute_command offered on Linux and macOS with sandbox isolation and approval, with conversation-owned background sessions, optionally on a pseudo-terminal, with user-chosen read-only program directories, followed by `process` (terminal sessions resizable) and fed by approved `process_write` text and terminal keys (tested through the real sandbox on Linux and macOS CI); WASM extension tools offered by default as `ext__<extension>__<tool>` once the user enables and grants the extension, still denied per conversation until permitted; MCP server tools offered by default as `mcp__<server>__<tool>` for enabled servers the conversation's scope covers, with the server's per-tool policy (Ask by default) as host policy and every call authorized again by the gateway's MCP call path, and a running call or pending approval stopped when the user disconnects, disables or changes the server (runtime tests against local mock APIs with a test-only tool host, and with the bundled workspace extension through the real worker: permit/approve/run, argument errors, capability ceiling, disabled extension, cancellation killing the worker, which is confined on Linux; with the Rust MCP fixture served in process and attached through a test-only hook: deny by default, Ask → approve → result, server Deny capping Allow, missing capability, unavailable server, tool-reported errors, withdrawal stopping a running call or approval, cancellation reaching the server; UI render tests of traces) | Native-run tests do not launch an MCP server through its sandbox (that path is tested in `a206-mcp` alone); the bundled extension's tools read, list, search and (with a separate write grant) write and edit workspace files; approval card interaction and the permission panel are not render-tested; the question card is render-tested but not clicked through in a running app; output rules are edited as files only, with no GUI or CLI listing; credential removal matches by shape only and does not scan instruction files, attachments or outbound requests; injection marking matches phrases only and does not scan user or messenger messages; Anthropic thinking blocks are not replayed with tool results; no real vendor acceptance |
| Web search | Implemented and tested: native `web_search` over Brave Search or Exa services the user adds (Connections page, `agent206 search-services`, TOML settings), key held by the vault broker and attached for the bound origin only (Brave's header since broker v15), needs the conversation's network grant and permission; options the chosen service lacks are refused; results bounded and passed through the usual credential redaction and injection scan; tested against stand-in Brave and Exa services through a real broker and the native tool loop, settings validation and the Connections section render-tested (docs/run-engine.md) | Not tried against the real Brave or Exa APIs; other search services; Connections section not clicked through in a running app |
| Usage/context | Compact composer widgets for last-request context used/limit, reported response cost, API prices and reported account/token balances; detail popover; usage clears when model selection changes; conversation totals over every run, alone and with subagents (sums of reported figures, cost per provider, exact only when nothing went unreported or unfinished), in the popover and `agent206 usage` | Richer history graphs and provider currency semantics; totals across conversations or providers |
| Context management (native runs) | Implemented and tested: automatic compaction when the last request's reported input tokens exceed 75% of the API-reported context limit (never with an unknown limit), user-triggered `CompactThread` (context popover, `agent206 compact`), summaries stored as visible typed history items while the original messages stay in the thread; project instruction files (`AGENTS.md`, `CLAUDE.md`, `SOUL.md`, `IDENTITY.md`, `TOOLS.md`) loaded only with `ReadWorkspace`, cap-std scoped and size-bounded, with per-file provenance; a pre-compaction memory flush round, offered once per summary when the conversation may call `memory_save` and the round budget leaves room to answer (runtime tests against scripted OpenAI/Anthropic/Gemini streams: threshold, unknown limit, manual, cancellation, summary failure, tool pairs, grants/ancestors, symlinks, limits, flush offered once, denied save, round budget) | No flush before a manual compaction; a single turn larger than the window is not reduced; compaction does not free conversation storage; context popover is not render-tested; no real vendor acceptance |
| Modes | Chat, Plan, Code, Review, Research; editable system prompts; mode ceilings | Mode-specific run engines, tools and approval UX |
| Projects/subagents | Project coordinator conversations, parent-linked delegated threads, explicit model allowlists, narrowed grants, revocation checks; experimental `subagent_*` tools let a native run start, message, wait on and cancel its own subagents within those limits, and cancelling a run cancels its subagents; `Steer` adds the user's or a parent's message to a running native run, read between rounds and answered before it ends (native run tests with a scripted provider; composer rendering only); `PauseRun`/`ResumeRun` hold a native run and its subagents before their next round (native run tests; composer status and palette unit tests); a Running work page and `agent206 running` list every running response with its subagents and what waits on the user (rendered-page and CLI listing tests); a Waiting on you page (`ui/src/inbox.rs`, counted in the sidebar) answers every pending tool approval and `ask_user` question from all conversations and subagents in place, oldest first, with the same cards as the conversation, and offers nothing while the gateway is offline (rendered-page tests); conversation details show where a conversation was delegated from and its subagent tree with each one's state (rendered-page tests); optional per-run token budgets bounded along the delegation chain (scripted provider tests); user-written agent profiles (TOML) that a subagent can start from, which only narrow its mode, model and capabilities and add instructions (parsing and scripted provider tests; Modes page and delegate control unit-tested for their choices, not clicked through); user-written agent teams that start a coordinator subagent which may delegate only to the team's member profiles (parsing and scripted provider tests; Modes page card and Start team control not clicked through) | Autonomous project management, task assignment |
| Scheduled messages | Experimental: at/every/cron jobs (seconds, IANA zones) that send a message to a conversation under its current grants; one catch-up run after downtime, pause/resume/run now/remove through gateway commands, `agent206 schedule` and the Schedules page; native runs add jobs to their own conversation only with per-call approval; heartbeat checks send the project's `HEARTBEAT.md` under the conversation's grants and record `HEARTBEAT_OK` replies as quiet (scripted provider test); trigger checks ask an enabled extension's declared trigger whether it fired, under the user's grant to the extension and the conversation's grant, and send its report (untrusted text, credentials removed) only when it did, with bundled `file_changed` and `page_changed` triggers (tested through the real extension worker); each job keeps its latest 50 runs with their outcomes and end times (Schedules page, `agent206 schedule runs`); the user can have each completed run's reply posted to a webhook or allowed messenger chat, with each post's result recorded (webhook stand-in test through the vault broker); tested as pure due-time functions, gateway commands, a native run against a scripted provider and a rendered page (`docs/schedules.md`) | Clicking the page through in a running app |
| Concurrent UI | Resizable navigation/chat/inspector, up to four visible conversations in resizable columns kept in the address, independently scoped controls, conversation title editing, searchable complete conversation list, message-text search across every conversation (gateway `SearchMessages`, Conversations page card whose results open at the matching message, and `agent206 search`), compact integrated composer controls, themed raised/feature surfaces | Saved layouts and comprehensive terminal interaction QA; more than two columns not yet tried in a running desktop or browser |
| Navigation | Typed Dioxus Router routes, persistent shell, fc-dioxus links, Back/Forward controls, conversation and split-view URLs, deep-link reloads, loading/missing recovery ; a command palette (Ctrl+K/⌘K or Go to… in the top bar) that filters every page, conversation and the open conversation's stop/summarize actions as you type (`ui/src/palette.rs`), and composer slash commands `/new`, `/rename`, `/compact`, `/stop`, `/pause`, `/resume`, `/mode`, `/model`, `/effort`, `/in`, `/every` with Tab completion (`ui/src/slash.rs`); unit and render tested, keyboard handling not yet exercised in a running TUI | Cross-profile deep-link handling, draft/form preservation across route unmounts, terminal interaction acceptance |
| Vault | SecureStore-compatible encryption, private atomic writes, typed credentials and all five RustyClaw access policies; isolated broker with verified TOTP, replay protection, authentication throttle, step-up and destination-bound provider requests; SSH keys (`vault add --kind ssh-key`, Ed25519 or ECDSA) sign only SSH authentication requests for their bound `ssh://host:port` servers through broker v16, which no SSH client uses yet. Shared desktop/web Security controls create, unlock, lock, enroll/cancel TOTP and pair a local gateway; TUI vault controls are refused because its pinned renderer lacks safe password input. Web setup requires independently verified broker identity and uses a bounded Noise NK ciphertext relay (`/api/vault/channel`, broker v10); `/api/vault` retains public status/lock only. Enrollment offers a locally generated QR plus a manual-key alternative. Private broker v17 adds bounded metadata pages and fixed-policy API-key creation. Tested against the real broker and gateway, encrypted HTTP relay, UI action rendering and QR decoding; browser setup acceptance described in docs/web-vault-setup.md | Remote SSH vault controls, advanced GUI credential entry and policy management and TOTP removal (CLI only), agent approval UI, recovery codes, key-file migration, modern vault-envelope migration, credentialed external backends and browser injection |
| Security model | Deny-by-default grants, no secret-bearing agent tool, restricted MCP, bounded worker, SSRF-resistant public downloads, capability-scoped text reads | Full RustyClaw defense parity, an OS-confined Windows WASM worker, leak/injection boundary scanning, adversarial review |
| MCP client | Experimental: user-configured stdio servers, disabled by default, run under macOS sandbox-exec or Linux bubblewrap + Landlock with only declared authority (project read/write, commands, network, and the user's program directories when the entry opts in) and a cleared environment; per-tool Allow/Ask/Deny; calls authorized against the thread's current grant and project; bounded lines, listings, results and time; status, tools and typed failures in the snapshot; Extensions page and `mcp-servers` CLI; offered to native runs through the run engine with per-call approvals, and a dropped or cancelled call is cancelled at the server (`docs/mcp.md`, `docs/run-engine.md`). Tested against a Rust fixture server as a child process, through the real macOS and Linux sandboxes on CI (files, symlinks, network and program grants), gateway policy denials, and native runs with the fixture served in process | Streamable HTTP, vault-brokered server credentials, Windows isolation, resources/prompts, network sandbox acceptance |
| Skills | Experimental: `SKILL.md` folders in the profile (AgentSkills/OpenClaw frontmatter), listed with parse failures and unmet requirements, off until the user approves the exact content (SHA-256 pinned; edits to any supporting file withdraw approval), reviewable package text, bounded supporting files without traversal or symlinks; CLI and Extensions page; bundled `agent206.skills` WASM `skill_list`/`skill_info` tools for native runs, denied per conversation until permitted (`docs/skills.md`); an advisory audit of SKILL.md and supporting text, reported by file and line before package approval; opaque files explicitly flagged (package-reader and gateway tests). Gateway tests cover approval, edits, failures and path escapes; a native run test reads an approved skill through the real WASM worker; complete-result bounds include remaining conversation storage | ClawHub install with audit, agent-prepared skills, SkillOnly credentials, project skills |
| WASM | ABI v2: Rust guest SDK; bundled `agent206.workspace` compiled to WASM by `build.rs`; separate worker child per call relaying MessagePack host calls, confined on Linux by bubblewrap + Landlock and on macOS by Seatbelt, with no network, no project files and nothing executable but itself (tested on both by an in-sandbox probe on CI; a missing layer fails the call); only the two ABI imports, no WASI; fuel/memory/input/output/host-call/time bounds; workspace read/list/find (glob)/search (literal or regex, case folding, include glob), both skipping what `.gitignore` files and `.git/info/exclude` exclude unless asked (tested with nested, negated and anchored rules), and write/edit/create-directory/multi-file patch (cap-std scoped, atomic writes, a patch checked whole before any file is written, separate write grant), git status/log/diff (gitoxide, no repository-configured programs; bundled `agent206.git`) branch and tag listing, and staging, unstaging, commits of named files or of what is staged, branch creation and plain or annotated tag creation under a separate git-write grant, refused where git would sign, run hooks or convert content (tested against gitoxide-built repositories), approved skill imports restricted to the skills builtin, HTTPS fetch to manifest-declared hosts through the SSRF guard (bundled `agent206.web`, raw or as readable Markdown/text extracted in the guest within a metered fuel budget; tested against a scripted network, never a live one); bundled `agent206.memory` with BM25 `memory_search` and `memory_get` over `MEMORY.md` and `memory/**/*.md` (read grant) and `memory_save` appending to notes under `memory/` (separate write grant), paths and symlinks refused in the guest, self-metered within the invocation fuel (guest unit tests with generated corpora; fuel bound tested against six adversarial corpora in the real interpreter; native-run tests through the worker for grants, path confinement and the write split) and per-extension state host calls checked against user grant ∩ caller ceiling ∩ declaration; tools callable from native runs within the conversation's grant (tested against real WASM and adversarial modules; end-to-end smoke test through the worker; native-run tests through the worker); user-installed packages validated before storing, digest-pinned and verified on every load, disabled with nothing granted until the user enables them (gateway tests through the real worker, CLI smoke test over the wire); agent-prepared packages through the native `extension_propose` tool, held for review apart from installed extensions and installed only by the user, with no grants carried over (native-run tests through the real gateway, bounded at 8); native `extension_list` reporting each tool's offered name and what blocks it, and waiting packages, bounded to one result (unit tests over the store; native-run test through the worker); an advisory pre-install audit of package and proposal text, shown by field before the user enables or installs (`a206-audit` unit tests per rule, with ordinary text each rule must spare) | Exec host calls, memory consolidation, vector or graph memory, a host-side memory index for corpora beyond one invocation's fuel, a live-network fetch test, OS confinement for the Windows worker, registry install; the page's install and proposal controls are not tested in a running desktop or browser |
| Plugin UI/API | ABI v2 documented (`docs/plugin-api.md`); manifests declare typed tools and panels; validated panel node trees rendered with fc-dioxus on the Extensions page (desktop/TUI, and the web client through its bridge: a bundled panel opened in Chromium after granting its permission) and as text in the CLI; button events; per-capability grant and enable switches in app chrome | Viewer/command contributions, subscriptions, richer components |
| Downloads | HTTPS monitor, progress events, cancellation and partial-file cleanup, SHA-256 verification, atomic no-clobber publication, transfer library or ~/models | Range resume, persisted ETags, retries, remote model transfer, agent-scoped subscriptions |
| Joshua | Owned local/SSH child start/stop, loopback forwarding, user-selected file/port, API model discovery, owned provider retirement on stop/observed exit/restart | Installation/updating, verified remote supervision/readiness, one-step model switching, real model/remote-host acceptance tests |
| Documents | UTF-8 text/Markdown source preview scoped by cap-std to a project | WASM format plugins, editing/rendering, PDF/office viewers, ArtCraft evaluation |
| Git/PR/CI | Scoped read-only Git status with bounded output and disabled fsmonitor; extension git status/log/diff, branch and tag listing, staging and unstaging of named files, commits of named files or of what is staged, branch creation, plain and annotated tag creation, switching branches from a clean working tree, and the native `git_push` (fast-forward of one branch, or one tag the remote lacks, to an HTTPS remote over git's smart HTTP protocol, each call approved, token sent by the vault broker as Basic auth for its bound origin only, broker v13), and the native `git_fetch` (one branch into its remote-tracking branch, or one tag into a new local tag, through the same credential, each call approved, forced updates only where the refspec has `+`, fetched history checked complete before the branch moves, broker v14), and the read-only `pull_requests` and `pull_request` tools (pull requests, checks and reviews described forge-neutrally by crate `a206-forge`, with GitHub, GitLab and Gitea/Forgejo adapters, read through the vault broker with the same credential as a bearer token), and `pull_request_create`, `pull_request_comment`, `pull_request_line_comment` and `pull_request_merge` (open a pull request, post a comment, comment on a line of its changes, or merge one, the last two at the head commit the user approves, each call approved); tested against a stand-in smart HTTP forge whose packs gitoxide indexes and a stand-in GitHub/GitLab/Gitea REST API, not against a real forge | ssh remotes, force push, pushing or fetching several references at once, fetching several branches or pruning, pushes and fetches over about 4 MiB, line comments over a range or on GitLab renamed files, replies to review comments, more than 30 pull requests or 100 checks per kind, setting a forge API in the GUI |
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
preview has no credential injection. Third-party extensions install only from a package the user supplies or a package an agent prepared and the user installs; registry installs are unavailable.
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
compiled and launched against the pinned Dioxus 0.7 fork. A macOS PTY attempt at
`91b8e57` (140 columns, 45 rows, `TERM=xterm-256color`) produced overlapping or
truncated text and an unsupported-image marker. Ctrl+K and Tab/Ctrl+K did not
visibly open navigation in the captured output. The owned session exited with
Ctrl+C; the disposable profile's gateway remained stopped and no vault was
created. This is failed terminal acceptance, tracked in [#153](https://github.com/rexlunae/agent-206/issues/153),
not a completed Security interaction check. Layout, graphics fallbacks and
keyboard navigation still need investigation and acceptance.

The terminal root now embeds the unchanged bundled toolkit styles and its own
app stylesheet as style text, using fc-dioxus `dc98bdd` (inline delivery and
native-compatible hidden attributes). Desktop and web keep linked stylesheets.
A repeat 140×45 PTY run showed navigation text, but still omitted the main pane,
showed an unsupported-image marker and produced no visible palette after Ctrl+K
or Tab/Ctrl+K. It exited cleanly with Ctrl+C and the disposable gateway stayed
stopped. The renderer's independent flex-row painting failure is tracked in
[dioxus-tui#5](https://github.com/JakkuSakura/dioxus-tui/issues/5); keyboard response
still needs separate diagnosis. This is partial resource integration, not passed
terminal interaction acceptance.

An isolated public-marker probe also found that the pinned terminal renderer
paints password values as plain text ([dioxus-tui#6](https://github.com/JakkuSakura/dioxus-tui/issues/6)).
The TUI therefore does not mount vault controls, including setup, unlock,
authenticator enrollment, imports and API-key entry; it shows desktop/CLI recovery
instructions instead. No real secrets were used in the probe. Masking output
alone would not establish safe native value storage or tracing. Desktop and web
vault controls remain available; terminal credential acceptance remains blocked.

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

The shared Import page now offers RustyClaw settings review and per-item apply
through gateway-owned commands also available in the CLI. A release web client
was checked with disposable source/target profiles: blank source and empty
selection actions were disabled, an edited source was refused, a fresh preview
reset selections, and only the chosen provider and disabled messenger were
added with a receipt. No vault or real credentials were created or imported.
Gateway tests additionally cover target revision changes, invalid selections,
replayed application and receipt-write partial failures; CLI tests exercise the
same commands through the real binary. Desktop/TUI interactive acceptance,
browser receipt-failure acceptance, remote SSH vault migration and other source
adapters remain pending. Existing provider labels, endpoints, protocols and vault
references can be edited on Connections or through `settings --apply`.
See [imports.md](imports.md).

The Import source selector and `import review-claude-code` also review one explicit
Claude Code settings.json directory through that isolated reader. Only an explicit
Anthropic Messages base URL is offered, with SDK request-prefix mapping and no
credential/model binding. Other scopes, shell settings, cloud/host-managed routing,
permissions, hooks, plugins and MCP activation stay behind. Unit tests and real
isolated-reader/gateway/CLI tests cover private data omission and guarded selected
apply. Release web acceptance with a disposable source verified source selection,
source-change refusal and cleared selections on re-preview, then selected apply
with a private receipt. Private/model source text was absent from the DOM, source
bytes stayed unchanged and no vault was created. Native desktop/TUI interactive
acceptance remains pending.

`import review-codex` and the Codex choice on Import review one Codex
`config.toml` through the same reader. Each `[model_providers]` table becomes an
OpenAI-compatible connection with its base URL unchanged and no credential or
model; reserved, Bedrock/AWS, sign-in, query-parameter and non-Responses tables
are refused. Codex speaks the Responses API, so an imported connection works only
if its endpoint also serves chat completions. Unit tests cover private-data
omission and each refusal; a CLI test runs the real isolated reader in CI. Not
tried against a real Codex installation; desktop/TUI acceptance pending.

1. Restore complete secret custody: encrypted broker IPC, explicit destination/use
   approvals, TOTP and lossless legacy vault migration. Enable authenticated model
   dispatch only after those boundary tests pass.
2. Add the project scheduler and the background task lifecycle.
3. Grow the extension SDK's host domains and builtins, and add registry installation
   with reviewed provenance and integrity verification.
4. Add verified local/SSH installers, remote supervision, browser/vault integration,
   document format plugins and forge-neutral PR/CI workflows.
5. Complete context/accounting controls and API integrations without introducing a
   static model catalog, fabricated prices or default account balances.

## Encrypted web enrollment update (2026-10-16)

The shared Security panel is available in the web renderer for a gateway local to
the web server. Broker-authenticated Noise NK carries password/TOTP requests and
seed replies through a ciphertext-only HTTP relay. A trusted-terminal public-key
verification precedes secret inputs. The panel supports create/unlock/lock,
step-up, begin/confirm/cancel TOTP and pairing, with actual broker state and
separate pairing failures. First-run gateway selection opens Security. Native and
web use the same typed setup subset and broker rules. See
[web-vault-setup.md](web-vault-setup.md). Remote SSH vault setup and migration,
key-file workflows and modern vault-envelope migration remain pending.
The shared settings import review/apply screen is described above; the existing
RustyClaw terminal vault import remains available.
## Supporting-file review update (2026-10-17)

Skill approval now covers the bounded package, including hidden supporting files.
The shared GUI and CLI review show each supporting file's path, byte count and
text where readable. Text-pattern findings identify file and line; binary or
NUL-containing files are flagged as uninspected. Adding, removing, renaming or
editing supporting content withdraws approval, and model reads use bytes from a
digest-checked snapshot rather than reopening the file after the check.
Standalone SKILL.md approvals remain compatible; packages with supporting files
require renewed review. Package-reader and gateway tests cover these boundaries.
The audit is advisory, not evidence that a package is safe. Registry installation
and provenance verification remain planned; GUI interaction acceptance is pending.

## Shared run-budget controls (2026-10-17)

Native conversations now offer a Run limits editor in the inspector and details
popover, including split views. It uses the existing gateway command for model
rounds, tool calls and optional API-reported token budgets, and shows saved and
ancestor-limited values separately. Unknown lineage stays unknown. Offline,
invalid and conflicting drafts cannot be saved; failures retain structured causes.
Input, lineage and render tests cover the shared UI. Interactive desktop/TUI
acceptance remains pending. These are user-defined execution limits, not model
catalog facts or a hard currency/spending cap.
The web controls were clicked through against a disposable local gateway: saves
from the inspector and details popover update both, invalid input blocks Save,
and a concurrent CLI edit preserves an unsaved draft and requires Reload. No
provider accounts, credentials or tool grants were configured for that check.

## Connection editor (2026-10-18)

Connections now offers an editor for existing account names, endpoints, protocols
and saved vault credential references in the shared GUI. It uses SaveSettings,
matching the CLI's `settings --apply`; account identity and unedited settings are
preserved. Polling does not overwrite drafts. A changed settings revision blocks
Save until explicit Reload; failed saves keep the draft and structured error.
Offline, in-flight, empty and unchanged drafts cannot be submitted. A gateway
switch remounts the editor rather than carrying a draft into another gateway.
The gateway retains endpoint validation, managed-Joshua protections, active-run
cancellation and catalog invalidation. This editor does not create credentials,
grant secret access or discover models automatically. Pure draft/gating tests
cover the shared editor. Release web acceptance with a disposable gateway
confirmed concurrent CLI edits preserve the draft and disable Save, explicit
Reload restores current values, gateway endpoint rejection retains the draft
and error, and successful edits appear immediately and reload correctly. No
vault, real credentials, model calls or grants were created. Native desktop/TUI
interaction remains unverified.

## Vault source identity (2026-10-18)

The vault library now offers bounded, read-only source identity capture and
reviewed import of exactly those bytes. A changed source is refused before
staging; TOTP retries recheck it. The existing private-copy authentication,
policy preservation and target no-replace behavior are shared with legacy
import. Tests cover equal-length source changes, size identity mismatches,
nonfiles/symlinks/oversized sources, wrong passwords, enrolled TOTP/replay,
unchanged source bytes and refusal to replace existing targets. Broker v11 now
holds reviews per connection and exposes schema-v1 preview/apply/cancel through
native and encrypted setup channels. Security/Get started use these operations,
as do `vault import FILE [--apply]` and the vault category of the older setup
import command. Old peers are refused without an unreviewed fallback.

Vault migration confirms the copy separately from receipt durability and
gateway pairing. Broker-written TOML receipts carry source identity, not secret
values; a failed receipt is an explicit partial result with structured causes.
Tests cover connection scoping, cancellation/replacement/lock invalidation,
source and target changes, factor enforcement, native/encrypted policy denials,
old peers, private receipts and receipt failure after copy. UI tests cover
version/input gating and preserving partial failures. Release web acceptance with
a disposable encrypted source verified source-change refusal, required source
TOTP, cleared password inputs, successful import/pairing, a private receipt and
unchanged source bytes. Native desktop/TUI interaction, remote SSH migration, key-file support
and rollback remain unverified or planned.

### Shared credential metadata and API-key entry

Security now has private broker metadata pages and API-key entry in the shared
fc-dioxus UI. Metadata includes kind, policy/scopes, disabled state and permitted
origins, never values. Broker v17 enforces step-up, fixed per-use approval and
atomic refusal to overwrite; global agent access is unchanged. Pages contain at
most 50 entries and 48 KiB encoded, with explicit errors for oversized metadata.
Native/encrypted integration tests cover the new operations and compatibility
refusal; desktop/TUI interactive acceptance is still pending. Other CLI-supported
entry kinds, edits, legacy adoption, removal, reveal and global access changes
remain CLI operations, including SSH-key entry. Form and payment entry remain unavailable.
Agent credential-use approval and remote SSH vault controls are pending.

Release-web acceptance before the v17 integration used a disposable encrypted password vault: private
metadata loaded, locked skill scopes and disabled flags were shown, a synthetic
key saved, duplicate replacement was refused, and a fresh session required
step-up before entry. Locking removed the unsaved form and metadata. The password
input had no DOM value attribute; the synthetic key was absent from DOM attributes
and text. An independent Rust check confirmed the saved policy/origin/value,
agent-access off, encrypted bytes and mode 0600. No real accounts or model calls
were configured. Native desktop/TUI interactive acceptance remains pending.

After integration with broker v17, release-web acceptance in a fresh disposable
profile verified broker-key confirmation, vault creation, metadata loading,
SSH-origin refusal without losing the nonsecret draft, key clearing after refusal,
corrected HTTPS-origin save, lock hiding metadata and automatic reload after
unlock. The synthetic key was absent from DOM text and attributes, the vault file
was mode 0600, agent access stayed off and the deliberately stopped gateway stayed
stopped. The temporary browser tab, web server and broker were closed afterward.
