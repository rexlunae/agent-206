# CLI contract and RustyClaw compatibility

Desktop is primary; the CLI is a supported interface for users, SSH and automation,
not an internal debugging path. Both use the same typed commands, lifecycle
controller and security/authentication rules. New application-owned output and
configuration use TOML; owned wire APIs use bounded MessagePack. Keep diagnostics
on stderr, output on stdout, meaningful exit codes and no secrets in argv/env/logs.

Current headless build: `cargo build -p agent206 --no-default-features`.
Use `--data-dir` for an isolated profile and `--remote ssh://user@host` for an
already-installed remote gateway. With no subcommand, the app launches the desktop; `tui`
(alias `ui`) explicitly launches the shared terminal UI when built with that feature.

| RustyClaw command family | Agent 206 now | Required follow-up |
| --- | --- | --- |
| desktop / tui | desktop, tui / ui | Terminal interaction and graphics acceptance |
| status | status: process and live workspace counts; gateway status: process only | Health checks/doctor, richer API-sourced usage, profile selection UX |
| gateway start / stop / restart / status | Same nested commands, plus reconnect; stop-gateway compatibility entry point | Verified installers/updaters and real SSH acceptance |
| setup / onboard / configure | Not implemented | Same encrypted enrollment/TOTP/security state machine as the GUI; no weaker headless path |
| config get / set / unset | settings exports TOML; settings --apply FILE uses revision-checked shared API | Reviewed field-level commands and parity aliases |
| command / ask / chat | new TITLE, chat THREAD TEXT, cancel THREAD | Streaming, interactive/headless sessions, stdin/file prompts and clear compatibility options |
| model / providers | providers presets/add, refresh-models; API-only catalogs | Model/account inspection and selection commands with no static model defaults |
| secrets | vault init/unlock/lock/status/stop, list, add, adopt (untyped legacy entries), policy, bind, enable/disable, delete, agent-access, reveal (step-up), totp enroll/remove, all through the vault broker | SSH key, form and payment entry; recovery codes; key-file import; remote broker over SSH; GUI parity |
| import | Not implemented | RustyClaw, Cline, Claude Code, Codex, DeepSeek Harness and extensible adapters in imports.md |
| skills / registries | extension ID executes the bounded bundled WASM tool | Reviewed install/update/list/export and separate instruction/skill management |
| swarm / projects | project NAME ROOT creates a coordinator; snapshots retain linked threads | Delegation/task/project orchestration and scoped steering/control commands |
| doctor | Not implemented | Non-mutating diagnostics by default; explicit reviewed fixes |

Other current commands include Joshua start/stop/status, backend configure/select,
downloads, Git status, document reads, durable events, ACP/MCP, web and render.
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
