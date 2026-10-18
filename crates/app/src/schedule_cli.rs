//! `agent206 schedule`: messages the gateway sends to a conversation at set
//! times. Each run is an ordinary message under the conversation's grants.
use a206_core::{
    Command, Job, JobKind, JobRun, Reply, Schedule,
    schedule::{parse_instant, parse_interval},
};
use a206_runtime::client::Client;
use anyhow::{Result, bail};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum ScheduleAction {
    /// List scheduled messages with their next and latest runs.
    List,
    /// Schedule a message (--text) or a heartbeat check of the project's
    /// HEARTBEAT.md (--heartbeat). Give exactly one of --at, --in, --every or --cron.
    Add {
        /// The conversation the message is sent to.
        thread: uuid::Uuid,
        /// A short name for the job.
        #[arg(long)]
        name: String,
        /// The message text.
        #[arg(long, required_unless_present = "heartbeat")]
        text: Option<String>,
        /// Send the checklist in HEARTBEAT.md at the conversation's project
        /// root, read when the job runs; a reply of exactly HEARTBEAT_OK is
        /// recorded as quiet.
        #[arg(long, conflicts_with = "text")]
        heartbeat: bool,
        /// Once, at an RFC 3339 time such as 2026-10-18T09:00:00Z.
        #[arg(long, group = "when")]
        at: Option<String>,
        /// Once, after this long (for example "90m" or "1d"), by the gateway's clock.
        #[arg(long = "in", group = "when")]
        after: Option<String>,
        /// Repeatedly, at this interval (for example "90m" or "1d").
        #[arg(long, group = "when")]
        every: Option<String>,
        /// Start the --every grid at this RFC 3339 time instead of when the
        /// gateway accepts the job.
        #[arg(long, requires = "every")]
        anchor: Option<String>,
        /// On a cron expression: five fields, or six with seconds first.
        #[arg(long, group = "when")]
        cron: Option<String>,
        /// The IANA time zone of --cron, when not the gateway's own.
        #[arg(long, requires = "cron")]
        zone: Option<String>,
    },
    /// Stop a job from running until it is resumed.
    Pause { id: uuid::Uuid },
    /// Resume a paused job from now; times passed while paused are skipped.
    Resume { id: uuid::Uuid },
    /// Run a job now, leaving its schedule as it is.
    Run { id: uuid::Uuid },
    /// Also post each run's reply to a webhook (by its name) or an allowed
    /// chat of a messenger that can post (as name/chat); without a
    /// destination, stop posting. A heartbeat check posts only replies that
    /// need attention.
    Deliver { id: uuid::Uuid, to: Option<String> },
    /// List a job's latest runs, newest first, as TOML.
    Runs { id: uuid::Uuid },
    /// Delete a job. Its conversation keeps the messages it sent.
    Remove { id: uuid::Uuid },
}

fn schedule(
    at: Option<String>,
    after: Option<String>,
    every: Option<String>,
    anchor: Option<String>,
    cron: Option<String>,
    zone: Option<String>,
) -> Result<Schedule> {
    Ok(match (at, after, every, cron) {
        (Some(at), None, None, None) => Schedule::At {
            at: parse_instant(&at)?,
        },
        (None, Some(after), None, None) => Schedule::In {
            seconds: parse_interval(&after)?,
        },
        (None, None, Some(every), None) => Schedule::Every {
            seconds: parse_interval(&every)?,
            anchor: anchor.as_deref().map(parse_instant).transpose()?,
        },
        (None, None, None, Some(expression)) => Schedule::Cron { expression, zone },
        _ => bail!("give exactly one of --at, --in, --every or --cron"),
    })
}

fn print(job: &Job) -> Result<()> {
    print!("{}", toml::to_string_pretty(job)?);
    Ok(())
}

/// A job's runs as TOML.
fn history(runs: Vec<JobRun>) -> Result<String> {
    #[derive(serde::Serialize)]
    struct History {
        runs: Vec<JobRun>,
    }
    Ok(toml::to_string_pretty(&History { runs })?)
}

