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
| setup / onboard / configure | setup [--local \| --ssh URL [--trust FP]] [--skip-totp]: chooses the gateway when asked, then on a local profile creates or unlocks the vault, offers TOTP enrollment and pairs a running gateway through the same broker requests as the GUI, skipping finished steps, and prints the doctor report. A closed stdin declines every step. Remote profiles are told to run setup on the gateway host | Provider step, desktop/TUI wizard, onboard/configure aliases only if semantics match |
| config get / set / unset | settings exports TOML; settings --apply FILE uses revision-checked shared API | Reviewed field-level commands and parity aliases |
| command / ask / chat | new TITLE, chat THREAD TEXT, cancel THREAD; search QUERY [--limit N] prints the messages matching it in every conversation as TOML hits; tools list/permit/budget THREAD and approvals list/approve/deny and prompts list/choose/answer/decline for the native tool loop (answer from a second terminal while chat waits) | Streaming, interactive/headless sessions with inline approval prompts, stdin/file prompts and clear compatibility options |
| model / providers | providers presets/add, refresh-models; API-only catalogs | Model/account inspection and selection commands with no static model defaults |
| secrets | vault init/unlock/lock/status/stop, pair (gateway pairing for this unlock), list, add, adopt (untyped legacy entries, optionally renamed with `--as`), policy, bind, enable/disable, delete, agent-access, reveal (step-up), totp enroll/remove, all through the vault broker | SSH key, form and payment entry; recovery codes; key-file import; remote broker over SSH; GUI parity |
| import | import rustyclaw [PATH] previews (text or --toml) and changes nothing; --apply copies a password vault in through the broker (asks its password and TOTP code), adds missing provider accounts and disabled messenger entries (tokens stay behind), writes imports/<id>.toml; --skip-vault / --skip-providers / --skip-messengers | Cline, Claude Code, Codex, DeepSeek Harness and extensible adapters in imports.md; GUI import screen |
| mcp | mcp-servers list/enable/disable/connect/disconnect; entries added through settings --apply (docs/mcp.md) | Field-level add/edit commands; HTTP servers |
| skills / registries | skills list/show/enable --digest/disable (approval pinned to the reviewed content, docs/skills.md); extension list, grant ID [--capability C…], enable/disable ID, panel ID PANEL [--project P] [--press BUTTON], tool ID TOOL [--project P] [--args TOML], all through the gateway's extension policy | Reviewed install/update/list/export |
| swarm / projects | project NAME ROOT creates a coordinator; snapshots retain linked threads | Delegation/task/project orchestration and scoped steering/control commands |
| messenger | messenger list/add/set/remove/enable/disable/allow/disallow/allow-sender/disallow-sender/connect/disconnect: entries are added disabled with no chat allowed; kinds are telegram and matrix (matrix needs --endpoint, its homeserver); the token is a vault credential name; edits go through the revision-checked settings API (docs/messengers.md); the GUI Messengers page uses the same settings and commands | More services |
| doctor | doctor [--toml]: non-mutating checks of the profile, gateway, vault broker, pairing and provider credentials/catalogs; exits non-zero when something must be fixed; never starts the gateway or broker | Explicit reviewed fixes (--repair) |

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
