//! `agent206 schedule`: messages the gateway sends to a conversation at set
//! times. Each run is an ordinary message under the conversation's grants.
use a206_core::{Command, Job, Reply, Schedule};
use a206_runtime::client::Client;
use anyhow::{Context, Result, bail};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum ScheduleAction {
    /// List scheduled messages with their next and latest runs.
    List,
    /// Schedule a message. Give exactly one of --at, --every or --cron.
    Add {
        /// The conversation the message is sent to.
        thread: uuid::Uuid,
        /// A short name for the job.
        #[arg(long)]
        name: String,
        /// The message text.
        #[arg(long)]
        text: String,
        /// Once, at an RFC 3339 time such as 2026-10-18T09:00:00Z.
        #[arg(long, group = "when")]
        at: Option<String>,
        /// Repeatedly, at this interval (for example "90m" or "1d").
        #[arg(long, group = "when")]
        every: Option<String>,
        /// Start the --every grid at this RFC 3339 time instead of now.
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
    /// Delete a job. Its conversation keeps the messages it sent.
    Remove { id: uuid::Uuid },
}

fn unix(time: &str) -> Result<u64> {
    let time: jiff::Timestamp = time
        .parse()
        .with_context(|| format!("{time} is not an RFC 3339 time"))?;
    u64::try_from(time.as_second()).context("times before 1970 are not supported")
}

fn schedule(
    at: Option<String>,
    every: Option<String>,
    anchor: Option<String>,
    cron: Option<String>,
    zone: Option<String>,
) -> Result<Schedule> {
    Ok(match (at, every, cron) {
        (Some(at), None, None) => Schedule::At { at: unix(&at)? },
        (None, Some(every), None) => {
            let interval: jiff::SignedDuration = every
                .parse()
                .with_context(|| format!("{every} is not a duration"))?;
            if interval.subsec_nanos() != 0 {
                bail!("intervals are whole seconds; {every} is not");
            }
            let seconds =
                u64::try_from(interval.as_secs()).context("the interval must be positive")?;
            let anchor = match anchor {
                Some(anchor) => unix(&anchor)?,
                None => u64::try_from(jiff::Timestamp::now().as_second())
                    .context("the system clock is before 1970")?,
            };
            Schedule::Every { seconds, anchor }
        }
        (None, None, Some(expression)) => Schedule::Cron { expression, zone },
        _ => bail!("give exactly one of --at, --every or --cron"),
    })
}

fn print(job: &Job) -> Result<()> {
    print!("{}", toml::to_string_pretty(job)?);
    Ok(())
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
            at,
            every,
            anchor,
            cron,
            zone,
        } => Command::ScheduleJob {
            name,
            thread,
            text,
            schedule: schedule(at, every, anchor, cron, zone)?,
        },
        ScheduleAction::Pause { id } => Command::SetJobEnabled { id, enabled: false },
        ScheduleAction::Resume { id } => Command::SetJobEnabled { id, enabled: true },
        ScheduleAction::Run { id } => Command::RunJob { id },
        ScheduleAction::Remove { id } => Command::RemoveJob { id },
    };
    match client.call(command).await? {
        Reply::Job(job) => print(&job),
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
    fn exactly_one_schedule_is_accepted() {
        let seconds = 60 + uuid::Uuid::new_v4().as_u128() as u64 % 10_000;
        let anchor = jiff::Timestamp::from_second(seconds as i64).unwrap();
        assert_eq!(
            schedule(
                None,
                Some(format!("{seconds}s")),
                Some(anchor.to_string()),
                None,
                None
            )
            .unwrap(),
            Schedule::Every {
                seconds,
                anchor: seconds
            }
        );
        assert_eq!(
            schedule(Some(anchor.to_string()), None, None, None, None).unwrap(),
            Schedule::At { at: seconds }
        );
        assert!(schedule(None, None, None, None, None).is_err());
        assert!(
            schedule(
                Some(anchor.to_string()),
                None,
                None,
                Some("* * * * *".into()),
                None
            )
            .is_err()
        );
        assert!(schedule(None, Some("-5m".into()), None, None, None).is_err());
        // Fractions of a second are refused, not truncated.
        let fraction = format!("{seconds}.{}s", 1 + seconds % 9);
        assert!(schedule(None, Some(fraction), None, None, None).is_err());
        assert!(schedule(Some("tomorrow".into()), None, None, None, None).is_err());
    }
}
