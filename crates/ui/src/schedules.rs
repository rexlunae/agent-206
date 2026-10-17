//! Scheduled messages: text the gateway sends to a conversation at set times.
//! The same gateway commands as `agent206 schedule`; a job carries no
//! authority of its own and runs under its conversation's permissions.
use crate::{PageFrame, State, dispatch, error::ErrorNotice, route::Route};
use a206_core::{
    Command, Job, JobAuthor, JobOutcome, Schedule, ScheduleInputError, Snapshot,
    schedule::{format_interval, parse_interval, parse_local},
};
use fc_dioxus::prelude::*;
use jiff::{Timestamp, tz::TimeZone};
use uuid::Uuid;

/// Seconds since the Unix epoch on this client's clock.
fn now() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A Unix time in `zone`, as people read it.
fn show_time(at: u64, zone: &TimeZone) -> String {
    i64::try_from(at)
        .ok()
        .and_then(|at| Timestamp::from_second(at).ok())
        .map_or_else(
            || format!("{at} seconds after 1970"),
            |time| {
                time.to_zoned(zone.clone())
                    .strftime("%a %-d %b %Y, %H:%M %Z")
                    .to_string()
            },
        )
}

/// What a schedule means, in words.
fn describe(schedule: &Schedule, zone: &TimeZone) -> String {
    match schedule {
        Schedule::At { at } => format!("Once, {}", show_time(*at, zone)),
        Schedule::Every { seconds, anchor } => format!(
            "Every {}, counted from {}",
            format_interval(*seconds),
            show_time(*anchor, zone)
        ),
        Schedule::Cron {
            expression,
            zone: Some(cron_zone),
        } => format!("Cron `{expression}` in {cron_zone}"),
        Schedule::Cron {
            expression,
            zone: None,
        } => format!("Cron `{expression}` in the gateway's time zone"),
    }
}

fn outcome(outcome: &JobOutcome) -> (BadgeTone, &'static str) {
    match outcome {
        JobOutcome::Running => (BadgeTone::Info, "Running"),
        JobOutcome::Complete => (BadgeTone::Success, "Complete"),
        JobOutcome::Failed => (BadgeTone::Danger, "Failed"),
        JobOutcome::Cancelled => (BadgeTone::Neutral, "Cancelled"),
        JobOutcome::Refused(_) => (BadgeTone::Warning, "Not sent"),
    }
}

fn title(snapshot: &Snapshot, thread: Uuid) -> Option<String> {
    snapshot
        .threads
        .iter()
        .find(|t| t.id == thread)
        .map(|t| t.title.clone())
}

#[component]
pub fn Schedules() -> Element {
    let state = use_context::<State>();
    let snapshot = state.data.read();
    let mut jobs: Vec<&Job> = snapshot.jobs.iter().collect();
    // Soonest first; paused and finished jobs after every pending one.
    jobs.sort_by_key(|job| (job.next.is_none(), job.next, job.name.clone()));
    rsx! { PageFrame { kicker: "Workbench", title: "Messages on a schedule.", description: "Send a conversation a message once, on an interval or on a cron schedule. Each one runs as if you typed it, with that conversation's permissions at the time.",
        if jobs.is_empty() { EmptyState { compact: true, title: rsx! { "Nothing scheduled" }, body: rsx! { "Scheduled messages appear here, whether you add them or a conversation's model does with your approval." } } }
        for job in jobs {
            JobCard { key: "{job.id}", job: job.clone(), conversation: title(&snapshot, job.thread) }
        }
        AddJob {}
    } }
}

