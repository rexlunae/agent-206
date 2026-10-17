//! Scheduled messages: text the gateway sends to a conversation at set times.
//! A job is only a timed message; it runs with whatever the conversation is
//! allowed when it fires, so nothing here grants anything.
use crate::{PageFrame, State, dispatch, error::ErrorNotice, route::Route};
use a206_core::schedule::{InputError, format_interval, format_time, parse_interval, parse_time};
use a206_core::{
    Command, ErrorKind, ErrorReport, Job, JobAuthor, JobOutcome, Reply, Schedule, Snapshot,
};
use fc_dioxus::prelude::*;
use uuid::Uuid;

#[component]
pub fn Schedules() -> Element {
    let state = use_context::<State>();
    let snapshot = state.data.read();
    rsx! { PageFrame { kicker: "Schedules", title: "Send messages on a schedule.", description: "Each job sends its text to one conversation at set times. It runs like a message you typed, with that conversation's permissions and model as they are when it fires.",
        if snapshot.jobs.is_empty() {
            EmptyState { title: rsx! { "No scheduled messages" }, icon: rsx! { Icon { name: IconName::Refresh } },
                body: rsx! { "Add one below, or with agent206 schedule add." }
            }
        }
        for job in &snapshot.jobs {
            JobCard { key: "{job.id}", job: job.clone(), target: conversation(&snapshot, job.thread),
                author: match job.author { JobAuthor::User => None, JobAuthor::Agent { thread } => Some(conversation(&snapshot, thread).0) },
            }
        }
        AddJob {}
    } }
}

/// A conversation's title and link, or a note that it is gone.
fn conversation(snapshot: &Snapshot, id: Uuid) -> (String, Option<Route>) {
    match snapshot.threads.iter().find(|thread| thread.id == id) {
        Some(thread) => (
            format!("“{}”", thread.title),
            Some(Route::Chat {
                id,
                split: None,
                message: None,
            }),
        ),
        None => ("a conversation that no longer exists".to_owned(), None),
    }
}

/// When a schedule fires, in words. Times are shown in UTC because the
/// browser build carries no time zone data.
fn describe(schedule: &Schedule) -> String {
    match schedule {
        Schedule::At { at } => match format_time(*at) {
            Some(at) => format!("Once, at {at}"),
            None => "Once, at a time that cannot be shown".to_owned(),
        },
        Schedule::Every { seconds, anchor } => {
            let every = format_interval(*seconds).unwrap_or_else(|| format!("{seconds}s"));
            match format_time(*anchor) {
                Some(anchor) => format!("Every {every}, counted from {anchor}"),
                None => format!("Every {every}"),
            }
        }
        Schedule::Cron { expression, zone } => match zone {
            Some(zone) => format!("Cron “{expression}” in {zone}"),
            None => format!("Cron “{expression}” in the gateway's time zone"),
        },
    }
}

fn next(job: &Job) -> String {
    match job.next.and_then(format_time) {
        Some(next) => format!("Next: {next}"),
        None if !job.enabled => "Paused".to_owned(),
        None => "Not due again".to_owned(),
    }
}

/// The latest run in words, with its tone; a refusal also carries the
/// gateway's report.
fn last(job: &Job) -> Option<(String, BadgeTone, Option<ErrorReport>)> {
    let run = job.last.as_ref()?;
    let at = format_time(run.at).unwrap_or_default();
    Some(match &run.outcome {
        JobOutcome::Running => (format!("Running since {at}"), BadgeTone::Info, None),
        JobOutcome::Complete => (format!("Last run finished {at}"), BadgeTone::Success, None),
        JobOutcome::Failed => (
            format!("Last run failed {at}; the conversation shows why"),
            BadgeTone::Danger,
            None,
        ),
        JobOutcome::Cancelled => (format!("Last run stopped {at}"), BadgeTone::Warning, None),
        JobOutcome::Refused(report) => (
            format!("Last run not sent {at}"),
            BadgeTone::Danger,
            Some(report.clone()),
        ),
    })
}

