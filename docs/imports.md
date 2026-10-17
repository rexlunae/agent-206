# Reviewed migration during setup

Import is a required setup feature. This contract separates source discovery,
review and application so migration cannot silently grant authority or activate
foreign code. The RustyClaw adapter is implemented (below); the others are not.

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
| `credentials/secrets.json`, password-protected | `--apply` asks for its password (and TOTP code when enrolled) and sends `ImportVault` to the broker, which copies the encrypted file, opens only the copy, verifies both factors there and links it in only if this profile has no vault. Policies, kinds, disabled flags, agent access and TOTP enrollment travel inside the vault unchanged; the source is never written. Key-file vaults are reported as unsupported |
| `[[messengers]]` | Telegram entries become disabled messengers with no credential and their `allowed_chats` as the allow-list. When the source also had `allowed_users`, no chat is allowed, since Agent 206 cannot check users and would otherwise widen the restriction. Tokens and passwords are never read; the preview says when one stays behind and how to store it in the vault. Entries for other services, with a separate `config_path` or with chat ids the gateway would refuse are reported and skipped. Names are reduced to lowercase letters, digits and hyphens and made unique; an entry whose name a Telegram messenger already has counts as already imported |
| MCP servers, tool permissions, disabled tool groups, services, engines, sandbox, skills, plaintext registry token | Counted and left inactive. Denies in tool permissions cannot be represented yet, so nothing is granted in their place |
| SOUL.md/AGENTS.md, conversations, memory, messenger system prompt | Reported as not imported yet |
| `totp_enabled`, `agent_access` configuration flags | Ignored: enrollment and agent access come from the encrypted vault |

Applying writes an owner-only receipt (`imports/<id>.toml`) with the source
digest, accounts and messengers added and whether the vault was copied, which is the rollback
record. A repeated import adds nothing. Import only attaches to a running
gateway and never starts one, so a preview changes nothing.

Remaining gates of the flow below, not yet met by this adapter:

- Readers run in the CLI process, not the isolated reader process.
- Selection is per category (`--skip-vault`, `--skip-providers`,
  `--skip-messengers`), not per item.
- Apply is ordered, not atomic: the vault is copied before accounts are saved,
  and a failure after the copy is reported as that partial result.
- There is no rollback command; the receipt is the record to undo by hand.

## Sources and evidence

| Source | Intended data | Primary source / format gate |
| --- | --- | --- |
| RustyClaw | Settings, conversations/projects, prompts, encrypted vault and all policy metadata | Pinned source baseline in security-setup.md; configuration, sessions and credentials readers must be versioned |
| Cline | Task history, connection settings, instructions | [Task documentation](https://docs.cline.bot/core-workflows/task-management); inspect source-version storage schemas before implementing native task readers |
| Claude Code | JSON settings, instruction files, session transcripts | [Settings](https://code.claude.com/docs/en/settings), [JSON/stream output and session transcripts](https://code.claude.com/docs/en/headless); retain scope/precedence and transcript provenance |
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