#[component]
fn JobCard(job: Job, conversation: Option<String>) -> Element {
    let state = use_context::<State>();
    let zone = TimeZone::system();
    let id = job.id;
    let author = match job.author {
        JobAuthor::User => "Added by you",
        JobAuthor::Agent { .. } => "Added by the conversation's model, with your approval",
    };
    rsx! { Card {
        CardHeader { CardTitle { "{job.name}" } CardSubtitle { "{describe(&job.schedule, &zone)}" } }
        CardBody {
            div { class: "a206-row",
                if job.enabled { Badge { tone: BadgeTone::Accent, "Active" } } else { Badge { "Paused" } }
                span { class: "a206-muted", "{author}" }
            }
            p { "To " match conversation {
                Some(title) => rsx! { LinkButton { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, href: Route::Chat { id: job.thread, split: None, message: None }.to_string(), "{title}" } },
                None => rsx! { span { class: "a206-muted", "a conversation that no longer exists" } },
            } }
            blockquote { class: "a206-quote", "{job.text}" }
            p { class: "a206-muted", match (job.enabled, job.next) {
                (true, Some(next)) => format!("Next: {}", show_time(next, &zone)),
                (false, _) => "Paused; resuming skips the times it missed.".to_owned(),
                (true, None) => "It will not run again.".to_owned(),
            } }
            if let Some(last) = &job.last {
                { let (tone, label) = outcome(&last.outcome); rsx! {
                    div { class: "a206-row", span { class: "a206-muted", "Latest run {show_time(last.at, &zone)}" } Badge { tone, "{label}" } }
                } }
                if let JobOutcome::Refused(report) = &last.outcome { ErrorNotice { report: report.clone(), title: "The latest run was not sent".to_string() } }
            }
        }
        CardFooter {
            Button { variant: ButtonVariant::Secondary, onclick: move |_| dispatch(state, Command::SetJobEnabled { id, enabled: !job.enabled }), if job.enabled { "Pause" } else { "Resume" } }
            Button { variant: ButtonVariant::Secondary, onclick: move |_| dispatch(state, Command::RunJob { id }), "Run now" }
            Button { variant: ButtonVariant::Ghost, onclick: move |_| dispatch(state, Command::RemoveJob { id }), "Remove" }
        }
    } }
}

/// How the form's job repeats.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Once,
    Every,
    Cron,
}
impl Kind {
    const ALL: [Self; 3] = [Self::Once, Self::Every, Self::Cron];
    fn label(self) -> &'static str {
        match self {
            Self::Once => "Once",
            Self::Every => "On an interval",
            Self::Cron => "On a cron schedule",
        }
    }
}

/// The schedule the form describes, or why it cannot be one.
fn form_schedule(
    kind: Kind,
    date: &str,
    time: &str,
    zone: &str,
    every: &str,
    cron: &str,
    now: u64,
) -> Option<Result<Schedule, ScheduleInputError>> {
    match kind {
        Kind::Once if date.trim().is_empty() || time.trim().is_empty() => None,
        Kind::Once => Some(parse_local(date, time, zone).map(|at| Schedule::At { at })),
        Kind::Every if every.trim().is_empty() => None,
        Kind::Every => Some(parse_interval(every).map(|seconds| Schedule::Every {
            seconds,
            anchor: now,
        })),
        Kind::Cron if cron.trim().is_empty() => None,
        Kind::Cron => Some(Ok(Schedule::Cron {
            expression: cron.trim().to_owned(),
            zone: Some(zone.trim().to_owned()).filter(|z| !z.is_empty()),
        })),
    }
}

