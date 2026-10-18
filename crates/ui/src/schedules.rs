//! Scheduled messages: text the gateway sends to a conversation at set times,
//! heartbeat checks and checks of extension triggers.
//! The same gateway commands as `agent206 schedule`; a job carries no
//! authority of its own and runs under its conversation's permissions.
use crate::{PageFrame, State, call, dispatch, error::ErrorNotice, route::Route};
use a206_core::{
    Command, Delivery, ErrorKind, ErrorReport, Job, JobAuthor, JobKind, JobOutcome, JobRun,
    PluginSummary, Reply, Schedule, ScheduleInputError, Snapshot,
    extension::{Args, ParamKind, ParamSpec, TriggerSpec, Value},
    messengers::post_destinations,
    schedule::{MAX_JOB_RUNS, format_interval, parse_interval, parse_local},
};
use fc_dioxus::prelude::*;
use jiff::{Timestamp, tz::TimeZone};
use uuid::Uuid;

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
        Schedule::In { seconds } => format!("Once, in {}", format_interval(*seconds)),
        Schedule::Every {
            seconds,
            anchor: Some(anchor),
        } => format!(
            "Every {}, counted from {}",
            format_interval(*seconds),
            show_time(*anchor, zone)
        ),
        Schedule::Every {
            seconds,
            anchor: None,
        } => format!("Every {}", format_interval(*seconds)),
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

fn outcome(outcome: &JobOutcome, kind: &JobKind) -> (BadgeTone, &'static str) {
    match outcome {
        JobOutcome::Running => (BadgeTone::Info, "Running"),
        // A heartbeat that does not answer HEARTBEAT_OK found something.
        JobOutcome::Complete if *kind == JobKind::Heartbeat => {
            (BadgeTone::Warning, "Needs attention")
        }
        JobOutcome::Complete if matches!(kind, JobKind::Trigger { .. }) => {
            (BadgeTone::Accent, "Fired; answered")
        }
        JobOutcome::Complete => (BadgeTone::Success, "Complete"),
        JobOutcome::Quiet if matches!(kind, JobKind::Trigger { .. }) => {
            (BadgeTone::Neutral, "Did not fire")
        }
        JobOutcome::Quiet => (BadgeTone::Success, "Nothing needed attention"),
        JobOutcome::Failed => (BadgeTone::Danger, "Failed"),
        JobOutcome::Cancelled => (BadgeTone::Neutral, "Cancelled"),
        JobOutcome::Refused(_) => (BadgeTone::Warning, "Not sent"),
        JobOutcome::Unrecorded => (BadgeTone::Neutral, "Ended; result not recorded"),
    }
}

/// How posting a run's reply went, in words.
fn delivery(delivery: &Delivery) -> (BadgeTone, String) {
    match delivery {
        Delivery::Sent { parts: 1 } => (BadgeTone::Success, "Reply posted".to_owned()),
        Delivery::Sent { parts } => (BadgeTone::Success, format!("Reply posted in {parts} parts")),
        Delivery::Failed(_) => (BadgeTone::Warning, "Reply not posted".to_owned()),
        Delivery::Posting => (BadgeTone::Info, "Posting reply".to_owned()),
        Delivery::Unrecorded => (
            BadgeTone::Neutral,
            "Stopped while posting reply; it may not have arrived".to_owned(),
        ),
    }
}

/// The value the "Post replies to" field uses for no destination.
const NOWHERE: &str = "";

/// Choices for where a job's replies are posted: nowhere, each destination
/// configured now, and the job's current one if it no longer is.
fn delivery_choices(destinations: &[String], current: Option<&str>) -> Vec<SelectChoice> {
    let mut choices = vec![SelectChoice::new(NOWHERE, "Nowhere")];
    choices.extend(
        destinations
            .iter()
            .map(|to| SelectChoice::new(to.clone(), to.clone())),
    );
    if let Some(current) = current.filter(|c| !destinations.iter().any(|d| d == c)) {
        choices.push(SelectChoice::new(
            current,
            format!("{current} (no longer configured)"),
        ));
    }
    choices
}

/// What a trigger check does, naming the extension as the user knows it.
fn trigger_summary(plugins: &[PluginSummary], extension: &str, trigger: &str) -> String {
    let plugin = plugins.iter().find(|p| p.id == extension);
    let name = plugin.map_or(extension, |p| p.name.as_str());
    let found = plugin.and_then(|p| p.triggers.iter().find(|t| t.name == trigger));
    match (plugin, found) {
        (Some(_), Some(spec)) => format!(
            "Checks the trigger “{trigger}” of {name} each run: {} When it fires, the job sends what the extension reported, after the text below.",
            spec.description
        ),
        (Some(_), None) => format!(
            "Checks the trigger “{trigger}” of {name}, which no longer offers it; runs are refused until it does."
        ),
        (None, _) => format!(
            "Checks the trigger “{trigger}” of the extension {extension}, which is not installed; runs are refused until it is."
        ),
    }
}

