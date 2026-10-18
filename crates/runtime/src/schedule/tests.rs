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
fn relative_schedules_are_placed_at_acceptance_and_not_before() {
    let now = time();
    let seconds = 1 + time() % MAX_INTERVAL;
    let delay = Schedule::In { seconds };
    let unanchored = Schedule::Every {
        seconds,
        anchor: None,
    };
    for relative in [&delay, &unanchored] {
        assert!(matches!(
            next_after(relative, now),
            Err(ScheduleError::Unresolved)
        ));
    }
    assert_eq!(
        resolve(delay, now).unwrap(),
        Schedule::At { at: now + seconds }
    );
    assert_eq!(
        resolve(unanchored, now).unwrap(),
        Schedule::Every {
            seconds,
            anchor: Some(now)
        }
    );
    assert!(matches!(
        resolve(Schedule::In { seconds: 0 }, now),
        Err(ScheduleError::Delay)
    ));
    // Placed schedules are kept as they are.
    let cron = utc("0 9 * * *");
    assert_eq!(resolve(cron.clone(), now).unwrap(), cron);
}

#[test]
fn intervals_stay_on_their_anchored_grid() {
    let anchor = time();
    let seconds = MIN_INTERVAL + time() % 86_400;
    let schedule = Schedule::Every {
        seconds,
        anchor: Some(anchor),
    };
    let steps = time() % 1000;
    let late = time() % seconds;
    assert_eq!(
        next_after(&schedule, anchor + steps * seconds + late).unwrap(),
        Some(anchor + (steps + 1) * seconds)
    );
    assert_eq!(next_after(&schedule, anchor - 1).unwrap(), Some(anchor));
    for seconds in [MIN_INTERVAL - 1, MAX_INTERVAL + 1] {
        assert!(matches!(
            next_after(
                &Schedule::Every {
                    seconds,
                    anchor: Some(anchor)
                },
                anchor
            ),
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
        kind: a206_core::JobKind::Message,
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
        history: 0,
        deliver_to: None,
    };
    let message =
        |text: &str, at: u64| Message::new(Role::User, text.into(), at, CodingBackend::default());
    let text = Id::new_v4().to_string();
    let seconds = MIN_INTERVAL + time() % 86_400;
    let every = Schedule::Every {
        seconds,
        anchor: Some(started),
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

    // Sent, then another message started a later run that is still going:
    // the job's run ended before it, with a reply or without one.
    let mut answered = interrupted.clone();
    answered.id = Id::new_v4();
    answered.messages.push(Message::new(
        Role::Assistant,
        Id::new_v4().to_string(),
        started,
        CodingBackend::default(),
    ));
    answered
        .messages
        .push(message(&Id::new_v4().to_string(), started + 1));
    let mut unanswered = interrupted.clone();
    unanswered.id = Id::new_v4();
    unanswered.state = RunState::Complete;
    unanswered
        .messages
        .push(message(&Id::new_v4().to_string(), started + 1));

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
    jobs.push(job(answered.id, &text, Schedule::At { at: started }, None));
    jobs.push(job(
        unanswered.id,
        &text,
        Schedule::At { at: started },
        None,
    ));
    recover(
        &mut jobs,
        &[complete, interrupted, unsent, answered, unanswered],
    );

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
    assert_eq!(outcome(&jobs[6]), JobOutcome::Complete);
    assert_eq!(outcome(&jobs[7]), JobOutcome::Failed);
    assert!(
        jobs[6..]
            .iter()
            .all(|job| !job.enabled && job.next.is_none())
    );
}

#[test]
fn a_restart_settles_a_heartbeat_by_its_reply() {
    use a206_core::{
        CodingBackend, Id, Job, JobKind, JobRun, Message, Mode, Thread,
        schedule::{HEARTBEAT_OK, heartbeat_message},
    };
    let started = time();
    let heartbeat = |thread: Id| Job {
        kind: JobKind::Heartbeat,
        id: Id::new_v4(),
        name: Id::new_v4().to_string(),
        thread,
        text: String::new(),
        schedule: Schedule::At { at: started },
        enabled: false,
        created: started - 1,
        next: None,
        last: Some(JobRun::start(started)),
        author: Default::default(),
        history: 0,
        deliver_to: None,
    };
    // A conversation that received run `run`'s check, answered with `reply`.
    let conversation = |run: Id, reply: &str| {
        let mut thread = Thread::new(Id::new_v4().to_string(), Mode::Chat);
        let checklist = heartbeat_message(&Id::new_v4().to_string(), run);
        for (role, text) in [(Role::User, checklist.as_str()), (Role::Assistant, reply)] {
            thread.messages.push(Message::new(
                role,
                text.into(),
                started,
                CodingBackend::default(),
            ));
        }
        thread.state = RunState::Complete;
        thread
    };
    let run = |job: &Job| job.last.as_ref().unwrap().id;
    let mut jobs: Vec<Job> = (0..3).map(|_| heartbeat(Id::new_v4())).collect();
    let quiet = conversation(run(&jobs[0]), &format!("{HEARTBEAT_OK}\n"));
    let alert = conversation(run(&jobs[1]), &format!("{HEARTBEAT_OK} {}", Id::new_v4()));
    // An earlier run's check in the same second is not this run's: the run
    // never sent its own, so it is due again.
    let unsent = conversation(Id::new_v4(), HEARTBEAT_OK);
    for (job, thread) in jobs.iter_mut().zip([&quiet, &alert, &unsent]) {
        job.thread = thread.id;
    }
    recover(&mut jobs, &[quiet, alert, unsent]);
    let outcome = |job: &Job| job.last.as_ref().unwrap().outcome.clone();
    assert_eq!(outcome(&jobs[0]), JobOutcome::Quiet);
    assert_eq!(outcome(&jobs[1]), JobOutcome::Complete);
    assert!(matches!(outcome(&jobs[2]), JobOutcome::Refused(_)));
    assert_eq!(jobs[2].next, Some(started));
}
