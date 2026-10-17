use super::*;
use a206_core::{ErrorKind, ErrorReport, JobRun, Mode, Thread};
use dioxus::history::{History, MemoryHistory};
use std::rc::Rc;

/// A value the test does not choose by hand.
fn arbitrary(range: u64) -> u64 {
    (Uuid::new_v4().as_u128() % u128::from(range)) as u64
}

fn job(thread: Uuid) -> Job {
    let at = 86_400 + arbitrary(4_000_000_000);
    Job {
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
    let now = arbitrary(4_000_000_000);
    let seconds = 60 + arbitrary(1_000_000);
    let blank = |kind| form_schedule(kind, "", "", "", "", "", now);
    for kind in Kind::ALL {
        assert!(blank(kind).is_none(), "nothing typed is not an error");
    }
    let every = form_schedule(Kind::Every, "", "", "", &format_interval(seconds), "", now);
    assert_eq!(
        every.unwrap().unwrap(),
        Schedule::Every {
            seconds,
            anchor: now
        }
    );
    assert!(matches!(
        form_schedule(Kind::Every, "", "", "", "soon", "", now),
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
        now,
    );
    assert_eq!(once.unwrap().unwrap(), Schedule::At { at: at - at % 60 });
    let expression = format!("{} 9 * * *", arbitrary(60));
    for (zone, expected) in [("", None), (" UTC ", Some("UTC".to_owned()))] {
        assert_eq!(
            form_schedule(Kind::Cron, "", "", zone, "", &expression, now)
                .unwrap()
                .unwrap(),
            Schedule::Cron {
                expression: expression.clone(),
                zone: expected
            }
        );
    }
}
