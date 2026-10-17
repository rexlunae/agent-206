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
