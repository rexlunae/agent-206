# Reviewed migration during setup

Import is a required setup feature. This contract separates source discovery,
review and application so migration cannot silently grant authority or activate
foreign code. RustyClaw settings/vault migration and the scoped Claude Code
connection adapter below are implemented; other source adapters remain planned.

## Claude Code settings adapter

Select **Claude Code** on Import and choose an absolute directory containing
`settings.json` on the gateway host. The CLI uses
`agent206 import review-claude-code /absolute/settings-directory`, followed by
`import apply-review ID --provider 0` when the reviewed item is available. Both
use the same isolated source reader, held review, selected apply, source/target
checks and private receipt as RustyClaw settings. Preview changes nothing.

This adapter imports only an explicit `env.ANTHROPIC_BASE_URL` connection, using
Anthropic Messages. The SDK adds `/v1/messages` under that base, so the imported
Agent 206 endpoint adds `/v1` to preserve the request prefix. Invalid or
credential-bearing URLs are omitted and unavailable. A cloud-provider or
host-managed routing switch makes the item unavailable, even when the switch
is false or empty: the adapter does not reconstruct shell/host precedence.

The selected `settings.json` is one explicit scope. It does not merge
`settings.local.json`, managed settings, shell variables or another project's
settings, and never reads authentication files/keychains. Missing an explicit
base URL adds no default or subscription account. Credentials and credential
helpers, model choices, other environment values, permissions, hooks, plugins,
MCP configuration and sandbox settings remain inactive; their contents do not
cross the reader boundary. The connection has no credential reference and
requires live API discovery before model selection. Instructions, sessions,
history and other tool data are not imported by this adapter.