#[component]
fn JobCard(
    job: Job,
    /// The conversation it sends to, and its link while it exists.
    target: (String, Option<Route>),
    /// The conversation whose agent added it, if one did.
    author: Option<String>,
) -> Element {
    let state = use_context::<State>();
    let (target, link) = target;
    let id = job.id;
    let enabled = job.enabled;
    let latest = last(&job);
    rsx! { Card {
        CardHeader {
            CardTitle { "{job.name}" }
            CardSubtitle { "{describe(&job.schedule)}" }
        }
        CardBody {
            div { class: "a206-row",
                Badge { tone: if enabled { BadgeTone::Accent } else { BadgeTone::Neutral }, "{next(&job)}" }
                if let Some((text, tone, _)) = &latest { Badge { tone: *tone, "{text}" } }
                if let Some(author) = &author { Badge { tone: BadgeTone::Info, "Added by the agent in {author}" } }
            }
            div { class: "a206-row",
                span { class: "a206-muted", "Sends to" }
                match link {
                    Some(link) => rsx! { LinkButton { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, href: link.to_string(), "{target}" } },
                    None => rsx! { span { "{target}" } },
                }
            }
            p { class: "a206-message-text", "{job.text}" }
            if let Some((_, _, Some(report))) = latest {
                ErrorNotice { report, title: "The gateway did not send the last run".to_string() }
            }
        }
        CardFooter {
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Secondary, onclick: move |_| dispatch(state, Command::SetJobEnabled { id, enabled: !enabled }),
                if enabled { "Pause" } else { "Resume" }
            }
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Secondary, onclick: move |_| dispatch(state, Command::RunJob { id }), "Run now" }
            Button { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, onclick: move |_| dispatch(state, Command::RemoveJob { id }), "Remove" }
        }
    } }
}

/// How a new job repeats.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Repeat {
    Once,
    Every,
    Cron,
}

impl Repeat {
    const ALL: [Self; 3] = [Self::Once, Self::Every, Self::Cron];
    fn value(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Every => "every",
            Self::Cron => "cron",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Once => "Once",
            Self::Every => "At an interval",
            Self::Cron => "On a cron expression",
        }
    }
}

/// The schedule the form describes. An interval is counted from `now`.
fn schedule(repeat: Repeat, when: &str, zone: &str, now: u64) -> Result<Schedule, InputError> {
    Ok(match repeat {
        Repeat::Once => Schedule::At {
            at: parse_time(when)?,
        },
        Repeat::Every => Schedule::Every {
            seconds: parse_interval(when)?,
            anchor: now,
        },
        Repeat::Cron => Schedule::Cron {
            expression: when.trim().to_owned(),
            zone: Some(zone.trim().to_owned()).filter(|zone| !zone.is_empty()),
        },
    })
}