/// An argument value as text.
fn show_value(value: &Value) -> String {
    match value {
        Value::Nil => String::new(),
        Value::Bool(flag) => flag.to_string(),
        Value::Int(number) => number.to_string(),
        Value::Float(number) => number.to_string(),
        Value::Str(text) => text.clone(),
        Value::List(_) | Value::Map(_) => format!("{value:?}"),
    }
}

/// Why a trigger argument typed into the form cannot be used.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
enum ArgInputError {
    #[error("{0} is required")]
    Missing(String),
    #[error("{name} must be {}", kind.label())]
    Kind { name: String, kind: ParamKind },
    #[error(
        "{0} takes a list or table; schedule this trigger with `agent206 schedule add --trigger`"
    )]
    Structured(String),
}

/// The arguments the form's fields give a trigger, by its declared kinds.
/// Empty fields are left out.
fn trigger_args(
    params: &[ParamSpec],
    inputs: &std::collections::BTreeMap<String, String>,
) -> Result<Args, ArgInputError> {
    let mut args = Args::new();
    for param in params {
        let text = inputs.get(&param.name).map_or("", |text| text.trim());
        if text.is_empty() {
            if param.required {
                return Err(ArgInputError::Missing(param.name.clone()));
            }
            continue;
        }
        let wrong = || ArgInputError::Kind {
            name: param.name.clone(),
            kind: param.kind,
        };
        let value = match param.kind {
            ParamKind::String => Value::Str(text.to_owned()),
            ParamKind::Integer => Value::Int(text.parse().map_err(|_| wrong())?),
            ParamKind::Number => Value::Float(text.parse().map_err(|_| wrong())?),
            ParamKind::Boolean => Value::Bool(text.parse().map_err(|_| wrong())?),
            ParamKind::Array | ParamKind::Object => {
                return Err(ArgInputError::Structured(param.name.clone()));
            }
        };
        args.insert(param.name.clone(), value);
    }
    Ok(args)
}

/// What the form's job sends.
#[derive(Debug, Clone, PartialEq)]
enum Sends {
    Message,
    Heartbeat,
    Trigger {
        extension: String,
        spec: TriggerSpec,
    },
}
impl Sends {
    /// The choice's value in the "Send" field.
    fn key(&self) -> String {
        match self {
            Self::Message => "message".into(),
            Self::Heartbeat => "heartbeat".into(),
            // Extension ids hold no `/`.
            Self::Trigger { extension, spec } => format!("trigger/{extension}/{}", spec.name),
        }
    }
}

/// What a job can send: a message, a heartbeat check, and each trigger of an
/// enabled extension, labelled with the extension's name.
fn send_choices(plugins: &[PluginSummary]) -> Vec<(Sends, String)> {
    let mut choices = vec![
        (Sends::Message, "A message".to_owned()),
        (
            Sends::Heartbeat,
            "A heartbeat check (HEARTBEAT.md)".to_owned(),
        ),
    ];
    for plugin in plugins.iter().filter(|p| p.enabled) {
        for spec in &plugin.triggers {
            choices.push((
                Sends::Trigger {
                    extension: plugin.id.clone(),
                    spec: spec.clone(),
                },
                format!("A trigger check: {} “{}”", plugin.name, spec.name),
            ));
        }
    }
    choices
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
    let destinations = post_destinations(&snapshot.settings.messengers);
    rsx! { PageFrame { kicker: "Workbench", title: "Messages on a schedule.", description: "Send a conversation a message, a heartbeat check of its HEARTBEAT.md, or what an extension's trigger reports when it fires, once, on an interval or on a cron schedule. Each one runs as if you typed it, with that conversation's permissions at the time.",
        if jobs.is_empty() { EmptyState { compact: true, title: rsx! { "Nothing scheduled" }, body: rsx! { "Scheduled messages appear here, whether you add them or a conversation's model does with your approval." } } }
        for job in jobs {
            JobCard { key: "{job.id}", job: job.clone(), conversation: title(&snapshot, job.thread), destinations: destinations.clone(), plugins: snapshot.plugins.clone() }
        }
        AddJob {}
    } }
}

