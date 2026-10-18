# CLI contract and RustyClaw compatibility

Desktop is primary; the CLI is a supported interface for users, SSH and automation,
not an internal debugging path. Both use the same typed commands, lifecycle
controller and security/authentication rules. New application-owned output and
configuration use TOML; owned wire APIs use bounded MessagePack. Keep diagnostics
on stderr, output on stdout, meaningful exit codes and no secrets in argv/env/logs.

Current headless build: `cargo build -p agent206 --no-default-features`.
Use `--data-dir` for an isolated profile. Before a client command connects, choose
the profile's gateway once with `gateway setup --local` or `gateway setup --ssh
ssh://user@host[:port]` (an unknown host prints its key fingerprints; rerun with
`--trust SHA256:...` after verifying one out of band). `--remote ssh://user@host`
overrides the saved choice for one invocation. Unconfigured client commands fail
without launching a gateway. With no subcommand, the app launches the desktop; `tui`
(alias `ui`) explicitly launches the shared terminal UI when built with that feature.

| RustyClaw command family | Agent 206 now | Required follow-up |
| --- | --- | --- |
| desktop / tui | desktop, tui / ui | Terminal interaction and graphics acceptance |
| status | status: process and live workspace counts; gateway status: process only | Richer API-sourced usage, profile selection UX |
| setup / onboard (gateway target) | gateway setup --local / --ssh with fingerprint review; same saved TOML as the desktop/TUI setup screen | Gateway security enrollment (vault/TOTP) after target selection |
| gateway start / stop / restart / status | Same nested commands, plus reconnect; stop-gateway compatibility entry point | Verified installers/updaters and real SSH acceptance |
| setup / onboard / configure | setup [--local \| --ssh URL [--trust FP]] [--skip-totp]: chooses the gateway when asked, then on a local profile creates or unlocks the vault, offers TOTP enrollment and pairs a running gateway through the same broker requests as the GUI, skipping finished steps, and prints the doctor report. A closed stdin declines every step. Remote profiles are told to run setup on the gateway host | Provider step, onboard/configure aliases only if semantics match (the GUI Get started page shows the same steps) |
| config get / set / unset | settings exports TOML; settings --apply FILE uses revision-checked shared API (including `program_directories`, the extra directories sandboxed commands may run programs from) | Reviewed field-level commands and parity aliases |
| command / ask / chat | new TITLE, chat THREAD TEXT, steer THREAD TEXT (add a message to the running native run, answered before it ends), pause THREAD / resume THREAD (hold the native run and its subagents before their next model round), running (every conversation with a response in progress as TOML: parent, paused/held, waiting approvals and questions), cancel THREAD, compact THREAD (summarize all but the latest turn of a native conversation; `cancel` stops it; refused with a conflict when there is nothing to compact); search QUERY [--limit N] prints the messages matching it in every conversation as TOML hits; tools list/permit/budget THREAD (budget takes --rounds, --tool-calls and an optional --tokens per run) and approvals list/approve/deny and prompts list/choose/answer/decline for the native tool loop; plan THREAD prints the checklist the conversation's models keep with `plan_update` as TOML `steps` (text and status) (answer from a second terminal while chat waits) | Streaming, interactive/headless sessions with inline approval prompts, stdin/file prompts and clear compatibility options |
| model / providers | providers presets/add, refresh-models; API-only catalogs; existing accounts edited through settings --apply, matching the GUI Connections editor | Field-level account editing, model/account inspection and selection commands with no static model defaults |
| secrets | vault init/unlock/lock/status/stop, import FILE [--apply] (broker-held review, source identity and password/TOTP checks without starting a gateway), pair, list, add, adopt (untyped legacy entries, optionally renamed with `--as`), policy, bind, enable/disable, delete, agent-access, reveal (step-up), totp enroll/remove, all through the vault broker | SSH key, form and payment entry; recovery codes; key-file import; remote broker over SSH; credential-editor GUI parity |
| import | import review-rustyclaw /absolute/source and import review-claude-code /absolute/settings-directory (gateway-host TOML reviews; Claude Code carries only an explicit Anthropic endpoint); import apply-review ID --provider INDEX --messenger INDEX (repeatable zero-based indices, source/target checked); import rustyclaw [PATH] previews through isolation; --apply uses the same reviewed vault broker operations as Security and vault import, adds missing provider accounts and disabled messenger entries, and writes receipts; --skip-vault / --skip-providers / --skip-messengers | Cline, Codex, DeepSeek Harness and broader Claude Code data adapters in imports.md; remote SSH vault migration, key files and rollback |
| mcp | mcp-servers list/enable/disable/connect/disconnect; entries added through settings --apply (docs/mcp.md) | Field-level add/edit commands; HTTP servers |
| skills / registries | skills list/show/enable --digest/disable (approval pinned to the reviewed content, docs/skills.md); extension list, grant ID [--capability C…], enable/disable ID, panel ID PANEL [--project P] [--press BUTTON], tool ID TOOL [--project P] [--args TOML], install MANIFEST MODULE (arrives disabled, nothing granted; installing a new version updates it), uninstall ID, install-proposal DIGEST and dismiss DIGEST for packages agents prepared (list shows them), all through the gateway's extension policy | Registry install/search, skill install, export |
| swarm / projects | project NAME ROOT creates a coordinator; snapshots retain linked threads; profiles lists the agent profiles subagents can start from and unusable profile files as TOML (docs/agent-profiles.md) | Delegation/task/project orchestration and scoped steering/control commands |
| messenger | messenger list/add/set/remove/enable/disable/allow/disallow/allow-sender/disallow-sender/connect/disconnect: entries are added disabled with no chat allowed; kinds are telegram, matrix, discord, slack, irc, signal and beeper (beeper's credential is the Beeper Desktop API token and its endpoint defaults to Beeper Desktop on the gateway's computer; matrix needs --endpoint, its homeserver; slack needs --app-credential, its app-level token; irc needs --endpoint ircs://host[:port] and --login, its services account, and folds channel and account names as the network does; signal needs --endpoint, its signal-cli daemon's unix:// socket or loopback tcp:// port, and --login, the account's +number or ACI, and takes no credential); the token is a vault credential name; edits go through the revision-checked settings API (docs/messengers.md); the GUI Messengers page uses the same settings and commands | More services |
| cron | schedule list/add/pause/resume/run/remove: add THREAD --name with --text or --heartbeat (send the project's HEARTBEAT.md; HEARTBEAT_OK replies are quiet) and one of --at, --in, --every [--anchor] or --cron [--zone] (docs/schedules.md) | Agent-requested jobs |
| doctor | doctor [--toml]: non-mutating checks of the profile, gateway, vault broker, pairing and provider credentials/catalogs; exits non-zero when something must be fixed; never starts the gateway or broker | Explicit reviewed fixes (--repair) |

Other current commands include Joshua start/stop/status, backend configure/select,
downloads, Git status, document reads, durable events, `activity [--before SEQ]
[--limit N]` (the gateway's notable events newest first as a TOML page, without
conversation or download progress notices), ACP/MCP, web and render.
Use `agent206 --help` and subcommand help for the actual supported tree.

Status/reconnect must not launch a missing or deliberately stopped gateway.
Explicit start/restart can resume it. Stop/restart wait for bounded cleanup and
verified acknowledgement; never kill a guessed PID. Local stop also persists for
an offline profile. Failed cleanup restores work admission while leaving already
cancelled work cancelled. Repeated start preserves the running connection.
Scripts must distinguish a
running process from completed security enrollment. Missing remote capabilities,
unknown metadata or offline workspace counts must be reported honestly.

RustyClaw aliases should be added only when semantics match. Never translate its
WebSocket endpoint flags into an unverified SSH connection, import static curated
model defaults, activate foreign hooks/plugins, or claim unsupported setup/auth
commands work. Test actual CLI workflows and policy denials with the real binary.

`agent206 [--data-dir PATH] vault channel-key` prints the current broker's public
identity key for the web Security page. Verify it through a trusted terminal
before entering a vault password in the browser. It is public and changes when
the broker restarts. See [web vault setup](web-vault-setup.md).