#[component]
fn AddJob() -> Element {
    let state = use_context::<State>();
    let mut name = use_signal(String::new);
    let mut thread = use_signal(|| None::<String>);
    let mut text = use_signal(String::new);
    let mut kind = use_signal(|| Kind::Once);
    let mut date = use_signal(String::new);
    let mut time = use_signal(String::new);
    let mut every = use_signal(String::new);
    let mut cron = use_signal(String::new);
    // The client's own zone; the field shows it so it is never assumed.
    let mut zone = use_signal(|| {
        TimeZone::system()
            .iana_name()
            .unwrap_or_default()
            .to_owned()
    });
    let conversations = state
        .data
        .read()
        .threads
        .iter()
        .map(|t| SelectChoice::new(t.id.to_string(), t.title.clone()))
        .collect::<Vec<_>>();
    let schedule = form_schedule(kind(), &date(), &time(), &zone(), &every(), &cron(), now());
    let invalid = match &schedule {
        Some(Err(error)) => Some(error.to_string()),
        _ => None,
    };
    let target = thread().and_then(|id| id.parse::<Uuid>().ok());
    let valid = matches!(schedule, Some(Ok(_)));
    let ready = !name().trim().is_empty() && !text().trim().is_empty() && target.is_some() && valid;
    let kind_label = use_memo(move || Some(kind().label().to_owned()));
    rsx! { Card { variant: CardVariant::Elevated,
        CardHeader { CardTitle { "Schedule a message" } CardSubtitle { "The gateway checks the schedule and the conversation when you add it." } }
        CardBody { div { class: "a206-grid",
            Field { label: rsx! { "Name" }, Input { value: ReadSignal::from(name), oninput: move |v| name.set(v), placeholder: "Morning summary" } }
            Field { label: rsx! { "Conversation" }, Select { label: name!("Conversation to message"), value: ReadSignal::from(thread), placeholder: "Choose a conversation", options: conversations, onchange: move |id: String| thread.set(Some(id)) } }
            Field { label: rsx! { "Message" }, Textarea { value: ReadSignal::from(text), oninput: move |v| text.set(v), rows: "3" } }
            Field { label: rsx! { "Repeat" }, Select { label: name!("How the message repeats"), value: ReadSignal::from(kind_label),
                options: Kind::ALL.into_iter().map(|k| SelectChoice::new(k.label(), k.label())).collect(),
                onchange: move |label: String| if let Some(k) = Kind::ALL.into_iter().find(|k| k.label() == label) { kind.set(k) }
            } }
            match kind() {
                Kind::Once => rsx! {
                    Field { label: rsx! { "Date" }, Input { r#type: InputType::Date, value: ReadSignal::from(date), oninput: move |v| date.set(v) } }
                    Field { label: rsx! { "Time" }, Input { r#type: InputType::Time, value: ReadSignal::from(time), oninput: move |v| time.set(v) } }
                },
                Kind::Every => rsx! {
                    Field { label: rsx! { "Interval" }, hint: rsx! { "Such as 90m, 6h or 1d; at least a minute. Counted from now." }, error: invalid.clone().map(|e| rsx! { "{e}" }), Input { value: ReadSignal::from(every), oninput: move |v| every.set(v), placeholder: "1d" } }
                },
                Kind::Cron => rsx! {
                    Field { label: rsx! { "Cron expression" }, hint: rsx! { "Five fields, or six with seconds first." }, Input { value: ReadSignal::from(cron), oninput: move |v| cron.set(v), placeholder: "0 9 * * 1-5" } }
                },
            }
            if kind() != Kind::Every {
                Field { label: rsx! { "Time zone" }, hint: rsx! { if kind() == Kind::Cron { "An IANA name such as Europe/Paris. Leave empty for the gateway's own zone." } else { "An IANA name such as Europe/Paris." } }, error: invalid.clone().map(|e| rsx! { "{e}" }), Input { value: ReadSignal::from(zone), oninput: move |v| zone.set(v) } }
            }
        } }
        CardFooter { Button { variant: ButtonVariant::Primary, availability: if ready { Availability::Enabled } else { Availability::Disabled }, onclick: move |_| {
            // Built again now, so an interval counts from the click rather
            // than from when the form last rendered.
            let Some(Ok(schedule)) = form_schedule(kind(), &date(), &time(), &zone(), &every(), &cron(), now()) else { return };
            let Some(thread) = target else { return };
            dispatch(state, Command::ScheduleJob { name: name().trim().to_owned(), thread, text: text(), schedule });
        }, "Schedule message" } }
    } }
}

#[cfg(test)]
mod tests;
