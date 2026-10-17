//! Scheduled messages for the native run engine: a [`ToolHost`] that lets a
//! model schedule messages to its own conversation, as RustyClaw's `cron` tool
//! does.
//!
//! A job only sends text to the conversation later, under its grants and
//! permissions as they are then (`docs/schedules.md`), but it makes the
//! conversation run without the user typing. So, like every tool, these are
//! denied until the user permits them on the conversation, each
//! `schedule_add` call then still waits for the user's approval of that
//! exact call, and a conversation keeps at most
//! [`MAX_AGENT_JOBS`] jobs its models added. A model reaches only its own
//! conversation's jobs and removes only those a model added; the user's stay.
use crate::{
    gateway::{Jobs, MAX_AGENT_JOBS},
    provider::now,
    tools::{ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{ErrorKind, ErrorReport, Id, Job, JobAuthor, JobOutcome, Schedule, ToolPermission};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};

/// Schedules a message to this conversation.
pub const ADD: &str = "schedule_add";
/// Lists this conversation's scheduled messages.
pub const LIST: &str = "schedule_list";
/// Removes a scheduled message a model added.
pub const REMOVE: &str = "schedule_remove";
/// Longest message text a result shows; `message_bytes` gives the full size.
pub const MESSAGE_BYTES: usize = 512;
/// Most bytes of jobs one `schedule_list` returns; `next_skip` continues it.
pub const LIST_BYTES: usize = 48 * 1024;

/// Why a schedule tool call failed.
#[derive(Debug, thiserror::Error)]
pub enum ScheduleToolError {
    #[error("`{0}` is required")]
    Missing(&'static str),
    #[error("`{0}` has the wrong type")]
    Type(&'static str),
    #[error("unknown argument `{0}`")]
    Unknown(String),
    #[error("unknown schedule tool `{0}`")]
    UnknownTool(String),
    #[error("give exactly one of `at`, `every_seconds` or `cron`")]
    When,
    #[error("`zone` applies only to `cron`")]
    Zone,
    #[error("`at` is not an RFC 3339 time with an offset")]
    At(#[source] jiff::Error),
    #[error("`at` is before 1970")]
    Epoch,
    #[error("`at` must be a whole second, so the message never arrives early")]
    Fraction,
    #[error("`id` is not a scheduled message ID")]
    Id(#[source] uuid::Error),
}
impl crate::error::Classified for ScheduleToolError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::UnknownTool(_) => ErrorKind::NotFound,
            _ => ErrorKind::InvalidRequest,
        })
    }
}
fn fail(error: ScheduleToolError) -> ErrorReport {
    crate::error::report(&error)
}

/// Offers the schedule tools to native runs.
pub struct ScheduleHost {
    jobs: Jobs,
}
impl ScheduleHost {
    pub fn new(jobs: Jobs) -> Self {
        Self { jobs }
    }

    fn specs() -> Vec<ToolSpec> {
        let spec = |name: &str, description: &str, parameters: Value, permission| ToolSpec {
            name: name.into(),
            description: description.into(),
            parameters,
            capabilities: Default::default(),
            permission,
            // A listing is one JSON value bounded below the output cap, and
            // useless cut. Adding and removing commit before they return, so
            // their small results are never turned into a failure.
            whole_output: name == LIST,
        };
        vec![
            spec(
                ADD,
                "Schedule a message to this conversation: when it is due, `message` is sent here as if the user typed it and you run on it, with this conversation's permissions at that time. Give exactly one of `at` (once), `every_seconds` (repeating) or `cron`. The user approves each call.",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "A short name the user will see."},
                        "message": {"type": "string", "description": "The message to send; include everything you will need then, since it arrives as a new message."},
                        "at": {"type": "string", "description": "Once, at this RFC 3339 time with an offset, such as 2030-01-02T09:00:00Z."},
                        "every_seconds": {"type": "integer", "minimum": 60, "description": "Repeat this many seconds apart, starting now."},
                        "cron": {"type": "string", "description": "A cron expression: five fields, or six with seconds first."},
                        "zone": {"type": "string", "description": "The IANA time zone for `cron`, such as Europe/Paris; the gateway's own zone if omitted."}
                    },
                    "required": ["name", "message"],
                    "additionalProperties": false
                }),
                ToolPermission::Ask,
            ),
            spec(
                LIST,
                "List the messages scheduled to this conversation, by the user or by you. A long listing stops early with `next_skip`; call again with `skip` set to it for the rest.",
                json!({
                    "type": "object",
                    "properties": {
                        "skip": {"type": "integer", "minimum": 0, "description": "How many jobs to skip; `next_skip` from the previous listing."}
                    },
                    "additionalProperties": false
                }),
                ToolPermission::Allow,
            ),
            spec(
                REMOVE,
                "Remove a message you scheduled to this conversation. Messages the user scheduled cannot be removed here.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "The ID schedule_add or schedule_list returned."}
                    },
                    "required": ["id"],
                    "additionalProperties": false
                }),
                ToolPermission::Allow,
            ),
        ]
    }

    fn run(&self, thread: Id, request: &ToolRequest) -> Result<Value, ErrorReport> {
        let args = &request.arguments;
        match request.name.as_str() {
            ADD => {
                allow(
                    args,
                    &["name", "message", "at", "every_seconds", "cron", "zone"],
                )?;
                let name = text(args, "name")?;
                let message = text(args, "message")?;
                let job = self.jobs.add(thread, name, message, schedule(args)?)?;
                Ok(describe(&job, thread))
            }
            LIST => {
                allow(args, &["skip"])?;
                let skip = match optional(args, "skip") {
                    None => 0,
                    Some(skip) => skip
                        .as_u64()
                        .and_then(|skip| usize::try_from(skip).ok())
                        .ok_or_else(|| fail(ScheduleToolError::Type("skip")))?,
                };
                let jobs = self.jobs.list(thread)?;
                let mut scheduled = Vec::new();
                let mut bytes = 0;
                for job in jobs.iter().skip(skip) {
                    let entry = describe(job, thread);
                    bytes += entry.to_string().len() + 1;
                    if bytes > LIST_BYTES {
                        break;
                    }
                    scheduled.push(entry);
                }
                let next = skip.saturating_add(scheduled.len());
                Ok(json!({
                    "total": jobs.len(),
                    "next_skip": (next < jobs.len()).then_some(next),
                    "scheduled": scheduled,
                    "you_may_add": MAX_AGENT_JOBS.saturating_sub(
                        jobs.iter().filter(|job| job.author == JobAuthor::Agent { thread }).count()
                    ),
                }))
            }
            REMOVE => {
                allow(args, &["id"])?;
                let id = text(args, "id")?
                    .parse()
                    .map_err(|e| fail(ScheduleToolError::Id(e)))?;
                self.jobs.remove(thread, id)?;
                Ok(json!({ "removed": id }))
            }
            other => Err(fail(ScheduleToolError::UnknownTool(other.into()))),
        }
    }
}

