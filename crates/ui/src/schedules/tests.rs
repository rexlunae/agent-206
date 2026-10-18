use super::*;
use a206_core::{ErrorKind, ErrorReport, JobKind, JobRun, Mode, Thread};
use dioxus::history::{History, MemoryHistory};
use std::rc::Rc;

/// A value the test does not choose by hand.
fn arbitrary(range: u64) -> u64 {
    (Uuid::new_v4().as_u128() % u128::from(range)) as u64
}

fn job(thread: Uuid) -> Job {
    let at = 86_400 + arbitrary(4_000_000_000);
    Job {
        kind: a206_core::JobKind::Message,
        id: Uuid::new_v4(),
        name: format!("job-{}", Uuid::new_v4().simple()),
        thread,
        text: format!("text-{}", Uuid::new_v4().simple()),
        schedule: Schedule::At { at },
        enabled: true,
        created: at - 1,
        next: Some(at),
        last: None,
        author: JobAuthor::User,
        history: 0,
        deliver_to: None,
    }
}

#[derive(Clone, Debug, PartialEq, Routable)]
enum Fixture {
    #[route("/")]
    Page {},
}

#[component]
fn Page() -> Element {
    let snapshot = use_context::<Snapshot>();
    let state = crate::route::tests::loaded_state(snapshot);
    use_context_provider(|| state);
    rsx! { Schedules {} }
}

fn render(snapshot: Snapshot) -> String {
    fn app() -> Element {
        rsx! { Router::<Fixture> {} }
    }
    let mut dom = VirtualDom::new(app);
    dom.provide_root_context(snapshot);
    dom.provide_root_context(Rc::new(MemoryHistory::default()) as Rc<dyn History>);
    dom.rebuild_in_place();
    dioxus_ssr::render(&dom)
}

#[test]
fn jobs_show_their_conversation_author_state_and_refusal() {
    let thread = Thread::new(format!("t-{}", Uuid::new_v4().simple()), Mode::Chat);
    let refusal = format!("busy-{}", Uuid::new_v4().simple());
    let mut refused = job(thread.id);
    refused.last = Some(JobRun {
        id: Uuid::new_v4(),
        at: refused.created,
        outcome: JobOutcome::Refused(ErrorReport::new(ErrorKind::Conflict, refusal.clone())),
        ended: None,
        delivery: None,
    });
    let mut paused = job(thread.id);
    paused.enabled = false;
    paused.next = None;
    paused.author = JobAuthor::Agent { thread: thread.id };
    let orphan = job(Uuid::new_v4());
    let mut snapshot = crate::empty_snapshot();
    snapshot.threads = vec![thread.clone()];
    snapshot.jobs = vec![paused.clone(), refused.clone(), orphan.clone()];
    let html = render(snapshot);
    for job in [&refused, &paused, &orphan] {
        assert!(
            html.contains(&job.name) && html.contains(&job.text),
            "{html}"
        );
    }
    assert!(
        html.contains(&format!("/conversations/{}", thread.id)),
        "{html}"
    );
    assert_eq!(
        html.matches(&thread.title).count(),
        3,
        "two links and one form choice"
    );
    assert!(html.contains("a conversation that no longer exists"));
    assert!(
        html.contains("Not sent") && html.contains(&refusal),
        "{html}"
    );
    assert!(html.contains("with your approval"), "{html}");
    assert_eq!(html.matches(">Resume<").count(), 1, "{html}");
    assert_eq!(html.matches(">Pause<").count(), 2, "{html}");
    // A paused job is listed after every pending one.
    assert!(html.find(&paused.name) > html.find(&refused.name));
    assert!(html.find(&paused.name) > html.find(&orphan.name));
}

#[test]
fn an_empty_page_says_so_and_offers_the_form() {
    let html = render(crate::empty_snapshot());
    assert!(html.contains("Nothing scheduled"), "{html}");
    assert!(html.contains("Schedule a message"), "{html}");
}

#[test]
fn the_form_builds_exactly_the_schedule_it_shows() {
    let seconds = 60 + arbitrary(1_000_000);
    let blank = |kind| form_schedule(kind, "", "", "", "", "");
    for kind in Kind::ALL {
        assert!(blank(kind).is_none(), "nothing typed is not an error");
    }
    let every = form_schedule(Kind::Every, "", "", "", &format_interval(seconds), "");
    assert_eq!(
        every.unwrap().unwrap(),
        Schedule::Every {
            seconds,
            anchor: None
        }
    );
    assert!(matches!(
        form_schedule(Kind::Every, "", "", "", "soon", ""),
        Some(Err(ScheduleInputError::Duration { .. }))
    ));
    let at = 86_400 + arbitrary(4_000_000_000);
    let civil = Timestamp::from_second(at as i64)
        .unwrap()
        .to_zoned(TimeZone::UTC)
        .datetime();
    let once = form_schedule(
        Kind::Once,
        &civil.date().to_string(),
        &civil.time().strftime("%H:%M").to_string(),
        "UTC",
        "",
        "",
    );
    assert_eq!(once.unwrap().unwrap(), Schedule::At { at: at - at % 60 });
    let expression = format!("{} 9 * * *", arbitrary(60));
    for (zone, expected) in [("", None), (" UTC ", Some("UTC".to_owned()))] {
        assert_eq!(
            form_schedule(Kind::Cron, "", "", zone, "", &expression)
                .unwrap()
                .unwrap(),
            Schedule::Cron {
                expression: expression.clone(),
                zone: expected
            }
        );
    }
}

