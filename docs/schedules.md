# Scheduled messages

A scheduled message (a job) is text the gateway sends to one conversation at set
times, as RustyClaw's cron jobs do. A job is only a timed `Send`: it is admitted,
journaled and run like a message the user typed, under the conversation's grants,
mode, models, tool permissions and budget as they are when it fires. Narrowing any
of those applies to the next run; a job never carries authority of its own.

## Heartbeat checks

A job with `kind: Heartbeat` is RustyClaw's heartbeat, run by the same
scheduler. When it runs, the gateway reads `HEARTBEAT.md` at the conversation's
project root (at most 32 KiB, through the same confined reader as the project
instruction files, and only if the conversation's grants include reading the
project at that moment) and sends it as one message asking the model to work
through the checklist and reply with exactly `HEARTBEAT_OK` when nothing needs
attention. The checklist is read only there: instruction files never include it.

- No project, no read grant, or no `HEARTBEAT.md`: the run is `Refused` with
  the reason (`NotFound` or `PermissionDenied`) and nothing is sent.
- An empty `HEARTBEAT.md`: the run is `Quiet` and nothing is sent, so a
  checklist can be paused by emptying it.
- A reply of exactly `HEARTBEAT_OK` (surrounding whitespace aside): `Quiet`.
  Any other reply is `Complete` and shown as needing attention; the reply is in
  the conversation.

Heartbeat jobs take no text (`text` must be empty). The conversation keeps the
check and its reply like any other turn; compaction keeps long-running ones
small. RustyClaw's unwired `subconscious.rs` decider loop is not ported: a
heartbeat is an ordinary run with the conversation's model, grants and budget.

## Schedules

| Schedule | Meaning |
| --- | --- |
| `At { at }` | Once, at a Unix time in seconds. A time already passed is refused. After it runs the job is disabled and kept with its result. |
| `In { seconds }` | Once, 1 second to ten years after the gateway accepts the job. Stored as `At`. |
| `Every { seconds, anchor }` | At `anchor + n × seconds`, 60 seconds to ten years apart. A late run does not shift later ones. With no `anchor`, the gateway anchors it when it accepts the job, so it first runs one interval later. |
| `Cron { expression, zone }` | A cron expression (five fields, or six with seconds first; [croner](https://docs.rs/croner) syntax) in an IANA time zone, or the gateway's own zone when unset. |

`In` and an unanchored `Every` are measured by the gateway's clock, so a client
whose clock differs never moves them; the Schedules page sends intervals
unanchored. Intervals are written like `90m`, `1h 30m` or `1d`: days and weeks
are 24 and 168 hours, and months and years, which have no fixed length, are
refused.

If the gateway stops while a job's run is starting, the next start settles it:
when the conversation received the message (for a heartbeat, a check message
sent after the run started), the run ends as the conversation
shows (its current run's state while that message is the latest, otherwise
complete if a reply followed it); when it did not, the job is due again at once, so the message is sent
exactly once.

When the gateway was stopped through one or more due times, the job runs once
when it starts and then continues from the next time after now. Pausing clears
the next time; resuming computes it from now, so times passed while paused are
skipped. A schedule that stops resolving (for example a time zone removed from
the system) pauses the job and records why.

## Commands

- `ScheduleJob { name, thread, text, schedule, kind }` adds an enabled job
  (`Reply::Job`). Names are 1–200 bytes, message texts 1–128 KiB, at most 256
  jobs; `kind` is `Message` (the default) or `Heartbeat`, which takes no text.
- `SetJobEnabled { id, enabled }` pauses or resumes it.
- `RunJob { id }` runs it now and leaves its schedule as it is.
- `RemoveJob { id }` deletes it; its conversation keeps what it sent.

Jobs are in `Snapshot::jobs`, and each change publishes `JobChanged { id }`.
`Job::last` records the latest run, with its own ID so a late result of an
earlier run never replaces a newer one: `Running`, `Complete`, `Failed` (the
conversation shows the failure), `Cancelled`, `Quiet` (a heartbeat check that
found nothing needing attention) or `Refused` with the gateway's
report when the message was not run, for example because the conversation was
busy or had no model.

CLI: `agent206 schedule list`, `schedule add THREAD --name N --text T` (or
`--heartbeat` instead of `--text`) with one
of `--at RFC3339`, `--in 90m`, `--every 90m [--anchor RFC3339]` or `--cron EXPR [--zone Z]`,
and `schedule pause|resume|run|remove ID`. Times and intervals are parsed by
`a206_core::schedule::{parse_instant, parse_interval}`, shared with the page below.

## Schedules page

Desktop, TUI and web share the Schedules page (`/schedules`, `crates/ui/src/schedules.rs`).
It lists every job, soonest first, with its conversation, what its schedule means,
who added it, its next time and its latest run (a refusal shows the gateway's
report; a heartbeat check shows whether anything needed attention), and offers Pause/Resume, Run now and Remove. Its form sends `ScheduleJob`
with a message or a heartbeat check:
once at a date and time in an IANA zone (prefilled with the client's own zone;
`parse_local` moves a time a clock change skips forward by the gap and takes the
later of a repeated one), on an interval counted from now, or on a cron
expression with an optional zone. Times are shown in the client's zone. The web
build bundles the time zone database, since browsers have no zone files. Tested
by rendering the page and the form's schedule construction; not yet clicked
through in a running app.

## Scheduling from a native run

`crates/runtime/src/schedule_tool.rs` offers a model three tools, denied on each
conversation until the user permits them:

- `schedule_add { name, message, at | every_seconds | cron [, zone] }` schedules
  a message to the model's own conversation (`JobAuthor::Agent`). Every call
  waits for the user's approval of that exact call, even where the conversation
  allows the tool. A conversation keeps at most 16 jobs its models added. `at`
  must be a whole second, so a message never arrives before the time approved.
- `schedule_list` lists the conversation's jobs, the user's included, marked
  `added_by: user` or `you`. Results show the first 512 bytes of each message
  with its full `message_bytes`. Jobs are listed oldest first; a listing stops at 48
  KiB with `next_after`, a cursor over that order which `schedule_list { after }`
  continues from, so every listing is complete JSON below the output cap and
  every job is reachable. Removing jobs between pages never hides one; a job
  added while paging may sort before the cursor (creation times are whole
  seconds), so a new listing from the start is the one to trust after adding. Only the listing is
  marked whole output: adding and removing commit before they return, so their
  small results are never turned into a failure for lack of room.
- `schedule_remove { id }` removes a job a model of this conversation added; the
  user's jobs and other conversations' jobs are not found.

The tools reach no other conversation, cannot run, pause or resume a job, and
hold the gateway without its tool hosts. Tests: `schedule_tool/tests.rs` and an
approval round trip in `crates/runtime/tests/schedules.rs`.

## Implementation

`crates/runtime/src/schedule.rs` computes due times as pure functions;
`gateway/schedules.rs` holds the commands and `run_due_jobs(now)`, which the
gateway daemon's scheduler calls when the next job is due, when a job changes and
at least hourly. Tests: `schedule/tests.rs` (at, interval grid, cron with seconds
and zones), `gateway/schedules/tests.rs` (validation, one run per wake, pause,
run now) and `crates/runtime/tests/schedules.rs` (a due job's native run against a
scripted provider).

Not yet: delivery of results to a
messenger chat, run history beyond the latest run, and jitter for many jobs due
at once.