pub async fn run(client: &Client, action: ScheduleAction) -> Result<()> {
    let command = match action {
        ScheduleAction::List => {
            let jobs = match client.call(Command::Snapshot).await? {
                Reply::Snapshot(snapshot) => snapshot.jobs,
                Reply::Error(report) => return Err(report.into()),
                _ => bail!("gateway did not return its state"),
            };
            #[derive(serde::Serialize)]
            struct Listing {
                jobs: Vec<Job>,
            }
            print!("{}", toml::to_string_pretty(&Listing { jobs })?);
            return Ok(());
        }
        ScheduleAction::Add {
            thread,
            name,
            text,
            heartbeat,
            at,
            after,
            every,
            anchor,
            cron,
            zone,
        } => Command::ScheduleJob {
            kind: if heartbeat {
                JobKind::Heartbeat
            } else {
                JobKind::Message
            },
            name,
            thread,
            text: text.unwrap_or_default(),
            schedule: schedule(at, after, every, anchor, cron, zone)?,
        },
        ScheduleAction::Pause { id } => Command::SetJobEnabled { id, enabled: false },
        ScheduleAction::Resume { id } => Command::SetJobEnabled { id, enabled: true },
        ScheduleAction::Run { id } => Command::RunJob { id },
        ScheduleAction::Runs { id } => Command::JobRuns { id },
        ScheduleAction::Deliver { id, to } => Command::SetJobDelivery { id, to },
        ScheduleAction::Remove { id } => Command::RemoveJob { id },
    };
    match client.call(command).await? {
        Reply::Job(job) => print(&job),
        Reply::JobRuns(runs) => {
            print!("{}", history(runs)?);
            Ok(())
        }
        Reply::Ok => {
            println!("OK");
            Ok(())
        }
        Reply::Error(report) => Err(report.into()),
        _ => bail!("gateway sent an unexpected reply"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_history_prints_every_run_with_its_outcome() {
        use a206_core::{Delivery, ErrorKind, ErrorReport, JobOutcome};
        let at = uuid::Uuid::new_v4().as_u128() as u64 % 4_000_000_000;
        let refusal = uuid::Uuid::new_v4().simple().to_string();
        let running = JobRun::start(at);
        let refused = JobRun {
            outcome: JobOutcome::Refused(ErrorReport::new(ErrorKind::Conflict, refusal.clone())),
            ended: Some(at),
            ..JobRun::start(at)
        };
        let (parts, unposted) = (
            2 + uuid::Uuid::new_v4().as_u128() as u64 % 8,
            uuid::Uuid::new_v4().simple().to_string(),
        );
        let posted = JobRun {
            outcome: JobOutcome::Complete,
            delivery: Some(Delivery::Sent { parts }),
            ..JobRun::start(at)
        };
        let failed = JobRun {
            outcome: JobOutcome::Complete,
            delivery: Some(Delivery::Failed(ErrorReport::new(
                ErrorKind::NotFound,
                unposted.clone(),
            ))),
            ..JobRun::start(at)
        };
        let text = history(vec![running.clone(), refused.clone(), posted, failed]).unwrap();
        assert_eq!(text.matches("[[runs]]").count(), 4, "{text}");
        assert!(text.contains(&running.id.to_string()), "{text}");
        assert!(text.contains(&refusal), "{text}");
        assert!(text.contains(&format!("parts = {parts}")), "{text}");
        assert!(text.contains(&unposted), "{text}");
    }

    #[test]
    fn exactly_one_schedule_is_accepted() {
        let seconds = 60 + uuid::Uuid::new_v4().as_u128() as u64 % 10_000;
        let anchor = jiff::Timestamp::from_second(seconds as i64).unwrap();
        assert_eq!(
            schedule(
                None,
                None,
                Some(format!("{seconds}s")),
                Some(anchor.to_string()),
                None,
                None
            )
            .unwrap(),
            Schedule::Every {
                seconds,
                anchor: Some(seconds)
            }
        );
        // Without --anchor, and for --in, the gateway's clock decides.
        assert_eq!(
            schedule(None, None, Some(format!("{seconds}s")), None, None, None).unwrap(),
            Schedule::Every {
                seconds,
                anchor: None
            }
        );
        assert_eq!(
            schedule(None, Some(format!("{seconds}s")), None, None, None, None).unwrap(),
            Schedule::In { seconds }
        );
        assert_eq!(
            schedule(Some(anchor.to_string()), None, None, None, None, None).unwrap(),
            Schedule::At { at: seconds }
        );
        assert!(schedule(None, None, None, None, None, None).is_err());
        assert!(
            schedule(
                Some(anchor.to_string()),
                None,
                None,
                None,
                Some("* * * * *".into()),
                None
            )
            .is_err()
        );
        assert!(schedule(None, None, Some("-5m".into()), None, None, None).is_err());
        // Fractions of a second are refused, not truncated.
        let fraction = format!("{seconds}.{}s", 1 + seconds % 9);
        assert!(schedule(None, None, Some(fraction), None, None, None).is_err());
        assert!(schedule(Some("tomorrow".into()), None, None, None, None, None).is_err());
    }

    #[test]
    fn a_job_sends_either_text_or_a_heartbeat_check() {
        #[derive(clap::Parser)]
        struct Cli {
            #[command(subcommand)]
            action: ScheduleAction,
        }
        let thread = uuid::Uuid::new_v4().to_string();
        let name = uuid::Uuid::new_v4().to_string();
        let parse = |extra: &[&str]| {
            let mut args = vec!["schedule", "add", &thread, "--name", &name, "--every", "1h"];
            args.extend_from_slice(extra);
            <Cli as clap::Parser>::try_parse_from(args).map(|cli| cli.action)
        };
        let Ok(ScheduleAction::Add {
            text, heartbeat, ..
        }) = parse(&["--heartbeat"])
        else {
            panic!("a heartbeat check was refused");
        };
        assert!(heartbeat && text.is_none());
        let message = uuid::Uuid::new_v4().to_string();
        let Ok(ScheduleAction::Add {
            text, heartbeat, ..
        }) = parse(&["--text", &message])
        else {
            panic!("a message was refused");
        };
        assert!(!heartbeat && text == Some(message.clone()));
        assert!(parse(&[]).is_err(), "neither text nor a heartbeat");
        assert!(parse(&["--text", &message, "--heartbeat"]).is_err());
    }
}
