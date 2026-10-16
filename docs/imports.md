# Reviewed migration during setup

Import is a required setup feature; adapters are not implemented yet. This
contract separates source discovery, review and application so migration cannot
silently grant authority or activate foreign code.

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
