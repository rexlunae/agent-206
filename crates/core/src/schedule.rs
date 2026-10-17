//! Scheduled messages: text the gateway sends to a conversation at set times,
//! as RustyClaw's cron jobs do. A job is only a timed `Send`; it runs with the
//! conversation's grants and permissions as they are when it fires.
use crate::{ErrorReport, Id};
use serde::{Deserialize, Serialize};

/// When a job is due.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Schedule {
    /// Once, at a Unix time in seconds.
    At { at: u64 },
    /// Every `seconds`, at `anchor + n × seconds` (Unix seconds), so a late
    /// run does not shift later ones.
    Every { seconds: u64, anchor: u64 },
    /// A cron expression (five fields, or six with seconds first) evaluated
    /// in an IANA time zone, or in the gateway's own zone when `zone` is unset.
    Cron {
        expression: String,
        zone: Option<String>,
    },
}

/// Who created a job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobAuthor {
    #[default]
    User,
    /// A model in this conversation, through a call the user approved.
    Agent { thread: Id },
}

/// How a job's latest run went.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    Running,
    Complete,
    /// The run failed; the conversation shows why.
    Failed,
    Cancelled,
    /// The gateway did not run the message, for example because the
    /// conversation was busy or had no model.
    Refused(ErrorReport),
}

/// One run of a job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobRun {
    /// Identifies the run, so the result of an earlier run that ends late
    /// never replaces a newer one started in the same second.
    #[serde(default)]
    pub id: Id,
    /// Unix seconds.
    pub at: u64,
    pub outcome: JobOutcome,
}
impl JobRun {
    /// A run starting at `at`.
    pub fn start(at: u64) -> Self {
        Self {
            id: Id::new_v4(),
            at,
            outcome: JobOutcome::Running,
        }
    }
}

/// A message the gateway sends to `thread` on `schedule`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: Id,
    pub name: String,
    pub thread: Id,
    pub text: String,
    pub schedule: Schedule,
    pub enabled: bool,
    /// Unix seconds.
    pub created: u64,
    /// When it is next due, in Unix seconds; `None` when it will not run
    /// again (a finished one-time job) or is disabled.
    pub next: Option<u64>,
    pub last: Option<JobRun>,
    #[serde(default)]
    pub author: JobAuthor,
}

/// Why a typed schedule time or interval was refused.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("“{text}” is not an RFC 3339 time such as 2026-10-18T09:00:00Z")]
    Time {
        text: String,
        #[source]
        source: jiff::Error,
    },
    #[error("times before 1970 are not supported")]
    BeforeEpoch,
    #[error("“{text}” is not a duration such as 90m, 1h 30m or 1d")]
    Interval {
        text: String,
        #[source]
        source: jiff::Error,
    },
    #[error("intervals are whole seconds")]
    FractionalInterval,
    #[error("the interval must be positive")]
    NonPositiveInterval,
}

/// Unix seconds of an RFC 3339 time.
pub fn parse_time(text: &str) -> Result<u64, InputError> {
    let time: jiff::Timestamp = text.trim().parse().map_err(|source| InputError::Time {
        text: text.to_owned(),
        source,
    })?;
    u64::try_from(time.as_second()).map_err(|_| InputError::BeforeEpoch)
}

/// Whole seconds of a duration such as `90m`, `1h 30m` or `PT2H`.
pub fn parse_interval(text: &str) -> Result<u64, InputError> {
    let interval: jiff::SignedDuration =
        text.trim().parse().map_err(|source| InputError::Interval {
            text: text.to_owned(),
            source,
        })?;
    if interval.subsec_nanos() != 0 {
        return Err(InputError::FractionalInterval);
    }
    u64::try_from(interval.as_secs())
        .ok()
        .filter(|&seconds| seconds > 0)
        .ok_or(InputError::NonPositiveInterval)
}

/// An RFC 3339 UTC time for Unix seconds, for showing a schedule.
pub fn format_time(seconds: u64) -> Option<String> {
    let seconds = i64::try_from(seconds).ok()?;
    jiff::Timestamp::from_second(seconds)
        .ok()
        .map(|time| time.to_string())
}

/// A whole-second interval in the short form [`parse_interval`] reads back,
/// such as `1h 30m`.
pub fn format_interval(seconds: u64) -> Option<String> {
    let seconds = i64::try_from(seconds).ok()?;
    Some(format!("{:#}", jiff::SignedDuration::from_secs(seconds)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_times_and_intervals_parse_or_say_why_not() {
        let seconds = u64::from(crate::Id::new_v4().as_fields().1) * 60 + 60;
        let shown = format_time(seconds).unwrap();
        assert_eq!(parse_time(&format!(" {shown} ")).unwrap(), seconds);
        assert!(matches!(
            parse_time("1969-12-31T23:59:59Z"),
            Err(InputError::BeforeEpoch)
        ));
        let typo = crate::Id::new_v4().to_string();
        let error = parse_time(&typo).unwrap_err();
        assert!(matches!(&error, InputError::Time { text, .. } if *text == typo));
        assert!(std::error::Error::source(&error).is_some());

        assert_eq!(parse_interval(&format!("{seconds}s")).unwrap(), seconds);
        assert_eq!(parse_interval("1h 30m").unwrap(), 5_400);
        assert_eq!(format_interval(5_400).as_deref(), Some("1h 30m"));
        let shown = format_interval(seconds).unwrap();
        assert_eq!(parse_interval(&shown).unwrap(), seconds);
        assert!(matches!(
            parse_interval("1.5s"),
            Err(InputError::FractionalInterval)
        ));
        assert!(matches!(
            parse_interval("0s"),
            Err(InputError::NonPositiveInterval)
        ));
        assert!(matches!(
            parse_interval("-5m"),
            Err(InputError::NonPositiveInterval)
        ));
        assert!(matches!(
            parse_interval(&typo),
            Err(InputError::Interval { .. })
        ));
    }
}
