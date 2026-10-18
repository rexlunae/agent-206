# Agent profiles

Profile-based delegation uses the distinct `CreateProfileSubagent` command.
Older gateways refuse it; they cannot silently ignore a profile's policy as an
unknown optional field. Ordinary `CreateSubagent` remains wire compatible.

An agent profile is a named, reusable preset for subagents: a mode, a model, the
most capabilities to give and instructions added to the subagent's prompt. It is
Agent 206's counterpart to RustyClaw's named agent registry (`agents.rs`,
`agents_create`), rewritten around delegation: RustyClaw gives each named agent
its own workspace and sessions, while here a profile only shapes a subagent
conversation delegated from one the user configured.

## Files

Each profile is one TOML file in the gateway profile's `agent-profiles`
directory, named `<id>.toml`. The ID is 1–64 lowercase letters, digits, `-` or
`_`.

```toml
name = "Reviewer"                       # required, up to 80 bytes
description = "Reads a diff for bugs"   # optional, up to 400 bytes
mode = "review"                         # optional: chat, plan, code, review, research
model = "…"                             # optional: the name the provider's API lists
connection = "Work account"             # optional: the connection label, when several list `model`
capabilities = ["read_workspace", "git_read"]   # optional: the most it may hold
instructions = """
Report each bug with the file and line. Do not edit files.
"""                                     # optional, up to 16 KiB
```

Unknown keys, capabilities or modes are refused rather than ignored. Files are
read only if they are regular files of at most 32 KiB; at most 64 are read, in
name order. A file that cannot be used is listed with its reason beside the
profiles that can, so one mistake does not hide the rest. The directory is in
the gateway's protected state, which agents' tools cannot write: agents cannot
create or edit profiles.

The model is written by the user, never shipped: no model name is built in.
It stays unbound until used, and is then matched only against the models the
user permitted for the delegating conversation's subagents.

## What a profile can and cannot do

A profile only narrows. Starting a subagent from one (`CreateProfileSubagent`,
used by the desktop/web/TUI and by `subagent_spawn`):

- The subagent's mode must be the profile's `mode`, when it names one.
- Its model must be the profile's `model` (and `connection`), when it names one,
  and must still be one of the parent's subagent models. A profile naming a
  model the user did not permit is refused; nothing falls back to another model.
- Its grant is the parent's grant ∩ the requested capabilities ∩ the profile's
  `capabilities` ∩ the mode's ceiling.
- Its instructions are added to the subagent's prompt after the mode prompt,
  under `## Agent profile: <name>`, for native and external backends alike.

A profile never adds a model, a capability, a tool permission or a subagent
model. The subagent records the profile as it was when created (ID, name,
SHA-256 of the file, instructions), so editing the file changes later subagents
only. The gateway re-reads the file for every `CreateSubagent`, so what it
enforces is the file as it is then.

## Using profiles

- `subagent_spawn` takes an optional `profile`. The usable profiles are offered
  in its schema with their names and descriptions; with a profile, `mode` and
  `model` default to the profile's. An explicit `mode` or `model` that differs
  from the profile's is refused. If the profile directory cannot be read, the
  tools are still offered without profiles and the listing reports why.
- Conversation details (desktop, web, TUI) offer an agent profile choice before
  creating a delegated conversation, and show the profile a subagent started
  from.
- The Modes settings page lists every profile and every unusable file.
- `agent206 profiles` prints the same listing as TOML.
- Agent teams start a coordinator and members from profiles
  ([agent-teams.md](agent-teams.md)).

## Not yet

Editing profiles from the GUI or CLI (write the TOML files directly), importing
RustyClaw agents, and per-profile tool permissions.

Code: `crates/core/src/profiles.rs` (types), `crates/runtime/src/agent_profiles.rs`
(reading and checks), `crates/runtime/src/gateway/profiles.rs`,
`crates/runtime/src/delegation_tool.rs`, `crates/ui/src/profiles.rs`,
`crates/app/src/profiles_cli.rs`. Tests: `agent_profiles/tests.rs` (parsing,
limits, narrowing, model matching, directory listing),
`crates/runtime/tests/tool_loop.rs`
(`a_subagent_started_from_a_profile_takes_its_mode_model_and_instructions`),
`ui/src/profiles/tests.rs` and `app/src/profiles_cli.rs`.