#[component]
fn AddJob() -> Element {
    let state = use_context::<State>();
    let mut name = use_signal(String::new);
    let mut thread = use_signal(|| None::<String>);
    let mut text = use_signal(String::new);
    let mut repeat = use_signal(|| Some(Repeat::Once.value().to_owned()));
    let mut when = use_signal(String::new);
    let mut zone = use_signal(String::new);
    let mut failure = use_signal(|| None::<ErrorReport>);
    let mut sending = use_signal(|| false);
    let snapshot = state.data.read();
    let choices: Vec<_> = snapshot
        .threads
        .iter()
        .map(|t| SelectChoice::new(t.id.to_string(), t.title.clone()))
        .collect();
    let selected = Repeat::ALL
        .into_iter()
        .find(|r| Some(r.value()) == repeat().as_deref())
        .unwrap_or(Repeat::Once);
    let target = thread()
        .and_then(|id| Uuid::parse_str(&id).ok())
        .filter(|id| snapshot.threads.iter().any(|t| t.id == *id));
    let parsed = (!when().trim().is_empty())
        .then(|| schedule(selected, &when(), &zone(), crate::messengers::now()));
    let invalid = match &parsed {
        Some(Err(error)) => Some(error.to_string()),
        _ => None,
    };
    let valid = parsed
        .as_ref()
        .and_then(|parsed| parsed.as_ref().ok())
        .cloned();
    let ready = !sending()
        && target.is_some()
        && !name().trim().is_empty()
        && !text().trim().is_empty()
        && valid.is_some();
    let (when_label, when_hint, placeholder) = match selected {
        Repeat::Once => (
            "Time",
            "An RFC 3339 time with its offset, such as 2026-10-18T09:00:00Z.",
            "2026-10-18T09:00:00Z",
        ),
        Repeat::Every => (
            "Interval",
            "At least a minute, such as 90m, 1h 30m or 1d. Counted from now.",
            "1h",
        ),
        Repeat::Cron => (
            "Cron expression",
            "Five fields, or six with seconds first. The gateway checks it.",
            "0 9 * * 1-5",
        ),
    };
    rsx! { Card { variant: CardVariant::Elevated,
        CardHeader { CardTitle { "Schedule a message" } CardSubtitle { "It starts enabled. Pause or remove it at any time." } }
        CardBody {
            if snapshot.threads.is_empty() {
                p { class: "a206-muted", "Create a conversation first; a scheduled message is sent to one." }
            }
            div { class: "a206-grid",
                Field { label: rsx! { "Name" }, Input { value: ReadSignal::from(name), oninput: move |v| name.set(v), placeholder: "Morning check" } }
                Field { label: rsx! { "Conversation" }, Select { label: name!("Conversation to send to"), value: ReadSignal::from(thread), options: choices, onchange: move |value: String| thread.set(Some(value)) } }
                Field { label: rsx! { "Repeats" }, Select { label: name!("How the message repeats"), value: ReadSignal::from(repeat),
                    options: Repeat::ALL.into_iter().map(|r| SelectChoice::new(r.value(), r.label())).collect(),
                    onchange: move |value: String| repeat.set(Some(value))
                } }
                Field { label: rsx! { "{when_label}" }, hint: rsx! { "{when_hint}" }, error: invalid.map(|error| rsx! { "{error}" }),
                    Input { value: ReadSignal::from(when), oninput: move |v| when.set(v), placeholder }
                }
                if selected == Repeat::Cron {
                    Field { label: rsx! { "Time zone" }, hint: rsx! { "An IANA name such as Europe/Paris. Leave empty for the gateway's own." }, Input { value: ReadSignal::from(zone), oninput: move |v| zone.set(v) } }
                }
            }
            Field { label: rsx! { "Message" }, Textarea { autosize: true, rows: "2", value: ReadSignal::from(text), oninput: move |v| text.set(v) } }
            if let Some(report) = failure() {
                ErrorNotice { report, title: "Not scheduled".to_string(), dismiss: move |_| failure.set(None) }
            }
        }
        CardFooter { Button { variant: ButtonVariant::Primary, availability: if ready { Availability::Enabled } else { Availability::Disabled }, onclick: move |_| {
            let (Some(thread_id), Some(schedule)) = (target, valid.clone()) else { return };
            let command = Command::ScheduleJob { name: name().trim().to_owned(), thread: thread_id, text: text(), schedule };
            sending.set(true);
            spawn(async move {
                match crate::call(command).await {
                    Ok(Reply::Job(_)) => {
                        failure.set(None);
                        name.set(String::new());
                        text.set(String::new());
                        when.set(String::new());
                    }
                    Ok(_) => failure.set(Some(ErrorReport::new(ErrorKind::Protocol, "Unexpected schedule response"))),
                    Err(report) => failure.set(Some(report)),
                }
                sending.set(false);
            });
        }, if sending() { "Scheduling…" } else { "Schedule message" } } }
    } }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a206_core::{JobRun, Mode, Thread};

    fn job(thread: Uuid, schedule: Schedule) -> Job {
        Job {
            id: Uuid::new_v4(),
            name: Uuid::new_v4().simple().to_string(),
            thread,
            text: Uuid::new_v4().to_string(),
            schedule,
            enabled: true,
            created: 0,
            next: None,
            last: None,
            author: JobAuthor::User,
        }
    }

    #[derive(Clone, Debug, PartialEq, Routable)]
    enum PageRoute {
        #[route("/")]
        Page {},
    }

    #[component]
    fn Page() -> Element {
        let state = crate::route::tests::loaded_state(use_context::<Snapshot>());
        use_context_provider(|| state);
        rsx! { Schedules {} }
    }

    /// The Schedules page showing `snapshot` with `job` as its only job.
    fn render(mut snapshot: Snapshot, job: Job) -> String {
        use dioxus::history::{History, MemoryHistory};
        fn app() -> Element {
            rsx! { Router::<PageRoute> {} }
        }
        snapshot.jobs = vec![job];
        let mut dom = VirtualDom::new(app);
        dom.provide_root_context(snapshot);
        dom.provide_root_context(
            std::rc::Rc::new(MemoryHistory::default()) as std::rc::Rc<dyn History>
        );
        dom.rebuild_in_place();
        dioxus_ssr::render(&dom)
    }

    #[test]
    fn the_form_builds_each_kind_of_schedule_or_says_why_not() {
        let now = u64::from(Uuid::new_v4().as_fields().1) + 1;
        let at = format_time(now).unwrap();
        assert_eq!(
            schedule(Repeat::Once, &at, "", 0).unwrap(),
            Schedule::At { at: now }
        );
        assert_eq!(
            schedule(Repeat::Every, "1h 30m", "", now).unwrap(),
            Schedule::Every {
                seconds: 5_400,
                anchor: now
            }
        );
        let expression = format!("0 {} * * *", now % 24);
        assert_eq!(
            schedule(Repeat::Cron, &format!(" {expression} "), "  ", 0).unwrap(),
            Schedule::Cron {
                expression: expression.clone(),
                zone: None
            }
        );
        let zone = Uuid::new_v4().simple().to_string();
        assert_eq!(
            schedule(Repeat::Cron, &expression, &zone, 0).unwrap(),
            Schedule::Cron {
                expression,
                zone: Some(zone)
            }
        );
        assert!(matches!(
            schedule(Repeat::Once, "tomorrow", "", 0),
            Err(InputError::Time { .. })
        ));
        assert!(matches!(
            schedule(Repeat::Every, "0s", "", now),
            Err(InputError::NonPositiveInterval)
        ));
    }

    #[test]
    fn a_job_card_names_its_conversation_schedule_and_latest_run() {
        let thread = Thread::new(Uuid::new_v4().simple().to_string(), Mode::Chat);
        let mut snapshot = crate::empty_snapshot();
        snapshot.threads.push(thread.clone());
        let mut scheduled = job(
            thread.id,
            Schedule::Every {
                seconds: 5_400,
                anchor: 0,
            },
        );
        let next = u64::from(Uuid::new_v4().as_fields().1);
        scheduled.next = Some(next);
        scheduled.author = JobAuthor::Agent { thread: thread.id };
        let html = render(snapshot.clone(), scheduled.clone());
        assert!(html.contains(&scheduled.name));
        assert!(html.contains("Every 1h 30m, counted from 1970-01-01T00:00:00Z"));
        assert!(html.contains(&format!("Next: {}", format_time(next).unwrap())));
        assert!(html.contains(&format!("Added by the agent in “{}”", thread.title)));
        let link = Route::Chat {
            id: thread.id,
            split: None,
            message: None,
        };
        assert!(html.contains(&format!("href=\"{link}\"")));
        assert!(html.contains("Pause"));

        // A refused run shows the gateway's report; a paused job says so.
        let code = Uuid::new_v4().simple().to_string();
        let mut refused = job(Uuid::new_v4(), Schedule::At { at: 0 });
        refused.enabled = false;
        refused.last = Some(JobRun {
            id: Uuid::new_v4(),
            at: 0,
            outcome: JobOutcome::Refused(ErrorReport::new(ErrorKind::Conflict, &code)),
        });
        let html = render(snapshot, refused);
        assert!(html.contains("a conversation that no longer exists"));
        assert!(html.contains("Paused"));
        assert!(html.contains("Resume"));
        assert!(html.contains("The gateway did not send the last run"));
        assert!(html.contains(&code));
    }
}
