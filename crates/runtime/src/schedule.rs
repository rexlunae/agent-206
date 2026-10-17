//! When scheduled messages are due: pure functions of a schedule and a time,
//! so the scheduler's decisions are tested without a clock.
use a206_core::{ErrorKind, ErrorReport, Job, JobOutcome, Role, RunState, Schedule, Thread};
use jiff::{Timestamp, tz::TimeZone};
use std::str::FromStr;

/// Shortest interval between runs of a repeating job.
pub const MIN_INTERVAL: u64 = 60;
/// Longest interval between runs of a repeating job: ten years.
pub const MAX_INTERVAL: u64 = 10 * 366 * 86_400;

/// Why a schedule is unusable.
#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("that time has already passed")]
    Past,
    #[error("intervals must be {MIN_INTERVAL} seconds to ten years")]
    Interval,
    #[error("the cron expression is invalid")]
    Expression(#[source] croner::errors::CronError),
    #[error("unknown time zone `{zone}`")]
    Zone {
        zone: String,
        #[source]
        cause: jiff::Error,
    },
    #[error("the time is outside the supported range")]
    Range(#[source] jiff::Error),
    #[error("the schedule never comes due again")]
    Never,
}
impl crate::error::Classified for ScheduleError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(ErrorKind::InvalidRequest)
    }
}

/// The first time strictly after `after` (Unix seconds) that `schedule` is
/// due, or `None` if it never is again.
pub fn next_after(schedule: &Schedule, after: u64) -> Result<Option<u64>, ScheduleError> {
    match schedule {
        Schedule::At { at } => Ok((*at > after).then_some(*at)),
        Schedule::Every { seconds, anchor } => {
            if !(MIN_INTERVAL..=MAX_INTERVAL).contains(seconds) {
                return Err(ScheduleError::Interval);
            }
            if after < *anchor {
                return Ok(Some(*anchor));
            }
            let steps = (after - anchor) / seconds + 1;
            Ok(steps
                .checked_mul(*seconds)
                .and_then(|offset| anchor.checked_add(offset)))
        }
        Schedule::Cron { expression, zone } => {
            let cron = croner::Cron::from_str(expression).map_err(ScheduleError::Expression)?;
            let zone = match zone {
                Some(name) => TimeZone::get(name).map_err(|cause| ScheduleError::Zone {
                    zone: name.clone(),
                    cause,
                })?,
                None => TimeZone::system(),
            };
            let after = i64::try_from(after).unwrap_or(i64::MAX);
            let start = Timestamp::from_second(after)
                .map_err(ScheduleError::Range)?
                .to_zoned(zone);
            match cron.find_next_occurrence(&start, false) {
                Ok(next) => Ok(u64::try_from(next.timestamp().as_second()).ok()),
                // No match within croner's search horizon: never again.
                Err(croner::errors::CronError::TimeSearchLimitExceeded) => Ok(None),
                Err(error) => Err(ScheduleError::Expression(error)),
            }
        }
    }
}

/// The first time a new or resumed job is due, which must exist.
pub fn first(schedule: &Schedule, now: u64) -> Result<u64, ScheduleError> {
    next_after(schedule, now)?.ok_or(match schedule {
        Schedule::At { .. } => ScheduleError::Past,
        _ => ScheduleError::Never,
    })
}

/// Settle the job runs a restart interrupted. Call it on the stored state before
/// interrupted conversation runs are marked failed. A run whose message the
/// conversation received ends as that conversation shows it did; a run that
/// never sent it is due again at once, so the message is sent exactly once.
pub fn recover(jobs: &mut [Job], threads: &[Thread]) {
    for job in jobs {
        let Some(run) = job
            .last
            .as_mut()
            .filter(|run| run.outcome == JobOutcome::Running)
        else {
            continue;
        };
        let thread = threads.iter().find(|thread| thread.id == job.thread);
        let messages = thread.map_or(&[][..], |thread| &thread.messages[..]);
        let sent = messages
            .iter()
            .position(|m| m.role == Role::User && m.at >= run.at && m.text == job.text);
        // The run's own outcome is the conversation's only while its message
        // is the latest; a later message started another run, so this one
        // ended earlier, with a reply or without one.
        let later = |sent: usize| {
            messages[sent + 1..]
                .iter()
                .position(|m| m.role == Role::User)
        };
        run.outcome = match (sent, thread.map(|thread| &thread.state)) {
            (Some(sent), state) => match (later(sent), state) {
                (None, Some(RunState::Complete)) => JobOutcome::Complete,
                (None, Some(RunState::Cancelled)) => JobOutcome::Cancelled,
                (None, _) => JobOutcome::Failed,
                (Some(next), _) => {
                    let replies = &messages[sent + 1..sent + 1 + next];
                    if replies.iter().any(|m| m.role == Role::Assistant) {
                        JobOutcome::Complete
                    } else {
                        JobOutcome::Failed
                    }
                }
            },
            _ => {
                // Due again at the interrupted time; the scheduler runs it
                // once and then continues on its schedule.
                job.enabled = true;
                job.next = Some(job.next.map_or(run.at, |next| next.min(run.at)));
                JobOutcome::Refused(ErrorReport::new(
                    ErrorKind::Unavailable,
                    "The gateway restarted before sending this message; it is sent again.",
                ))
            }
        };
    }
}

#[cfg(test)]
mod tests;
