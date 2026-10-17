# Scheduled messages

A scheduled message (a job) is text the gateway sends to one conversation at set
times, as RustyClaw's cron jobs do. A job is only a timed `Send`: it is admitted,
journaled and run like a message the user typed, under the conversation's grants,
mode, models, tool permissions and budget as they are when it fires. Narrowing any
of those applies to the next run; a job never carries authority of its own.

## Schedules

| Schedule | Meaning |
| --- | --- |
| `At { at }` | Once, at a Unix time in seconds. A time already passed is refused. After it runs the job is disabled and kept with its result. |
| `Every { seconds, anchor }` | At `anchor + n × seconds`, 60 seconds to ten years apart. A late run does not shift later ones. |
| `Cron { expression, zone }` | A cron expression (five fields, or six with seconds first; [croner](https://docs.rs/croner) syntax) in an IANA time zone, or the gateway's own zone when unset. |

If the gateway stops while a job's run is starting, the next start settles it:
when the conversation received the message, the run ends as the conversation
shows (its current run's state while that message is the latest, otherwise
complete if a reply followed it); when it did not, the job is due again at once, so the message is sent
exactly once.

When the gateway was stopped through one or more due times, the job runs once
when it starts and then continues from the next time after now. Pausing clears
the next time; resuming computes it from now, so times passed while paused are
skipped. A schedule that stops resolving (for example a time zone removed from
the system) pauses the job and records why.

## Commands

- `ScheduleJob { name, thread, text, schedule }` adds an enabled job
  (`Reply::Job`). Names are 1–200 bytes, texts 1–128 KiB, at most 256 jobs.
- `SetJobEnabled { id, enabled }` pauses or resumes it.
- `RunJob { id }` runs it now and leaves its schedule as it is.
- `RemoveJob { id }` deletes it; its conversation keeps what it sent.

Jobs are in `Snapshot::jobs`, and each change publishes `JobChanged { id }`.
`Job::last` records the latest run, with its own ID so a late result of an
earlier run never replaces a newer one: `Running`, `Complete`, `Failed` (the
conversation shows the failure), `Cancelled` or `Refused` with the gateway's
report when the message was not run, for example because the conversation was
busy or had no model.

CLI: `agent206 schedule list`, `schedule add THREAD --name N --text T` with one
of `--at RFC3339`, `--every 90m [--anchor RFC3339]` or `--cron EXPR [--zone Z]`,
and `schedule pause|resume|run|remove ID`.

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

Not yet: a desktop/TUI page, delivery of results to a
messenger chat, run history beyond the latest run, and jitter for many jobs due
at once.
