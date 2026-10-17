use super::*;

/// A Unix time in this century, generated at runtime.
fn time() -> u64 {
    1_000_000_000 + (uuid::Uuid::new_v4().as_u128() % 3_000_000_000) as u64
}
fn utc(expression: &str) -> Schedule {
    Schedule::Cron {
        expression: expression.into(),
        zone: Some("UTC".into()),
    }
}

#[test]
fn one_time_jobs_are_due_once_and_never_in_the_past() {
    let at = time();
    let schedule = Schedule::At { at };
    assert_eq!(next_after(&schedule, at - 1).unwrap(), Some(at));
    assert_eq!(next_after(&schedule, at).unwrap(), None);
    assert!(matches!(first(&schedule, at), Err(ScheduleError::Past)));
    assert_eq!(first(&schedule, at - 1).unwrap(), at);
}

#[test]
fn intervals_stay_on_their_anchored_grid() {
    let anchor = time();
    let seconds = MIN_INTERVAL + time() % 86_400;
    let schedule = Schedule::Every { seconds, anchor };
    let steps = time() % 1000;
    let late = time() % seconds;
    assert_eq!(
        next_after(&schedule, anchor + steps * seconds + late).unwrap(),
        Some(anchor + (steps + 1) * seconds)
    );
    assert_eq!(next_after(&schedule, anchor - 1).unwrap(), Some(anchor));
    for seconds in [MIN_INTERVAL - 1, MAX_INTERVAL + 1] {
        assert!(matches!(
            next_after(&Schedule::Every { seconds, anchor }, anchor),
            Err(ScheduleError::Interval)
        ));
    }
}

#[test]
fn cron_expressions_follow_their_zone_with_optional_seconds() {
    let now = time();
    assert_eq!(
        next_after(&utc("0 * * * *"), now).unwrap(),
        Some((now / 3600 + 1) * 3600)
    );
    assert_eq!(
        next_after(&utc("*/30 * * * * *"), now).unwrap(),
        Some((now / 30 + 1) * 30)
    );
    assert!(matches!(
        next_after(&utc(&uuid::Uuid::new_v4().to_string()), now),
        Err(ScheduleError::Expression(_))
    ));
    let zone = uuid::Uuid::new_v4().simple().to_string();
    assert!(matches!(
        next_after(
            &Schedule::Cron {
                expression: "0 * * * *".into(),
                zone: Some(zone),
            },
            now
        ),
        Err(ScheduleError::Zone { .. })
    ));
}

#[test]
fn a_restart_sends_an_interrupted_message_once() {
    use a206_core::{CodingBackend, Id, Job, JobRun, Message, Mode, Thread};
    let started = time();
    let job = |thread: Id, text: &str, schedule: Schedule, next: Option<u64>| Job {
        id: Id::new_v4(),
        name: Id::new_v4().to_string(),
        thread,
        text: text.into(),
        schedule,
        enabled: next.is_some(),
        created: started - 1,
        next,
        last: Some(JobRun::start(started)),
        author: Default::default(),
    };
    let message =
        |text: &str, at: u64| Message::new(Role::User, text.into(), at, CodingBackend::default());
    let text = Id::new_v4().to_string();
    let seconds = MIN_INTERVAL + time() % 86_400;
    let every = Schedule::Every {
        seconds,
        anchor: started,
    };

    // Sent before the restart, and the run finished or was interrupted.
    let mut complete = Thread::new(text.clone(), Mode::Chat);
    complete.messages.push(message(&text, started));
    complete.state = RunState::Complete;
    let mut interrupted = complete.clone();
    interrupted.id = Id::new_v4();
    interrupted.state = RunState::Running;
    // Never sent: only an older copy of the text is in the conversation.
    let mut unsent = Thread::new(text.clone(), Mode::Chat);
    unsent.messages.push(message(&text, started - 1));
    unsent.state = RunState::Complete;

    let later = started + seconds;
    let mut jobs = vec![
        job(complete.id, &text, every.clone(), Some(later)),
        job(interrupted.id, &text, every.clone(), Some(later)),
        job(unsent.id, &text, every.clone(), Some(later)),
        job(unsent.id, &text, Schedule::At { at: started }, None),
        job(Id::new_v4(), &text, every, Some(later)),
    ];
    let mut settled = jobs[0].clone();
    settled.last = Some(JobRun {
        outcome: JobOutcome::Complete,
        ..JobRun::start(started)
    });
    jobs.push(settled.clone());
    recover(&mut jobs, &[complete, interrupted, unsent]);

    let outcome = |job: &Job| job.last.as_ref().unwrap().outcome.clone();
    assert_eq!(outcome(&jobs[0]), JobOutcome::Complete);
    assert_eq!(jobs[0].next, Some(later));
    assert_eq!(outcome(&jobs[1]), JobOutcome::Failed);
    assert_eq!(jobs[1].next, Some(later));
    for unsent in &jobs[2..5] {
        assert!(
            matches!(outcome(unsent), JobOutcome::Refused(ref r) if r.kind == ErrorKind::Unavailable)
        );
        assert!(unsent.enabled);
        assert_eq!(unsent.next, Some(started));
    }
    assert_eq!(jobs[5], settled, "a finished run is left alone");
}
