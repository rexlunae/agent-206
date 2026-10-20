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
    /// Schedule a message (--text), a heartbeat check of the project's
    /// HEARTBEAT.md (--heartbeat) or a check of an extension's trigger
    /// (--trigger). Give exactly one of --at, --in, --every or --cron.
    Add(Box<AddJob>),
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

#[derive(clap::Args)]
pub struct AddJob {
    /// The conversation the message is sent to.
    thread: uuid::Uuid,
    /// A short name for the job.
    #[arg(long)]
    name: String,
    /// The message text; with --trigger, optional text sent before the
    /// extension's report when it fires.
    #[arg(long, required_unless_present_any = ["heartbeat", "trigger"])]
    text: Option<String>,
    /// Send the checklist in HEARTBEAT.md at the conversation's project
    /// root, read when the job runs; a reply of exactly HEARTBEAT_OK is
    /// recorded as quiet.
    #[arg(long, conflicts_with_all = ["text", "trigger"])]
    heartbeat: bool,
    /// Check an enabled extension's trigger each run, as EXTENSION/TRIGGER
    /// (see `extension list`); when it fires, its report is sent.
    #[arg(long, value_parser = trigger)]
    trigger: Option<(String, String)>,
    /// The trigger's arguments as TOML, e.g. --args 'url = "https://example.org/"'.
    #[arg(long, requires = "trigger", default_value = "")]
    args: String,
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
}

/// `EXTENSION/TRIGGER`; extension ids hold no `/`.
fn trigger(text: &str) -> Result<(String, String), String> {
    match text.split_once('/') {
        Some((extension, trigger)) if !extension.is_empty() && !trigger.is_empty() => {
            Ok((extension.to_owned(), trigger.to_owned()))
        }
        _ => Err("give the trigger as EXTENSION/TRIGGER".to_owned()),
    }
}

/// What an added job sends.
fn kind(heartbeat: bool, trigger: Option<(String, String)>, args: &str) -> Result<JobKind> {
    Ok(match trigger {
        Some((extension, trigger)) => JobKind::Trigger {
            extension,
            trigger,
            args: crate::extension_cli::toml_args(args)?,
        },
        None if heartbeat => JobKind::Heartbeat,
        None => JobKind::Message,
    })
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
        ScheduleAction::Add(add) => {
            let AddJob {
                thread,
                name,
                text,
                heartbeat,
                trigger,
                args,
                at,
                after,
                every,
                anchor,
                cron,
                zone,
            } = *add;
            Command::ScheduleJob {
                kind: kind(heartbeat, trigger, &args)?,
                name,
                thread,
                text: text.unwrap_or_default(),
                schedule: schedule(at, after, every, anchor, cron, zone)?,
            }
        }
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
    fn a_trigger_job_names_its_extension_trigger_and_arguments() {
        use a206_core::extension::Value;
        let (extension, name, url) = (
            format!("x.{}", uuid::Uuid::new_v4().simple()),
            format!("t{}", uuid::Uuid::new_v4().simple()),
            format!("https://{}.example/", uuid::Uuid::new_v4().simple()),
        );
        let named = trigger(&format!("{extension}/{name}")).unwrap();
        assert_eq!(named, (extension.clone(), name.clone()));
        assert!(trigger(&extension).is_err());
        assert!(trigger(&format!("/{name}")).is_err());
        let JobKind::Trigger {
            extension: e,
            trigger: t,
            args,
        } = kind(false, Some(named), &format!("url = \"{url}\"")).unwrap()
        else {
            panic!("not a trigger check");
        };
        assert_eq!((e, t), (extension, name));
        assert_eq!(args["url"], Value::Str(url));
        assert_eq!(kind(true, None, "").unwrap(), JobKind::Heartbeat);
        assert_eq!(kind(false, None, "").unwrap(), JobKind::Message);
    }

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
        let Ok(ScheduleAction::Add(add)) = parse(&["--heartbeat"]) else {
            panic!("a heartbeat check was refused");
        };
        let AddJob {
            text, heartbeat, ..
        } = *add;
        assert!(heartbeat && text.is_none());
        let message = uuid::Uuid::new_v4().to_string();
        let Ok(ScheduleAction::Add(add)) = parse(&["--text", &message]) else {
            panic!("a message was refused");
        };
        let AddJob {
            text, heartbeat, ..
        } = *add;
        assert!(!heartbeat && text == Some(message.clone()));
        assert!(parse(&[]).is_err(), "neither text nor a heartbeat");
        assert!(parse(&["--text", &message, "--heartbeat"]).is_err());
        // A trigger check may go without text, but not with a heartbeat.
        let Ok(ScheduleAction::Add(add)) = parse(&["--trigger", "x.y/changed"]) else {
            panic!("a trigger check was refused");
        };
        assert_eq!(add.trigger, Some(("x.y".into(), "changed".into())));
        assert!(parse(&["--trigger", "x.y/changed", "--heartbeat"]).is_err());
        assert!(parse(&["--text", &message, "--args", "a = 1"]).is_err());
    }
}
