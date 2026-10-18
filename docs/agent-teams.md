# Agent teams

An agent team is a coordinator and the members it hands tasks to, each an
[agent profile](agent-profiles.md). It is Agent 206's counterpart to
RustyClaw's swarms (`swarm/*`, `tools/swarm_tools.rs`, `rustyclaw swarm`),
rewritten around delegation: RustyClaw materializes every member as a
registered agent with its own sessions and routes messages to them by label,
while here a team is started on a conversation the user chose, and its members
are ordinary delegated conversations under the same checks as any subagent.

## Files

Each team is one TOML file in the gateway profile's `agent-teams` directory,
named `<id>.toml`, with the same ID rules as profiles.

```toml
name = "Research"                          # required, up to 80 bytes
description = "Finds and summarizes sources"   # optional, up to 400 bytes
coordinator = "lead"                       # required: an agent profile ID
members = ["reader", "writer"]             # required: 1–16 agent profile IDs
instructions = """
Cite every source with its URL.
"""                                        # optional, up to 16 KiB, shared by all
```

Unknown keys are refused, as are a coordinator listed among the members and a
member listed twice. Files follow the profile directory's limits (regular files
of at most 32 KiB, at most 64 read in name order) and are in protected gateway
state: agents cannot create or edit teams. A file that cannot be used is listed
with its reason beside the teams that can.

## Starting a team

`StartTeam` (the conversation details' Agent team control, or
`agent206 teams start TEAM --parent ID`) creates one subagent of the chosen
conversation, the coordinator:

- It is created as `CreateProfileSubagent` creates one from the coordinator
  profile: that profile's mode and model must match, its model must be one the
  conversation permits for subagents, and its grant is the conversation's grant
  ∩ the requested capabilities ∩ the profile's capabilities ∩ the mode's
  ceiling. The desktop, web, TUI and CLI request the conversation's own
  capabilities, for these checks to narrow.
- Every member profile is read when the team starts, and the team is refused
  if any cannot be used.
- The coordinator may delegate to the conversation's subagent models and starts
  with the conversation's tool permissions, including the subagent tools'.
  Effective permissions remain the minimum along the delegation chain, so
  this never permits more than the conversation has.
- A conversation that already belongs to a team cannot start another.

The coordinator records the team as it was (ID, name, file SHA-256, shared
instructions, each member's profile ID, name and description); editing the file
changes later teams only.

## Coordinating

The coordinator's prompt adds, after its profile instructions, the team's
shared instructions and its members with their descriptions. It hands out
tasks with `subagent_spawn`, which for a coordinator offers only the member
profiles and requires one; it follows them with `subagent_wait`,
`subagent_status`, `subagent_send` and `subagent_cancel`, as any delegating
run does. The gateway refuses a coordinator subagent, from a client as well as
from the tools, that does not start from a member profile.

A member's prompt adds the team's shared instructions and its coordinator's
name. Members start with the coordinator's tool permissions except the subagent
tools, as every tool-started subagent does, so they do not delegate further.

Stopping the coordinator's run (Stop, `agent206 cancel`, the Running work page)
stops its members' runs; pausing it holds theirs. The Running work page and
conversation details show the team as the coordinator's subagent tree, and
details name each conversation's team and role.

## Differences from RustyClaw

- No built-in templates: RustyClaw's eight-role template ships prompts for
  services Agent 206 does not have, and a team here is only as useful as the
  user's own profiles. Write the profiles and a team file instead.
- No handoff flow: the user talks to any member directly by opening its
  conversation, and flows between members are not modeled; members do not
  delegate to each other.
- No separate send/status/stop/delete tools: the coordinator's subagent tools,
  the Running work page and ordinary conversation commands cover them.

## Not yet

Editing teams from the GUI or CLI, importing RustyClaw swarms, and a team
member reaching another member.

Code: `crates/core/src/teams.rs` (types, the shared `start` request),
`crates/runtime/src/agent_teams.rs` (reading and checks),
`crates/runtime/src/gateway/profiles.rs`, `crates/runtime/src/delegation_tool.rs`,
`crates/ui/src/teams.rs`, `crates/app/src/teams_cli.rs`. Tests:
`agent_teams/tests.rs` (parsing, limits, member checks, directory listing),
`crates/runtime/tests/tool_loop.rs`
(`a_team_coordinator_delegates_only_to_its_member_profiles`),
`core/src/teams/tests.rs`, `ui/src/teams/tests.rs` and `app/src/teams_cli.rs`.