fn allow(args: &Map<String, Value>, known: &[&str]) -> Result<(), ErrorReport> {
    match args.keys().find(|key| !known.contains(&key.as_str())) {
        Some(key) => Err(fail(ScheduleToolError::Unknown(key.clone()))),
        None => Ok(()),
    }
}
fn optional<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    args.get(name).filter(|value| !value.is_null())
}
fn text(args: &Map<String, Value>, name: &'static str) -> Result<String, ErrorReport> {
    optional(args, name)
        .ok_or_else(|| fail(ScheduleToolError::Missing(name)))?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| fail(ScheduleToolError::Type(name)))
}

/// The one schedule the arguments describe. The gateway validates its range.
fn schedule(args: &Map<String, Value>) -> Result<Schedule, ErrorReport> {
    let at = optional(args, "at");
    let every = optional(args, "every_seconds");
    let cron = optional(args, "cron");
    let zone = match optional(args, "zone") {
        None => None,
        Some(zone) => Some(
            zone.as_str()
                .ok_or_else(|| fail(ScheduleToolError::Type("zone")))?
                .to_owned(),
        ),
    };
    if zone.is_some() && cron.is_none() {
        return Err(fail(ScheduleToolError::Zone));
    }
    match (at, every, cron) {
        (Some(at), None, None) => {
            let at: jiff::Timestamp = at
                .as_str()
                .ok_or_else(|| fail(ScheduleToolError::Type("at")))?
                .parse()
                .map_err(|e| fail(ScheduleToolError::At(e)))?;
            if at.subsec_nanosecond() != 0 {
                return Err(fail(ScheduleToolError::Fraction));
            }
            let at = u64::try_from(at.as_second()).map_err(|_| fail(ScheduleToolError::Epoch))?;
            Ok(Schedule::At { at })
        }
        (None, Some(every), None) => Ok(Schedule::Every {
            seconds: every
                .as_u64()
                .ok_or_else(|| fail(ScheduleToolError::Type("every_seconds")))?,
            anchor: now(),
        }),
        (None, None, Some(cron)) => Ok(Schedule::Cron {
            expression: cron
                .as_str()
                .ok_or_else(|| fail(ScheduleToolError::Type("cron")))?
                .to_owned(),
            zone,
        }),
        _ => Err(fail(ScheduleToolError::When)),
    }
}

/// A job as the model sees it, with times in RFC 3339.
/// The start of `text`, at most `bytes` long and cut at a character boundary.
fn preview(text: &str, bytes: usize) -> &str {
    let end = (0..=bytes.min(text.len()))
        .rev()
        .find(|end| text.is_char_boundary(*end))
        .unwrap_or(0);
    &text[..end]
}

/// A job for the model. Messages are cut to [`MESSAGE_BYTES`], so a result
/// always fits the tool output cap.
fn describe(job: &Job, thread: Id) -> Value {
    let time = |seconds: u64| {
        i64::try_from(seconds)
            .ok()
            .and_then(|s| jiff::Timestamp::from_second(s).ok())
            .map(|t| t.to_string())
    };
    let schedule = match &job.schedule {
        Schedule::At { at } => json!({ "at": time(*at) }),
        Schedule::Every { seconds, .. } => json!({ "every_seconds": seconds }),
        Schedule::Cron { expression, zone } => json!({ "cron": expression, "zone": zone }),
    };
    let last = job.last.as_ref().map(|run| {
        let outcome = match &run.outcome {
            JobOutcome::Running => json!("running"),
            JobOutcome::Complete => json!("complete"),
            JobOutcome::Failed => json!("failed"),
            JobOutcome::Cancelled => json!("cancelled"),
            JobOutcome::Refused(report) => json!({ "refused": report.message }),
        };
        json!({ "at": time(run.at), "outcome": outcome })
    });
    json!({
        "id": job.id,
        "name": job.name,
        "message": preview(&job.text, MESSAGE_BYTES),
        "message_bytes": job.text.len(),
        "schedule": schedule,
        "enabled": job.enabled,
        "next": job.next.and_then(time),
        "last": last,
        "added_by": if job.author == (JobAuthor::Agent { thread }) { "you" } else { "user" },
    })
}

impl ToolHost for ScheduleHost {
    fn list<'a>(&'a self, _scope: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            ToolListing {
                tools: Self::specs(),
                unavailable: vec![],
            }
        })
    }
    fn call<'a>(
        &'a self,
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            self.run(scope.thread, &request).map(|value| ToolOutput {
                text: value.to_string(),
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests;
