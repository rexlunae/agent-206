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
    /// Once, `seconds` after the gateway accepts the job. The gateway stores
    /// it as `At`, measured by its own clock, so a client's clock never moves it.
    In { seconds: u64 },
    /// Every `seconds`, at `anchor + n × seconds` (Unix seconds), so a late
    /// run does not shift later ones. With no anchor the gateway anchors it
    /// when it accepts the job, by its own clock, so the first run comes one
    /// interval later.
    Every {
        seconds: u64,
        #[serde(default)]
        anchor: Option<u64>,
    },
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

/// What a job sends when it runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// The job's own text.
    #[default]
    Message,
    /// A heartbeat check, as RustyClaw's: the checklist in `HEARTBEAT.md` at
    /// the conversation's project root, read when the job runs, with the
    /// request to reply exactly [`HEARTBEAT_OK`] when nothing needs attention.
    /// A missing file refuses the run; an empty one sends nothing.
    Heartbeat,
}

/// The whole reply to a heartbeat check when nothing needs attention.
pub const HEARTBEAT_OK: &str = "HEARTBEAT_OK";
/// How every heartbeat check message begins, followed by its run's ID.
pub const HEARTBEAT_HEADER: &str = "Scheduled heartbeat check ";

/// The message run `run` of a heartbeat check sends, around the
/// `HEARTBEAT.md` checklist. The run's ID identifies the message after a
/// restart.
pub fn heartbeat_message(checklist: &str, run: Id) -> String {
    format!(
        "{HEARTBEAT_HEADER}{run}. Work through this checklist from HEARTBEAT.md now. Do not repeat tasks from earlier conversations that it does not list. If nothing needs attention, reply with exactly {HEARTBEAT_OK} and nothing else; otherwise reply with what needs attention and leave {HEARTBEAT_OK} out.\n\n{}",
        checklist.trim()
    )
}

/// The run that sent `text`, when it is a heartbeat check message.
pub fn heartbeat_run(text: &str) -> Option<Id> {
    let rest = text.strip_prefix(HEARTBEAT_HEADER)?;
    let (run, _) = rest.split_once('.')?;
    run.parse().ok()
}

/// Whether a reply to a heartbeat check says nothing needs attention.
pub fn is_heartbeat_ok(reply: &str) -> bool {
    reply.trim() == HEARTBEAT_OK
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
    /// A heartbeat check found nothing that needs attention: the reply was
    /// exactly [`HEARTBEAT_OK`], or `HEARTBEAT.md` was empty and nothing was sent.
    Quiet,
    /// The gateway did not run the message, for example because the
    /// conversation was busy or had no model.
    Refused(ErrorReport),
    /// The gateway stopped before it saw how this run ended. Only an earlier
    /// run in a job's history ends this way; restart recovery settles the
    /// latest run from its conversation.
    Unrecorded,
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
    /// When the run ended, in Unix seconds; `None` while it runs, and for a
    /// run whose end the gateway did not see (it was stopped meanwhile).
    #[serde(default)]
    pub ended: Option<u64>,
    /// Whether the run's reply was posted to the job's
    /// [`Job::deliver_to`]; `None` when there was nothing to post.
    #[serde(default)]
    pub delivery: Option<Delivery>,
}
impl JobRun {
    /// A run starting at `at`.
    pub fn start(at: u64) -> Self {
        Self {
            id: Id::new_v4(),
            at,
            outcome: JobOutcome::Running,
            ended: None,
            delivery: None,
        }
    }
}

/// How posting a run's reply to a messenger went. Only a run that completes
/// with a reply posts it; a heartbeat check posts only when something needs
/// attention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Delivery {
    /// The run's reply is being posted.
    Posting,
    /// Posted, in this many parts.
    Sent { parts: u64 },
    /// Not posted, or only partly: the destination was withdrawn or
    /// disconnected, or the service refused it.
    Failed(ErrorReport),
    /// The gateway stopped while the reply was being posted, so whether it
    /// was posted, or only partly, is not known.
    Unrecorded,
}

/// Most runs of one job the gateway keeps in its history.
pub const MAX_JOB_RUNS: usize = 50;