Format evidence inspected 2026-10-18: [Claude Code settings scopes](https://code.claude.com/docs/en/settings),
[environment variables](https://code.claude.com/docs/en/env-vars), and the
[official Anthropic TypeScript SDK message path](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/src/resources/messages/messages.ts).
Tests cover generated private/model strings staying behind, route refusal,
duplicates, malformed JSON diagnostics, source bounds and symlinks, plus actual
isolated gateway/CLI review and guarded apply. Release web acceptance with a
disposable source verified the source selector, blank/empty-selection gating,
source-change refusal, cleared selections after a fresh preview and selected
application with a private receipt. Source private/model text was absent from
the DOM, the source stayed unchanged and no vault was created. Native desktop
and TUI interactive acceptance remains pending.

## RustyClaw adapter

`agent206 import rustyclaw [PATH]` (default `~/.rustyclaw`, reference
`481a428c`) reads `config.toml`, the vault under `credentials/` (or the older
root `secrets.json`) and the presence of workspace files, through
`a206_import::rustyclaw::preview`. Reads are bounded (1 MiB configuration), refuse
symlinked files and keep only the fields mapped below; values that may hold
secrets or commands are counted, never stored, so they cannot reach the preview.

| Source | Result |
| --- | --- |
| `[model]` provider and `[[custom_providers]]` | Provider account from the matching preset (endpoint from the source, else the preset), added only when no account has the same API and endpoint. The credential name RustyClaw used (for example `OPENAI_API_KEY`) is referenced and shown in the preview with the origin it must be bound to. The broker spends it only for that exact origin: an untyped or unbound entry is refused until the user types and binds it (`vault adopt … --destination`), and a credential the user already bound to that origin is used as bound. Endpoints the gateway would refuse (not HTTPS or loopback HTTP, or carrying credentials, a query or a fragment) are skipped. The old default model is shown as a note and never selected. Copilot device-flow and unknown providers are reported and skipped |
| `credentials/secrets.json`, password-protected | `--apply` uses the broker's held source review, asks for its password and enrolled TOTP code, and imports only the checked bytes on a private copy when this profile has no vault. Security and `vault import FILE [--apply]` use the same operations. Policies, kinds, disabled flags, agent access and TOTP enrollment are preserved; the source is never written. Key-file vaults remain unsupported |
| `[[messengers]]` | Telegram, Matrix, Discord, Slack and IRC entries become disabled messengers with no credential, keeping `allowed_chats` and `allowed_users` as their allow-lists. A Matrix entry keeps its `homeserver` as the endpoint (an HTTPS, or loopback HTTP, origin); one without a usable homeserver is reported and skipped, and its `user_id` is not needed because the gateway asks the homeserver which account the token belongs to. An IRC entry keeps `server` and `port` as an `ircs://` endpoint, `nick` (RustyClaw's default `RustyClaw` when unset) as the services login and `irc_channels` among its allowed chats, with channel and user names folded as the network compares them; only entries with `use_tls` import, since Agent 206 never sends a password over plain text to a remote server. RustyClaw's IRC `allowed_users` were nicknames and are now matched against services accounts, which are usually the same names but stricter: someone using the nickname without signing in to that account is no longer accepted. Agent 206 additionally refuses group messages from senders who are not listed, so no restriction is widened. Tokens (Slack's `app_token` included) and passwords are never read; the preview says when one stays behind and how to store it in the vault. Signal entries are reported and skipped with instructions: RustyClaw ran signal-cli itself, while Agent 206 connects to a signal-cli daemon the user runs, which the source does not name. `webhook` entries, and `teams` and `google_chat` entries that post through a `webhook_url` rather than a Bot Framework app (`app_id`, `app_password`) or a service account (`credentials_path`), become disabled send-only webhook messengers in the `text` format (Teams: `teams`); the URL is itself the credential, so only its origin is kept, as the endpoint, and the preview says to store the URL in the vault bound to it. A webhook URL that is not HTTPS or loopback HTTP, or carries a user name or password, is reported and skipped. Other WhatsApp, iMessage and Google Chat entries are reported and skipped with a pointer to connecting that network to Beeper Desktop and adding a Beeper messenger. Entries for other services, with a separate `config_path` or with chat ids the gateway would refuse are reported and skipped. Names are reduced to lowercase letters, digits and hyphens and made unique; an entry whose name a messenger of the same kind already has counts as already imported |
| MCP servers, tool permissions, disabled tool groups, services, engines, sandbox, skills, plaintext registry token | Counted and left inactive. Denies in tool permissions cannot be represented yet, so nothing is granted in their place |
| SOUL.md/AGENTS.md, conversations, memory, messenger system prompt | Reported as not imported yet |
| `totp_enabled`, `agent_access` configuration flags | Ignored: enrollment and agent access come from the encrypted vault |

Applying writes an owner-only receipt (`imports/<id>.toml`) with the source
digest, accounts and messengers added and whether the vault was copied. This is
provenance for later review; automatic rollback is not available. A repeated import adds nothing. Import only attaches to a running
gateway and never starts one, so a preview changes nothing.

The legacy `import rustyclaw --apply` command retains these limitations:

- Selection is per category (`--skip-vault`, `--skip-providers`,
  `--skip-messengers`), not per item.
- Apply is ordered, not atomic: the vault is copied before accounts are saved,
  and a failure after the copy is reported as that partial result.
- There is no rollback command; the receipt is the record to undo by hand.

### Shared settings review

The Import page (`/settings/import`, linked from Get started and the sidebar)
previews a RustyClaw installation on the **gateway host**. Desktop, web and TUI
share this page and the `PreviewImport`/`ApplyImport` command API. The CLI exposes
the same flow with `import review-rustyclaw /absolute/source` (TOML review) and
`import apply-review <review-id> --provider <index> --messenger <index>`; indices
are zero-based positions in the corresponding reviewed arrays and each option
can be repeated. Neither operation starts a stopped gateway.

Nothing is selected initially. Users select individual addable providers and
messengers; existing and unsupported entries cannot be selected. The page shows
endpoints and messenger restrictions, plus inactive/unsupported categories.
Settings reviews omit free-form source provider IDs, labels, model text and
credential references inside the isolated reader, before its first reply.
Labels come from trusted target presets or a generated item number; source
messenger display names and unknown service text are omitted too. Renaming
custom connections and assigning their vault references is available through
Edit connection on Connections, or the `settings --apply` CLI. Both use the
same revision-checked settings command; neither operation grants vault access.
Review schema v2 is checked before GUI/CLI presentation; unversioned development
replies are refused with an update action. Rejected URLs are omitted
from previews because they can contain inline passwords or signed tokens.
Providers still need live API discovery; messengers are saved disabled and
without credentials. Vault migration is a separate action on Security or through
`agent206 vault import FILE --apply`; settings selection never copies or unlocks
a vault. Claude Code supports the limited connection adapter above; other tool
adapters remain planned.

Before any reader reply (including the legacy terminal flow), the remaining
preview strings are checked for known credential shapes. A match refuses the
whole preview with a static, correlated error rather than changing an endpoint,
login or policy restriction. This scanner is defense in depth and cannot identify
arbitrary unknown secrets in data required for migration. Optional free-form
fields are therefore omitted structurally from settings reviews, not merely
pattern-redacted. The legacy trusted-terminal flow retains legitimate source
labels, provenance and symbolic vault references for its explicit vault migration.

The gateway holds at most four reviews in memory, with at most 200 providers
and 200 messengers per review. A restart, eviction or attempted apply requires
a new preview. Apply takes only the review ID and selected indices; it never
accepts client-supplied configurations. It rereads the isolated source and
compares the complete preview (including the configuration hash), checks the
target revision again through the shared settings-save path, and imports only
the held selected data. A source edit after this reread cannot inject new data
because application does not reopen source files. Changed targets/sources,
missing reviews and invalid selections refuse the import. Agent tools do not
expose either command.

Settings saving and receipt writing are ordered, not a multi-file transaction.
A receipt failure returns an explicit `ImportApplied` partial result containing
the added identities and structured failure; the UI displays both, and the CLI
prints the receipt data and exits unsuccessfully. The ordinary gateway request
journal deduplicates application retries. Source/target guards, selected items,
inactive authority, disabled messenger restrictions, replay and receipt failures
are tested against the actual isolated reader and gateway; the CLI is tested
through the real binary. Rollback remains planned.

### Reviewed vault migration

`a206_vault::Vault::review_import` reads a regular, bounded source file without
decrypting it or creating a target, returning only its byte count and SHA-256.
Source opening on Unix uses no-follow/nonblocking flags and validates the opened
handle, so a final symlink or FIFO cannot bypass the regular-file check. Source
imports fail explicitly on platforms without this safe opening implementation.
`import_reviewed` rereads the source once and checks that identity before any
staging or authentication. It stages the same checked buffer, so an edit after
the check cannot change the imported vault. Each TOTP retry checks the source
again. A changed source leaves the target absent and locked; existing targets
remain protected. Password, enrolled TOTP, replay protection and policy checks
still run on the private copy. The identity is not evidence of a valid vault.

Security and Get started offer **Import a RustyClaw vault** while the target has
no vault. Select an absolute encrypted **file** on the broker host, preview its
path, byte count and source SHA-256, then enter its password and any enrolled
authenticator code. The native CLI offers `vault import FILE` for read-only
preview and `vault import FILE --apply` for a fresh preview and authenticated
copy, without starting a gateway. `import rustyclaw --apply` uses the same
reviewed broker operation for its vault category.

Broker protocol v11 carries schema-v1 `ReviewVaultImport`, `ApplyVaultImport`
and `CancelVaultImport` on both native and authenticated encrypted setup
channels. The broker holds one review per private connection. Apply accepts
only its opaque ID and authentication, never a supplied digest or replacement
configuration. New previews replace old reviews; cancellation, locking,
disconnect, source change and successful copy invalidate them. TOTP challenges
and authentication retries keep the review but recheck source bytes. Old peers
are refused; no client falls back to unreviewed import. The old `ImportVault`
opcode remains for older trusted terminal clients.

The source's policies, disabled flags, agent-access setting and enrolled factor
remain authoritative. No task/tool grants or provider accounts are imported.
TOTP verification advances replay state only in the private target copy; the
source stays unchanged. Inputs clear on submission, and passwords are not held
for retries. An interrupted request may already have committed: check vault
status rather than assuming cancellation rolled it back.

After copy, the broker writes an owner-only, atomically published and synced
TOML receipt under `vault/imports/<review-id>.toml`. Its source identity and time
contain no credentials. Copy and receipt are ordered operations: a write/sync
failure returns a confirmed `VaultImportApplied` with a structured
`receipt_error`. The GUI keeps this beside the actual vault and pairing states;
the CLI reports the copied vault and exits unsuccessfully before further import
steps. A receipt failure never replaces or deletes the imported vault.

These controls operate on this computer's broker for desktop/TUI, or the web
server's local broker after trusted-key verification. Remote SSH vault controls,
key-file migration, rollback and other import adapters remain pending. Native
desktop/TUI interactive acceptance remains unverified.

### Isolated source reader

The CLI launches `a206_runtime::imports::preview`: a fresh copy of the current
executable under Seatbelt on macOS or bubblewrap plus Landlock on Linux. The
reader gets only the selected source directory read-only, private scratch,
system libraries and its own executable. It has no network, other executable
grant or inherited environment. Missing isolation fails the preview. Source
directory symlinks are refused; a canonical source path fixes the granted root.
Paths configured outside that source are not granted implicitly. This may make
an external credentials/workspace location unavailable; it must be selected
separately when a future adapter supports additional sources.

One versioned, bounded MessagePack request/result carries duplicate-matching
account identities/endpoints, messenger names/kinds and preset defaults. User
prompts, MCP/backend commands, grants and credential handles are not forwarded.
The reader returns the existing reviewed preview; only the broker can import
credentials. Parser errors retain their byte range but discard source excerpts
and semantic messages that could echo plaintext credentials. Failures keep their
structured cause chain and request correlation. Oversized replies return a
bounded protocol error. A deadline or caller cancellation kills the reader.

The `import_reader` Cargo test runs the real reader and probes its source write,
unrelated-file, executable and network denials. Unit tests cover protocol version,
truncated/oversized frames, oversized results and target-data minimization.
This is the prerequisite for the shared GUI preview/apply flow, not that UI.

## Sources and evidence

| Source | Intended data | Primary source / format gate |
| --- | --- | --- |
| RustyClaw | Settings, conversations/projects, prompts, encrypted vault and all policy metadata | Pinned source baseline in security-setup.md; configuration, sessions and credentials readers must be versioned |
| Cline | Task history, connection settings, instructions | [Task documentation](https://docs.cline.bot/core-workflows/task-management); inspect source-version storage schemas before implementing native task readers |
| Claude Code | Explicit settings.json Anthropic endpoint (implemented); instruction files and session transcripts (planned) | [Settings](https://code.claude.com/docs/en/settings), [environment](https://code.claude.com/docs/en/env-vars); no implicit scope merging or imported authority |
| Codex | TOML configuration, instructions, session history | Local upstream source under ignored .reference/codex; pin the inspected revision and rollout schemas when implementing |
| DeepSeek Harness | Supported configuration/history/export format | [Official harness](https://deepseek.com/en/harness/), [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness); pin source/storage schemas before implementing, independently of the DeepSeek provider |
| Other common tools | Exported conversations, connection settings and instructions | Add versioned adapters for documented formats; evaluate OpenClaw, Aider, Continue, Cursor and Windsurf individually |

Historical model identifiers are provenance, not a current model catalog. Validate
any requested provider/model binding against a live provider API before enabling
it. Never import source limits, prices, capabilities or balances as evergreen facts.

DeepSeek Harness source inspected at
`86a402200d95fecef4529d969836f1fe13c6cb0d` (MIT). Its boot layer reads optional
user patches from `~/.dsh` and loads `cordis.yml`. Treat YAML expression/plugin
configuration as data; never evaluate it or load packages during import.
Its [session types](https://github.com/deepseek-ai/deepseek-harness/blob/86a402200d95fecef4529d969836f1fe13c6cb0d/packages/core/session/src/types.ts)
use logical format 4, versioned headers and ordered events. The
[storage contract](https://github.com/deepseek-ai/deepseek-harness/blob/86a402200d95fecef4529d969836f1fe13c6cb0d/packages/session/session-persistence/src/storage-contract.ts)
refuses unknown required events and mismatched identities. The importer must
preserve that refusal, validate sequence/lineage, and use the versioned migration
schemas for older sources instead of treating every JSONL file as chat messages.
This is source/schema research, not a shipped import adapter.

## Flow and boundaries

1. User selects a source/tool and an explicit path or export. Discovery is opt-in
   and shows paths/categories, never secret values. No automatic home-directory
   sweep, source runtime execution, keychain extraction or cloud login migration.
2. An isolated reader validates the source schema/version and file bounds. Reject
   traversal, escaping symlinks, oversized input and unexpected executable content.
   Treat foreign messages and instruction files as untrusted imported content.
3. Produce a versioned preview with provenance, counts, mapping, unsupported fields,
   conflicts and security consequences. Show exactly which categories will change.
   Secrets remain in an encrypted broker-to-broker path; plaintext credential files
   never pass through ordinary gateway requests, previews, logs or agent context.
4. User selects categories/items and resolves conflicts. Default to additive import;
   never overwrite existing accounts, threads, prompts or vault entries silently.
5. Apply atomically with durable receipts, content/source identity deduplication and
   a rollback record. Repeated imports must not duplicate conversations or execute
   side effects. Do not change or delete the original installation.
6. Report imported, skipped, unsupported and unresolved items honestly.

TOML is the portable Agent 206 import manifest/export configuration format.
Application-owned preview/apply messages use MessagePack. JSON readers exist only
for the external tools' actual standard/native formats.

## Security-preserving mapping

- Imported conversations are inert history. Tool calls, commands, approvals and
  agent output never execute on import. New threads receive no active grants and
  no implicit model/backend fallback.
- Imported hooks, MCP servers, shell commands, plugins, skills and auto-approval
  settings remain inactive pending separate user review. Do not install or launch
  a foreign runtime to parse its files.
- Preserve restrictive denies and organizational policy where representable.
  Unsupported or ambiguous constraints remain locked, with an explanation;
  never drop a restriction merely because Agent 206 uses a different policy model.
- RustyClaw vault migration retains disabled state, every policy variant,
  skill/trigger scope, credential kind, origin restrictions and enrollment state.
  Unknown skill/trigger identities grant nothing. An imported Always policy still
  cannot override the new user's global agent switch or per-task grants.
- Existing enrolled TOTP cannot be disabled by replacing settings with defaults.
  Legacy password/key-file access and any enrolled factor must be verified before
  sensitive migration; cancellation leaves the source intact and target locked.
- Credential/OAuth migration requires a supported destination-bound broker
  operation and explicit selected entries. Unsupported subscription/session tokens
  stay unimported; a source account does not confer authority in Agent 206.
- Do not import source-model costs/limits or activate API keys through ordinary
  provider configuration. Bind references only after live catalog validation.

Tests must generate model facts through live mock APIs, use independent
versioned source samples, and cover malformed input, source changes between
preview/apply, duplicate imports, conflicts, rollback, policy attenuation,
credential redaction, unsupported versions and source immutability.
