# Skills

A skill is a folder of instructions for one kind of task, in the AgentSkills /
OpenClaw layout RustyClaw uses: `<name>/SKILL.md` with YAML frontmatter, then
Markdown, plus optional supporting files. Skills are not extensions: they carry
no code Agent 206 runs and they never add permissions. A model following a skill
still works within its thread's grants.

Status: **experimental**. Listing, review and approval work in the CLI and the
desktop/TUI Extensions page. Native runs reach approved skills through the
`skill_list` and `skill_info` tools.

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

Installed skills start **off**. Approval names the text the user reviewed:
`agent206 skills show NAME` prints `SKILL.md` and its SHA-256 digest, and
`agent206 skills enable NAME --digest HEX` approves it; on the Extensions page,
**Review** opens the text and **Approve this text and enable** approves what is
shown. If the file changed after that review, approval is refused. The digest is
recorded in `settings.toml` (`enabled_skills`); saving settings never changes
it, so approval stays a separate user operation. Any later edit makes the skill
"changed": it is withheld from models until the user reviews and enables it
again. A skill that fails to parse is listed with its error, never silently
dropped. Installing or editing a skill changes files only, so the page's
**Refresh** re-reads the folder.

Bounds: 256 skill folders, 256 KiB per `SKILL.md`, 1 MiB per supporting file.
Supporting files are read only by relative path inside the skill; absolute paths,
`..` and symlinks are refused, and files are opened without following symlinks
at any step, so a link swapped in during a read cannot reach outside the skill.
A program requirement is met only by an executable file on `PATH`.

CLI: `agent206 skills list | show NAME | enable NAME --digest HEX | disable NAME`.

## Model access

Native runs get two tools, named as in RustyClaw, from
`crates/runtime/src/skill_tool.rs`:

- `skill_list` returns the `<available_skills>` block: names and descriptions of
  approved, unchanged skills, escaped as author text.
- `skill_info` takes `name` and an optional `path`, and returns the skill's
  Markdown body or one UTF-8 supporting file.

Both are offered only while at least one approved skill exists. They declare no
capability, since a skill adds instructions and never authority, and like every
tool they stay denied in a conversation until the user permits them there. Each
call re-reads the file and checks it against its approval, so an edit withdraws
it at once. A result longer than the tool output limit (64 KiB) is refused
rather than cut, because a cut skill would be followed as if it were whole.

The gateway methods `skill_index`, `skill_offers`, `skill_instructions` and
`skill_file` share the same checks. Failures are typed: `Disabled` and `Changed`
are permission denials; malformed skills and arguments are invalid requests.

## Not yet implemented

- ClawHub registry search/install, with pre-install audit.
- Agent-prepared skills (`skill_create`): a model drafts, the user approves.
- Skill-linked vault credentials (`SkillOnly` policy).
- Project-scoped skill folders.
