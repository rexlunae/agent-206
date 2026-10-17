//! When scheduled messages are due: pure functions of a schedule and a time,
//! so the scheduler's decisions are tested without a clock.
use a206_core::{ErrorKind, Schedule};
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

#[cfg(test)]
mod tests;