#[test]
fn heartbeat_checks_show_their_checklist_and_whether_anything_needed_attention() {
    let thread = Thread::new(format!("t-{}", Uuid::new_v4().simple()), Mode::Chat);
    let heartbeat = |outcome: JobOutcome| {
        let mut job = job(thread.id);
        job.kind = JobKind::Heartbeat;
        job.text = String::new();
        job.last = Some(JobRun {
            id: Uuid::new_v4(),
            at: job.created,
            outcome,
            ended: None,
            delivery: None,
        });
        job
    };
    let mut snapshot = crate::empty_snapshot();
    snapshot.threads = vec![thread.clone()];
    snapshot.jobs = vec![heartbeat(JobOutcome::Quiet)];
    let quiet = render(snapshot.clone());
    assert!(
        quiet.contains("Heartbeat check") && quiet.contains("HEARTBEAT.md"),
        "{quiet}"
    );
    assert!(quiet.contains("Nothing needed attention"), "{quiet}");
    assert!(!quiet.contains("Needs attention"), "{quiet}");
    snapshot.jobs = vec![heartbeat(JobOutcome::Complete)];
    let alert = render(snapshot.clone());
    assert!(alert.contains("Needs attention"), "{alert}");
    // A message job that completes is simply complete.
    let mut message = job(thread.id);
    message.last = heartbeat(JobOutcome::Complete).last;
    snapshot.jobs = vec![message];
    let plain = render(snapshot);
    assert!(
        plain.contains(">Complete<") && !plain.contains("Needs attention"),
        "{plain}"
    );
}

fn render_runs(runs: Vec<JobRun>) -> String {
    #[derive(Props, Clone, PartialEq)]
    struct Fixture {
        runs: Vec<JobRun>,
    }
    fn app(props: Fixture) -> Element {
        rsx! { RunList { runs: props.runs, kind: JobKind::Message } }
    }
    let mut dom = VirtualDom::new_with_props(app, Fixture { runs });
    dom.rebuild_in_place();
    dioxus_ssr::render(&dom)
}

#[test]
fn run_history_shows_each_run_and_marks_unrecorded_ends() {
    assert!(render_runs(vec![]).contains("It has not run yet."));
    let at = 86_400 + arbitrary(4_000_000_000);
    let took = 1 + arbitrary(3_000);
    let refusal = format!("busy-{}", Uuid::new_v4().simple());
    let current = JobRun::start(at + took + 1);
    let unrecorded = JobRun {
        outcome: JobOutcome::Unrecorded,
        ..JobRun::start(at)
    };
    let refused = JobRun {
        outcome: JobOutcome::Refused(ErrorReport::new(ErrorKind::Conflict, refusal.clone())),
        ended: Some(at + took),
        ..JobRun::start(at)
    };
    let html = render_runs(vec![current, refused, unrecorded]);
    assert_eq!(
        html.matches("Ended; result not recorded").count(),
        1,
        "{html}"
    );
    assert!(html.contains("Running"), "{html}");
    assert!(html.contains(&refusal), "{html}");
    assert!(
        html.contains(&format!("took {}", format_interval(took))),
        "{html}"
    );
}

#[test]
fn run_history_shows_whether_each_reply_was_posted() {
    let at = 86_400 + arbitrary(4_000_000_000);
    let parts = 2 + arbitrary(8);
    let refusal = format!("gone-{}", Uuid::new_v4().simple());
    let run = |delivery| JobRun {
        outcome: JobOutcome::Complete,
        delivery: Some(delivery),
        ..JobRun::start(at)
    };
    let html = render_runs(vec![
        run(Delivery::Sent { parts: 1 }),
        run(Delivery::Sent { parts }),
        run(Delivery::Failed(ErrorReport::new(
            ErrorKind::NotFound,
            refusal.clone(),
        ))),
        run(Delivery::Posting),
        run(Delivery::Unrecorded),
        JobRun::start(at),
    ]);
    assert_eq!(html.matches("Reply posted<").count(), 1, "{html}");
    assert_eq!(html.matches("Posting reply").count(), 1, "{html}");
    assert_eq!(html.matches("it may not have arrived").count(), 1, "{html}");
    assert!(
        html.contains(&format!("Reply posted in {parts} parts")),
        "{html}"
    );
    assert_eq!(html.matches("Reply not posted").count(), 2, "{html}");
    assert!(html.contains(&refusal), "{html}");
}

#[test]
fn a_job_offers_each_configured_destination_and_keeps_a_withdrawn_one() {
    let (hook, bot, chat, gone) = (
        format!("h-{}", Uuid::new_v4().simple()),
        format!("b-{}", Uuid::new_v4().simple()),
        format!("c-{}", Uuid::new_v4().simple()),
        format!("g-{}", Uuid::new_v4().simple()),
    );
    let destinations = vec![hook.clone(), format!("{bot}/{chat}")];
    let values = |choices: Vec<SelectChoice>| {
        choices
            .into_iter()
            .map(|c| (c.value, c.label))
            .collect::<Vec<_>>()
    };
    let offered = values(delivery_choices(&destinations, Some(&hook)));
    assert_eq!(offered.len(), 3, "{offered:?}");
    assert_eq!(offered[0].0, NOWHERE);
    assert_eq!(offered[2].0, format!("{bot}/{chat}"));
    let kept = values(delivery_choices(&destinations, Some(&gone)));
    assert_eq!(kept.len(), 4, "{kept:?}");
    assert_eq!(kept[3].0, gone);
    assert!(kept[3].1.contains("no longer configured"), "{kept:?}");
}