#[component]
fn JobCard(
    job: Job,
    conversation: Option<String>,
    destinations: Vec<String>,
    plugins: Vec<PluginSummary>,
) -> Element {
    let state = use_context::<State>();
    let zone = TimeZone::system();
    let id = job.id;
    let mut history = use_signal(|| false);
    let deliver_to = job.deliver_to.clone();
    let delivery_value = use_memo(use_reactive!(|deliver_to| Some(
        deliver_to.unwrap_or_else(|| NOWHERE.to_owned())
    )));
    let choices = delivery_choices(&destinations, job.deliver_to.as_deref());
    let replies = if job.kind == JobKind::Heartbeat {
        "Post replies that need attention to"
    } else {
        "Post replies to"
    };
    let author = match job.author {
        JobAuthor::User => "Added by you",
        JobAuthor::Agent { .. } => "Added by the conversation's model, with your approval",
    };
    rsx! { Card {
        CardHeader { CardTitle { "{job.name}" } CardSubtitle { "{describe(&job.schedule, &zone)}" } }
        CardBody {
            div { class: "a206-row",
                if job.enabled { Badge { tone: BadgeTone::Accent, "Active" } } else { Badge { "Paused" } }
                if job.kind == JobKind::Heartbeat { Badge { "Heartbeat check" } }
                if matches!(job.kind, JobKind::Trigger { .. }) { Badge { "Trigger check" } }
                span { class: "a206-muted", "{author}" }
            }
            p { "To " match conversation {
                Some(title) => rsx! { LinkButton { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, href: Route::Chat { id: job.thread, split: None, message: None }.to_string(), "{title}" } },
                None => rsx! { span { class: "a206-muted", "a conversation that no longer exists" } },
            } }
            match &job.kind {
                JobKind::Message => rsx! { blockquote { class: "a206-quote", "{job.text}" } },
                JobKind::Heartbeat => rsx! { p { class: "a206-muted", "Sends the checklist in HEARTBEAT.md at the conversation's project root, read when it runs. A reply of exactly HEARTBEAT_OK means nothing needed attention." } },
                JobKind::Trigger { extension, trigger, args } => rsx! {
                    p { class: "a206-muted", "{trigger_summary(&plugins, extension, trigger)}" }
                    if !args.is_empty() {
                        DescriptionList { for (name, value) in args.iter() { DescriptionItem { key: "{name}", term: rsx! { "{name}" }, "{show_value(value)}" } } }
                    }
                    if !job.text.is_empty() { blockquote { class: "a206-quote", "{job.text}" } }
                },
            }
            p { class: "a206-muted", match (job.enabled, job.next) {
                (true, Some(next)) => format!("Next: {}", show_time(next, &zone)),
                (false, _) => "Paused; resuming skips the times it missed.".to_owned(),
                (true, None) => "It will not run again.".to_owned(),
            } }
            if let Some(last) = &job.last {
                { let (tone, label) = outcome(&last.outcome, &job.kind); rsx! {
                    div { class: "a206-row", span { class: "a206-muted", "Latest run {show_time(last.at, &zone)}" } Badge { tone, "{label}" } }
                } }
                if let JobOutcome::Refused(report) = &last.outcome { ErrorNotice { report: report.clone(), title: "The latest run was not sent".to_string() } }
                if let Some(Delivery::Failed(report)) = &last.delivery { ErrorNotice { report: report.clone(), title: "The latest reply was not posted".to_string() } }
            }
            Field { label: rsx! { "{replies}" }, hint: rsx! { "A webhook, or an allowed chat of a connected messenger, from Messengers. Each completed run's reply is also posted there." },
                Select { label: name!("Messenger the replies are posted to"), value: ReadSignal::from(delivery_value), options: choices,
                    onchange: move |to: String| dispatch(state, Command::SetJobDelivery { id, to: Some(to).filter(|to| to != NOWHERE) })
                }
            }
            if history() { JobHistory { job: job.clone() } }
        }
        CardFooter {
            Button { variant: ButtonVariant::Secondary, onclick: move |_| dispatch(state, Command::SetJobEnabled { id, enabled: !job.enabled }), if job.enabled { "Pause" } else { "Resume" } }
            Button { variant: ButtonVariant::Secondary, onclick: move |_| dispatch(state, Command::RunJob { id }), "Run now" }
            Button { variant: ButtonVariant::Ghost, onclick: move |_| history.toggle(), if history() { "Hide run history" } else { "Run history" } }
            Button { variant: ButtonVariant::Ghost, onclick: move |_| dispatch(state, Command::RemoveJob { id }), "Remove" }
        }
    } }
}

