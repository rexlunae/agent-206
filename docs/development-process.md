# Development process

Agent 206's coding conversations follow the process of
[Literate AI](https://github.com/jordanhubbard/literate-ai): a project's intent
and progress live in files rather than in conversation, work is recorded before
it is implemented, a checklist box is checked only with its evidence, and changes
land through review.

## What the agent is told

Native Plan, Code and Review runs that may read their project (`ReadWorkspace`,
the same condition as [project instruction files](run-engine.md#project-instruction-files))
get a "Development process" section after the project's instruction files. The
texts are in `crates/runtime/src/process.toml`; which one applies depends on the
project:

| Project has | Process |
| --- | --- |
| `literate.project.json` | Full process, plus: specifications, Flavors and pinned skills are authority; follow the project's `SKILL.md`; prefer `litai` commands (`project guidance`, `verify`, `work record`/`close`, `land`) where commands may run; ask before compiling or running generated code |
| `PROJECT.md` or `docs/roadmap/active-work.md` | Full process |
| neither | Lighter form; the agent may offer to start a queue but does not create one unasked |

The full process for Code mode, in brief:

1. Read `PROJECT.md` (goals and completeness) and the open queue items.
2. Record a work item before editing: owner, direction, conclusion and scope,
   dependencies, implementation subtasks and the evidence that must pass.
3. Survey peer work (open pull requests, branches, worktrees) before starting.
4. Change intent where it lives; add discovered follow-ups as subtasks.
5. Verify locally; check a box only with its durable result.
6. Close the item only when every box is checked; add the outcome under the
   changelog's Unreleased section; commit queue, change and changelog together.
7. Land through a pull request, wait for required CI, never force-push.
8. Turn reusable lessons into an update of the narrowest owning rule.

Plan runs express their plan as items ready to record. Review runs check a change
against its item: work without an item, scope beyond its conclusion, boxes checked
without a result, and a missing changelog entry. Chat and Research runs get no
process section. `PROJECT.md` is also read as an instruction file, after
`AGENTS.md` and `CLAUDE.md`.

External coding backends (Codex, Claude Agent) run their own loops and do not get
this section or the work queue tools.

## Work queue tools

The bundled `agent206.worklog` extension (`plugins/worklog`) keeps
`docs/roadmap/active-work.md` and `CHANGELOG.md` in the format Literate AI's
`litai work record` and `litai work close` use, so either tool can keep the same
queue:

```markdown
### [ ] API-012 — Title

- **Priority:** P1
- **Owner:** the narrowest owning authority
- **Direction:** what the user asked for
- **Conclusion:** the decided scope
- **Depends on:** none
- **Implementation:**
  - [ ] a concrete change
- **Evidence:**
  - [ ] the proof that must pass
```

| Tool | Grant | Does |
| --- | --- | --- |
| `work_list` | read | Open items with checklist progress and the next unchecked task; one item in full by `id` |
| `work_record` | read + write | Adds an item, creating the queue if needed. `id` must match `^[A-Z][A-Z0-9-]{2,63}$` and be unused; `area` allocates the next `AREA-NNN`. Goes under the queue's `## <priority>` section when there is one |
| `work_add` | read + write | Adds an unchecked subtask or evidence box; reopens a closed item |
| `work_check` | read + write | Checks box `number` of a checklist, appending ` — result`; an evidence box needs a result. The item's heading becomes `[~]` |
| `work_close` | read + write | Marks the item `[x]`; refused while any box is unchecked |
| `changelog_add` | read + write | Adds an entry under `## Unreleased` (and an optional `### Category`), matching the section's spacing; creates the section or file if needed. `path` may name a component's own `CHANGELOG.md` |

Like every extension, the tools do nothing until the user grants the extension
and permits each tool in a conversation. Each change reads the file and writes
it back with `edit_file` over the whole old text, so a file edited meanwhile is
refused with `conflict` rather than overwritten; for the same reason an existing
but empty queue or changelog is refused until it has a heading. Files over 256 KiB
are refused; archive closed items first. Headings and checkboxes inside fenced code
blocks are ignored, with fences closed as CommonMark closes them.

Not implemented: release-goal manifests and `Release target` reconciliation,
detailed-roadmap lifecycle headers, tracker reconciliation (`litai project tracker
inspect`), and the process for external coding backends.