/// A message the gateway sends to `thread` on `schedule`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: Id,
    pub name: String,
    pub thread: Id,
    /// The message; empty for a heartbeat check, whose message is built
    /// from `HEARTBEAT.md` when it runs.
    pub text: String,
    #[serde(default)]
    pub kind: JobKind,
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
    /// Changes when an earlier run, no longer `last`, ends in the run
    /// history, so a client showing the history reloads it. `last` covers
    /// every other change.
    #[serde(default)]
    pub history: u64,
    /// A messenger destination each run's reply is also posted to, as
    /// [`crate::messengers::post_destinations`] names it; set only by the
    /// user, with [`crate::Command::SetJobDelivery`].
    #[serde(default)]
    pub deliver_to: Option<String>,
}

/// Why a time or interval someone typed for a schedule cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ScheduleInputError {
    #[error("`{input}` is not an RFC 3339 time such as 2026-10-18T09:00:00Z")]
    Time {
        input: String,
        #[source]
        cause: jiff::Error,
    },
    #[error("`{input}` is not a date and time")]
    Local {
        input: String,
        #[source]
        cause: jiff::Error,
    },
    #[error("unknown time zone `{zone}`")]
    Zone {
        zone: String,
        #[source]
        cause: jiff::Error,
    },
    #[error("`{input}` is not a duration such as 90m or 1d")]
    Duration {
        input: String,
        #[source]
        cause: jiff::Error,
    },
    #[error("intervals are whole seconds; `{0}` is not")]
    Fraction(String),
    #[error("times are whole seconds, so a message never arrives early; `{0}` is not")]
    FractionalTime(String),
    #[error("the interval must be positive")]
    NotPositive,
    #[error("times before 1970 are not supported")]
    BeforeEpoch,
}

fn unix(time: jiff::Timestamp) -> Result<u64, ScheduleInputError> {
    u64::try_from(time.as_second()).map_err(|_| ScheduleInputError::BeforeEpoch)
}

/// An RFC 3339 time, in Unix seconds. A fraction of a second is refused
/// rather than cut, which would make the message early.
pub fn parse_instant(input: &str) -> Result<u64, ScheduleInputError> {
    let time =
        input
            .trim()
            .parse::<jiff::Timestamp>()
            .map_err(|cause| ScheduleInputError::Time {
                input: input.to_owned(),
                cause,
            })?;
    if time.subsec_nanosecond() != 0 {
        return Err(ScheduleInputError::FractionalTime(input.to_owned()));
    }
    unix(time)
}

/// A wall-clock date (`2026-10-18`) and time (`09:00`) in an IANA time zone,
/// in Unix seconds. A time a clock change skips moves forward by the gap
/// (02:30 becomes 03:30), and one it repeats is the later of the two.
pub fn parse_local(date: &str, time: &str, zone: &str) -> Result<u64, ScheduleInputError> {
    let input = format!("{}T{}", date.trim(), time.trim());
    let civil =
        input
            .parse::<jiff::civil::DateTime>()
            .map_err(|cause| ScheduleInputError::Local {
                input: input.clone(),
                cause,
            })?;
    let zone = jiff::tz::TimeZone::get(zone.trim()).map_err(|cause| ScheduleInputError::Zone {
        zone: zone.to_owned(),
        cause,
    })?;
    let zoned =
        zone.to_ambiguous_zoned(civil)
            .later()
            .map_err(|cause| ScheduleInputError::Local {
                input: input.clone(),
                cause,
            })?;
    if zoned.timestamp().subsec_nanosecond() != 0 {
        return Err(ScheduleInputError::FractionalTime(input));
    }
    unix(zoned.timestamp())
}

/// A positive whole-second interval such as `90m`, `1d` or `1h 30m`. Days
/// and weeks are 24 and 168 hours; months and years have no fixed length and
/// are refused.
pub fn parse_interval(input: &str) -> Result<u64, ScheduleInputError> {
    let refused = |cause| ScheduleInputError::Duration {
        input: input.to_owned(),
        cause,
    };
    let interval = input
        .trim()
        .parse::<jiff::Span>()
        .and_then(|span| span.to_duration(jiff::SpanRelativeTo::days_are_24_hours()))
        .map_err(refused)?;
    if interval.subsec_nanos() != 0 {
        return Err(ScheduleInputError::Fraction(input.to_owned()));
    }
    u64::try_from(interval.as_secs())
        .ok()
        .filter(|seconds| *seconds > 0)
        .ok_or(ScheduleInputError::NotPositive)
}

/// An interval in whole seconds as people write it, such as `1h 30m`.
pub fn format_interval(seconds: u64) -> String {
    match i64::try_from(seconds) {
        Ok(seconds) => format!("{:#}", jiff::SignedDuration::from_secs(seconds)),
        Err(_) => format!("{seconds}s"),
    }
}

#[cfg(test)]
mod tests;
