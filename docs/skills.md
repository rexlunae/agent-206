# Skills

A skill is a folder of instructions for one kind of task, in the AgentSkills /
OpenClaw layout RustyClaw uses: `<name>/SKILL.md` with YAML frontmatter, then
Markdown, plus optional supporting files. Skills are not extensions: they carry
no code Agent 206 runs and they never add permissions. A model following a skill
still works within its thread's grants.

Status: **experimental**. Listing, review and approval work in the CLI and the
desktop/TUI Extensions page. The run engine that shows skills to models is not
merged yet.

## Installing

Copy a skill folder into the gateway profile's `skills` directory
(`~/.agent206/skills/<name>/SKILL.md` by default; on a remote gateway, its
profile). The folder name must equal the skill's `name`.

```markdown
---
name: release-notes
description: Draft release notes from merged changes.
metadata: {"openclaw": {"emoji": "📝", "requires": {"bins": ["git"]}}}
---

# Release notes
...
```

Recognized frontmatter: `name` (lowercase letters, digits, single hyphens, up
to 64), `description`, and OpenClaw `metadata` (JSON string or mapping) with
`emoji`, `homepage`, `os`, `requires.bins`, `requires.anyBins` and
`requires.env`. Unmet OS and program requirements are shown; they are not
enforced, since tools enforce their own grants. Required environment variables
are shown as expectations: Agent 206 tools never inherit the gateway's
environment, so credentials must come through the vault.

## Approval

Installed skills start **off**. Enabling a skill (`agent206 skills enable NAME`
or **Approve and enable**) records the SHA-256 of its `SKILL.md` in
`settings.toml` (`enabled_skills`). Any later edit makes the skill "changed": it
is withheld from models until the user reviews and enables it again. A skill that
fails to parse is listed with its error, never silently dropped.

Bounds: 256 skill folders, 256 KiB per `SKILL.md`, 1 MiB per supporting file.
Supporting files are read only by relative path inside the skill; absolute paths,
`..` and symlinks are refused.

CLI: `agent206 skills list | show NAME | enable NAME | disable NAME`.

## Run engine interface

Gateway methods (`crates/runtime/src/gateway/skills.rs`):

- `skill_index(thread)` returns the model-facing `<available_skills>` block for
  the system prompt (names and descriptions escaped); `skill_offers(thread)`
  returns the same list as data.
- `skill_instructions(thread, name)` returns the Markdown body, re-checking at
  read time that this exact version is approved.
- `skill_file(thread, name, path)` returns a bounded supporting file.

Failures are `GatewayError::Skill(SkillError)`: `Disabled` and `Changed` are
permission denials, malformed skills are invalid requests.

## Not yet implemented

- ClawHub registry search/install, with pre-install audit.
- Agent-prepared skills (`skill_create`): a model drafts, the user approves.
- Skill-linked vault credentials (`SkillOnly` policy).
- Project-scoped skill folders.