async fn load_runs(id: Uuid) -> Result<Vec<JobRun>, ErrorReport> {
    match call(Command::JobRuns { id }).await? {
        Reply::JobRuns(runs) => Ok(runs),
        Reply::Error(report) => Err(report),
        _ => Err(ErrorReport::new(
            ErrorKind::Protocol,
            "gateway did not list the job's runs",
        )),
    }
}

/// A job's latest runs, newest first, as the gateway keeps them.
#[component]
fn JobHistory(job: Job) -> Element {
    let id = job.id;
    let mut runs = use_signal(|| None::<Vec<JobRun>>);
    let mut failure = use_signal(|| None::<ErrorReport>);
    // Only the newest request's reply is shown, so a slow earlier one never
    // replaces it.
    let mut latest = use_signal(|| 0_u64);
    let mut reload = move || {
        let request = latest() + 1;
        latest.set(request);
        spawn(async move {
            let loaded = load_runs(id).await;
            if latest() != request {
                return;
            }
            match loaded {
                Ok(list) => {
                    failure.set(None);
                    runs.set(Some(list));
                }
                Err(report) => failure.set(Some(report)),
            }
        });
    };
    // Each change to the latest run (a new run, or how one ended) is in the
    // history too, as is an earlier run ending after a newer one started.
    let (last, settled) = (job.last.clone(), job.history);
    use_effect(use_reactive!(|last, settled| {
        let _ = (last, settled);
        reload();
    }));
    rsx! { div { class: "a206-stack",
        if let Some(report) = failure() { ErrorNotice { report, title: "Run history unavailable".to_string(), dismiss: move |_| failure.set(None) } }
        match runs() {
            None if failure().is_none() => rsx! { p { class: "a206-muted", "Loading run history…" } },
            None => rsx! {},
            Some(list) => rsx! { RunList { runs: list, kind: job.kind.clone() } },
        }
        div { Button { size: ButtonSize::Sm, variant: ButtonVariant::Ghost, onclick: move |_| reload(), "Refresh run history" } }
    } }
}

#[component]
fn RunList(runs: Vec<JobRun>, kind: JobKind) -> Element {
    let zone = TimeZone::system();
    if runs.is_empty() {
        return rsx! { p { class: "a206-muted", "It has not run yet." } };
    }
    rsx! {
        p { class: "a206-muted", "The gateway keeps the latest {MAX_JOB_RUNS} runs of each scheduled message, newest first." }
        ul { class: "a206-stack",
            for run in runs {
                li { key: "{run.id}",
                    { let (tone, label) = outcome(&run.outcome, &kind); rsx! {
                        div { class: "a206-row",
                            span { "{show_time(run.at, &zone)}" }
                            Badge { tone, "{label}" }
                            if let Some(ended) = run.ended { span { class: "a206-muted", "took {format_interval(ended.saturating_sub(run.at))}" } }
                            if let Some(posted) = &run.delivery { { let (tone, label) = delivery(posted); rsx! { Badge { tone, "{label}" } } } }
                        }
                    } }
                    if let JobOutcome::Refused(report) = &run.outcome { ErrorNotice { report: report.clone(), title: "Not sent".to_string() } }
                    if let Some(Delivery::Failed(report)) = &run.delivery { ErrorNotice { report: report.clone(), title: "Reply not posted".to_string() } }
                }
            }
        }
    }
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
) -> Option<Result<Schedule, ScheduleInputError>> {
    match kind {
        Kind::Once if date.trim().is_empty() || time.trim().is_empty() => None,
        Kind::Once => Some(parse_local(date, time, zone).map(|at| Schedule::At { at })),
        Kind::Every if every.trim().is_empty() => None,
        // The gateway counts the interval from when it accepts the job, by
        // its own clock.
        Kind::Every => Some(parse_interval(every).map(|seconds| Schedule::Every {
            seconds,
            anchor: None,
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
    let mut sends = use_signal(|| Sends::Message);
    let mut inputs = use_signal(std::collections::BTreeMap::<String, String>::new);
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
    let snapshot = state.data.read();
    let conversations = snapshot
        .threads
        .iter()
        .map(|t| SelectChoice::new(t.id.to_string(), t.title.clone()))
        .collect::<Vec<_>>();
    let choices = send_choices(&snapshot.plugins);
    drop(snapshot);
    let schedule = form_schedule(kind(), &date(), &time(), &zone(), &every(), &cron());
    let invalid = match &schedule {
        Some(Err(error)) => Some(error.to_string()),
        _ => None,
    };
    let target = thread().and_then(|id| id.parse::<Uuid>().ok());
    let valid = matches!(schedule, Some(Ok(_)));
    let job_kind = match sends() {
        Sends::Message => Ok(JobKind::Message),
        Sends::Heartbeat => Ok(JobKind::Heartbeat),
        Sends::Trigger { extension, spec } => {
            trigger_args(&spec.params, &inputs()).map(|args| JobKind::Trigger {
                extension,
                trigger: spec.name,
                args,
            })
        }
    };
    let ready = !name().trim().is_empty()
        && (sends() != Sends::Message || !text().trim().is_empty())
        && target.is_some()
        && valid
        && job_kind.is_ok();
    let sends_key = use_memo(move || Some(sends().key()));
    let kind_label = use_memo(move || Some(kind().label().to_owned()));
    let options = choices
        .iter()
        .map(|(choice, label)| SelectChoice::new(choice.key(), label.clone()))
        .collect::<Vec<_>>();
    let arg_error = job_kind.as_ref().err().map(ToString::to_string);
    rsx! { Card { variant: CardVariant::Elevated,
        CardHeader { CardTitle { "Schedule a message" } CardSubtitle { "The gateway checks the schedule and the conversation when you add it." } }
        CardBody { div { class: "a206-grid",
            Field { label: rsx! { "Name" }, Input { value: ReadSignal::from(name), oninput: move |v| name.set(v), placeholder: "Morning summary" } }
            Field { label: rsx! { "Conversation" }, Select { label: name!("Conversation to message"), value: ReadSignal::from(thread), placeholder: "Choose a conversation", options: conversations, onchange: move |id: String| thread.set(Some(id)) } }
            Field { label: rsx! { "Send" }, hint: rsx! { "Triggers come from enabled extensions." }, Select { label: name!("What the job sends"), value: ReadSignal::from(sends_key), options,
                onchange: move |key: String| if let Some((choice, _)) = choices.iter().find(|(c, _)| c.key() == key) {
                    sends.set(choice.clone());
                    inputs.set(Default::default());
                }
            } }
            match sends() {
                Sends::Heartbeat => rsx! {
                    p { class: "a206-muted", "The conversation's project needs a HEARTBEAT.md checklist, and the conversation needs permission to read the project." }
                },
                Sends::Message => rsx! {
                    Field { label: rsx! { "Message" }, Textarea { value: ReadSignal::from(text), oninput: move |v| text.set(v), rows: "3" } }
                },
                Sends::Trigger { spec, .. } => rsx! {
                    p { class: "a206-muted", "{spec.description} Each run checks it with the conversation's permissions at the time; the extension also needs the permissions the trigger uses. A run that finds nothing sends nothing." }
                    for param in spec.params.clone() {
                        ArgField { key: "{param.name}", param: param.clone(), value: inputs().get(&param.name).cloned().unwrap_or_default(),
                            oninput: move |v: String| { inputs.write().insert(param.name.clone(), v); }
                        }
                    }
                    if let Some(error) = arg_error.clone() { p { class: "a206-muted", role: "status", "{error}" } }
                    Field { label: rsx! { "When it fires" }, hint: rsx! { "Optional. Sent before the extension's report, such as what to do about it." }, Textarea { value: ReadSignal::from(text), oninput: move |v| text.set(v), rows: "2" } }
                },
            }
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
                    Field { label: rsx! { "Interval" }, hint: rsx! { "Such as 90m, 6h or 1d; at least a minute. Counted from when it is scheduled." }, error: invalid.clone().map(|e| rsx! { "{e}" }), Input { value: ReadSignal::from(every), oninput: move |v| every.set(v), placeholder: "1d" } }
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
            let Some(Ok(schedule)) = form_schedule(kind(), &date(), &time(), &zone(), &every(), &cron()) else { return };
            let Some(thread) = target else { return };
            let Ok(kind) = job_kind.clone() else { return };
            let text = if kind == JobKind::Heartbeat { String::new() } else { text() };
            dispatch(state, Command::ScheduleJob { name: name().trim().to_owned(), thread, text, schedule, kind });
        }, "Schedule" } }
    } }
}

/// A field for one trigger parameter, labelled with its declaration.
#[component]
fn ArgField(param: ParamSpec, value: String, oninput: EventHandler<String>) -> Element {
    let value = use_memo(use_reactive!(|value| value));
    let need = if param.required { ", required" } else { "" };
    rsx! { Field { label: rsx! { "{param.name}" }, hint: rsx! { "{param.description} ({param.kind.label()}{need})" },
        Input { value: ReadSignal::from(value), oninput: move |v| oninput.call(v) }
    } }
}

#[cfg(test)]
mod tests;
