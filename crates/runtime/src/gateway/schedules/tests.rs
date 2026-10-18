use super::*;

async fn send(g: &Gateway, body: Command) -> Reply {
    g.request(Request {
        id: Id::new_v4(),
        body,
    })
    .await
    .body
}
fn nonce() -> String {
    Id::new_v4().simple().to_string()
}
async fn thread(g: &Gateway) -> Id {
    let Reply::Thread(thread) = send(
        g,
        Command::CreateThread {
            title: nonce(),
            mode: Mode::Chat,
        },
    )
    .await
    else {
        panic!("thread created");
    };
    thread.id
}
async fn schedule(g: &Gateway, thread: Id, schedule: Schedule) -> Reply {
    send(
        g,
        Command::ScheduleJob {
            kind: a206_core::JobKind::Message,
            name: nonce(),
            thread,
            text: nonce(),
            schedule,
        },
    )
    .await
}
fn job(g: &Gateway, id: Id) -> Job {
    g.snapshot()
        .unwrap()
        .jobs
        .into_iter()
        .find(|job| job.id == id)
        .unwrap()
}
/// The job's latest run once it has ended.
async fn ended(g: &Gateway, id: Id) -> JobRun {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match job(g, id).last {
                Some(run) if run.outcome != JobOutcome::Running => return run,
                _ => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .expect("the job's run ended")
}
/// A Unix time in this century, generated at runtime.
fn arbitrary() -> u64 {
    1_000_000_000 + (Id::new_v4().as_u128() % 3_000_000_000) as u64
}
/// Times the gateway reads the live clock for are chosen so that only a
/// clock step of many years could change a test's outcome.
const YEAR: u64 = 366 * 86_400;
fn every(seconds: u64) -> Schedule {
    Schedule::Every {
        seconds,
        anchor: Some(arbitrary() % 1_000_000_000),
    }
}
fn kind(reply: Reply) -> ErrorKind {
    match reply {
        Reply::Error(report) => report.kind,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn unusable_jobs_are_refused() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let thread = thread(&g).await;
    let past = Schedule::At {
        at: 1 + arbitrary() % 1000,
    };
    assert_eq!(
        kind(schedule(&g, thread, past).await),
        ErrorKind::InvalidRequest
    );
    let fast = every(schedule::MIN_INTERVAL - 1);
    assert_eq!(
        kind(schedule(&g, thread, fast).await),
        ErrorKind::InvalidRequest
    );
    let nowhere = schedule(&g, Id::new_v4(), every(schedule::MIN_INTERVAL)).await;
    assert_eq!(kind(nowhere), ErrorKind::NotFound);
    let unnamed = send(
        &g,
        Command::ScheduleJob {
            kind: a206_core::JobKind::Message,
            name: " ".into(),
            thread,
            text: nonce(),
            schedule: every(schedule::MIN_INTERVAL),
        },
    )
    .await;
    assert_eq!(kind(unnamed), ErrorKind::InvalidRequest);
    assert!(g.snapshot().unwrap().jobs.is_empty());
}

#[tokio::test]
async fn due_jobs_run_once_per_wake_and_keep_their_grid() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let thread = thread(&g).await;
    let seconds = schedule::MIN_INTERVAL + arbitrary() % 1000;
    let Reply::Job(created) = schedule(&g, thread, every(seconds)).await else {
        panic!("job scheduled");
    };
    let first = created.next.unwrap();
    assert!(first > created.created);

    // Not yet due: nothing runs, and the scheduler learns when to wake.
    assert_eq!(g.run_due_jobs(first - 1).unwrap(), Some(first));
    assert_eq!(job(&g, created.id).last, None);

    // Several times passed (the gateway was stopped): one run, then the
    // next time on the grid after now.
    let missed = 2 + arbitrary() % 5;
    let late = first + missed * seconds + 1;
    assert_eq!(
        g.run_due_jobs(late).unwrap(),
        Some(first + (missed + 1) * seconds)
    );
    let run = ended(&g, created.id).await;
    assert_eq!(run.at, late);
    // The conversation has no model, so the gateway refused to run it.
    let JobOutcome::Refused(report) = run.outcome else {
        panic!("the run was refused");
    };
    assert_eq!(report.kind, ErrorKind::InvalidRequest);
    assert_eq!(g.run_due_jobs(late).unwrap(), job(&g, created.id).next);
}

#[tokio::test]
async fn one_time_and_paused_jobs_do_not_run_again() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let thread = thread(&g).await;
    let at = now() + YEAR + arbitrary() % YEAR;
    let Reply::Job(once) = schedule(&g, thread, Schedule::At { at }).await else {
        panic!("job scheduled");
    };
    assert_eq!(g.run_due_jobs(at).unwrap(), None);
    let once = job(&g, once.id);
    assert!(!once.enabled && once.next.is_none());
    assert_eq!(once.last.map(|run| run.at), Some(at));
    ended(&g, once.id).await;
    assert_eq!(g.run_due_jobs(at + 1).unwrap(), None);
    assert_eq!(job(&g, once.id).last.map(|run| run.at), Some(at));

    let Reply::Job(repeating) = schedule(&g, thread, every(schedule::MIN_INTERVAL)).await else {
        panic!("job scheduled");
    };
    let due = repeating.next.unwrap();
    let paused = send(
        &g,
        Command::SetJobEnabled {
            id: repeating.id,
            enabled: false,
        },
    )
    .await;
    assert!(matches!(paused, Reply::Job(job) if !job.enabled && job.next.is_none()));
    assert_eq!(g.run_due_jobs(due).unwrap(), None);
    assert_eq!(job(&g, repeating.id).last, None);
    let resumed = send(
        &g,
        Command::SetJobEnabled {
            id: repeating.id,
            enabled: true,
        },
    )
    .await;
    assert!(matches!(resumed, Reply::Job(job) if job.enabled && job.next.is_some()));

    assert_eq!(
        send(&g, Command::RemoveJob { id: repeating.id }).await,
        Reply::Ok
    );
    assert_eq!(
        kind(send(&g, Command::RemoveJob { id: repeating.id }).await),
        ErrorKind::NotFound
    );
}

#[tokio::test]
async fn run_now_leaves_the_schedule_alone() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let thread = thread(&g).await;
    let Reply::Job(created) = schedule(&g, thread, every(schedule::MIN_INTERVAL)).await else {
        panic!("job scheduled");
    };
    let Reply::Job(ran) = send(&g, Command::RunJob { id: created.id }).await else {
        panic!("job ran");
    };
    assert_eq!(ran.next, created.next);
    assert!(matches!(
        ended(&g, created.id).await.outcome,
        JobOutcome::Refused(_)
    ));
}

#[tokio::test]
async fn an_earlier_run_ending_late_leaves_the_latest_result() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let thread = thread(&g).await;
    let Reply::Job(created) = schedule(&g, thread, every(schedule::MIN_INTERVAL)).await else {
        panic!("job scheduled");
    };
    // Two runs in the same second: the first one's result arrives last.
    let at = arbitrary();
    let first = JobRun::start(at);
    let second = JobRun::start(at);
    job_mut(&mut g.lock().unwrap().state, created.id)
        .unwrap()
        .last = Some(second.clone());
    g.finish_job(created.id, first.id, JobOutcome::Failed, false)
        .unwrap();
    assert_eq!(job(&g, created.id).last, Some(second.clone()));
    g.finish_job(created.id, second.id, JobOutcome::Complete, false)
        .unwrap();
    assert_eq!(
        job(&g, created.id).last.map(|run| run.outcome),
        Some(JobOutcome::Complete)
    );
}

#[tokio::test]
async fn relative_schedules_count_from_when_the_gateway_accepts_them() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let thread = thread(&g).await;
    let seconds = schedule::MIN_INTERVAL + arbitrary() % YEAR;
    // An interval with no anchor is anchored at acceptance, so it first
    // runs one interval later, whatever the client's clock says.
    let Reply::Job(repeating) = schedule(
        &g,
        thread,
        Schedule::Every {
            seconds,
            anchor: None,
        },
    )
    .await
    else {
        panic!("interval job scheduled");
    };
    assert_eq!(
        repeating.schedule,
        Schedule::Every {
            seconds,
            anchor: Some(repeating.created)
        }
    );
    assert_eq!(repeating.next, Some(repeating.created + seconds));
    // A delay is stored as the time it ends.
    let Reply::Job(once) = schedule(&g, thread, Schedule::In { seconds }).await else {
        panic!("delayed job scheduled");
    };
    assert_eq!(
        once.schedule,
        Schedule::At {
            at: once.created + seconds
        }
    );
    assert_eq!(once.next, Some(once.created + seconds));
    for unusable in [0, schedule::MAX_INTERVAL + 1] {
        assert_eq!(
            kind(schedule(&g, thread, Schedule::In { seconds: unusable }).await),
            ErrorKind::InvalidRequest
        );
    }
}

#[test]
fn a_heartbeat_is_quiet_by_its_reply_even_when_steering_follows_it() {
    let mut thread = Thread::new(nonce(), Mode::Chat);
    thread.state = RunState::Complete;
    let say = |role, text: &str| Message::new(role, text.into(), 0, CodingBackend::default());
    thread.messages = vec![
        say(Role::User, &nonce()),
        say(
            Role::Assistant,
            &format!("{}\n", a206_core::schedule::HEARTBEAT_OK),
        ),
        // Steered after the run's last round, so the run did not answer it.
        say(Role::User, &nonce()),
    ];
    assert_eq!(super::ended(JobKind::Heartbeat, &thread), JobOutcome::Quiet);
    assert_eq!(
        super::ended(JobKind::Message, &thread),
        JobOutcome::Complete
    );
    thread.messages[1].text = nonce();
    assert_eq!(
        super::ended(JobKind::Heartbeat, &thread),
        JobOutcome::Complete
    );
    thread.state = RunState::Failed;
    assert_eq!(
        super::ended(JobKind::Heartbeat, &thread),
        JobOutcome::Failed
    );
}

fn runs(reply: Reply) -> Vec<JobRun> {
    match reply {
        Reply::JobRuns(runs) => runs,
        other => panic!("expected a job's runs, got {other:?}"),
    }
}

#[tokio::test]
async fn a_jobs_runs_are_kept_newest_first_and_go_with_it() {
    let state = tempfile::tempdir().unwrap();
    let id = {
        let g = Gateway::open(state.path()).unwrap();
        let thread = thread(&g).await;
        let Reply::Job(created) = schedule(&g, thread, every(schedule::MIN_INTERVAL)).await else {
            panic!("job scheduled");
        };
        assert!(runs(send(&g, Command::JobRuns { id: created.id }).await).is_empty());
        let mut started = vec![];
        for _ in 0..3 {
            let Reply::Job(ran) = send(&g, Command::RunJob { id: created.id }).await else {
                panic!("job ran");
            };
            started.push(ran.last.unwrap().id);
            ended(&g, created.id).await;
        }
        let history = runs(send(&g, Command::JobRuns { id: created.id }).await);
        started.reverse();
        assert_eq!(history.iter().map(|r| r.id).collect::<Vec<_>>(), started);
        // This thread has no model, so each run was refused, and ended.
        assert!(
            history
                .iter()
                .all(|r| matches!(r.outcome, JobOutcome::Refused(_))
                    && r.ended.is_some_and(|ended| ended >= r.at))
        );
        created.id
    };
    // The history survives a restart.
    let g = Gateway::open(state.path()).unwrap();
    assert_eq!(runs(send(&g, Command::JobRuns { id }).await).len(), 3);
    assert_eq!(send(&g, Command::RemoveJob { id }).await, Reply::Ok);
    assert_eq!(
        kind(send(&g, Command::JobRuns { id }).await),
        ErrorKind::NotFound
    );
    assert!(g.lock().unwrap().job_runs(id).unwrap().is_empty());
}

#[tokio::test]
async fn the_history_keeps_a_bounded_number_of_runs_and_settles_earlier_ones() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let thread = thread(&g).await;
    let Reply::Job(created) = schedule(&g, thread, every(schedule::MIN_INTERVAL)).await else {
        panic!("job scheduled");
    };
    let at = arbitrary();
    let start = |run: &JobRun| {
        let mut store = g.lock().unwrap();
        job_mut(&mut store.state, created.id).unwrap().last = Some(run.clone());
        store
            .persist(Some(EventKind::JobChanged { id: created.id }))
            .unwrap();
    };
    let first = JobRun::start(at);
    start(&first);
    // A newer run starts before the first one ends.
    let second = JobRun::start(at);
    start(&second);
    let revision = job(&g, created.id).history;
    let to = nonce();
    job_mut(&mut g.lock().unwrap().state, created.id)
        .unwrap()
        .deliver_to = Some(to.clone());
    // A run with a reply to post records its end first, as posting.
    assert_eq!(
        g.finish_job(created.id, first.id, JobOutcome::Complete, true)
            .unwrap(),
        Some(to)
    );
    // Clients showing the history learn that it changed.
    assert_ne!(job(&g, created.id).history, revision);
    let history = g.lock().unwrap().job_runs(created.id).unwrap();
    assert_eq!(history[0], second, "the latest run is still running");
    assert_eq!(history[1].id, first.id);
    assert_eq!(history[1].outcome, JobOutcome::Complete);
    assert_eq!(history[1].delivery, Some(Delivery::Posting));
    assert!(history[1].ended.is_some());
    let posted = Delivery::Sent {
        parts: 1 + arbitrary() % 8,
    };
    g.update_run(created.id, first.id, |_, run| {
        run.delivery = Some(posted.clone())
    })
    .unwrap();
    let history = g.lock().unwrap().job_runs(created.id).unwrap();
    assert_eq!(history[1].delivery, Some(posted));
    // A run that failed, or has no reply, has nothing to post.
    for (outcome, posts) in [(JobOutcome::Failed, true), (JobOutcome::Complete, false)] {
        let run = JobRun::start(at);
        start(&run);
        assert_eq!(
            g.finish_job(created.id, run.id, outcome, posts).unwrap(),
            None
        );
        assert_eq!(job(&g, created.id).last.unwrap().delivery, None);
    }

    let extra = 1 + arbitrary() as usize % 8;
    let mut newest = vec![];
    for _ in 0..a206_core::schedule::MAX_JOB_RUNS + extra {
        let run = JobRun::start(at);
        start(&run);
        newest.push(run.id);
    }
    newest.reverse();
    newest.truncate(a206_core::schedule::MAX_JOB_RUNS);
    let kept: Vec<Id> = runs(send(&g, Command::JobRuns { id: created.id }).await)
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(kept, newest);

    // A run pushed out of the history still posts its reply, while the job
    // keeps a destination; a removed job's does not.
    let pruned = Id::new_v4();
    let to = job(&g, created.id).deliver_to;
    assert!(to.is_some());
    assert_eq!(
        g.finish_job(created.id, pruned, JobOutcome::Complete, true)
            .unwrap(),
        to
    );
    assert_eq!(
        g.finish_job(created.id, pruned, JobOutcome::Failed, true)
            .unwrap(),
        None
    );
    assert_eq!(
        send(&g, Command::RemoveJob { id: created.id }).await,
        Reply::Ok
    );
    assert_eq!(
        g.finish_job(created.id, pruned, JobOutcome::Complete, true)
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn runs_still_going_at_a_restart_end_unrecorded_in_the_history() {
    let state = tempfile::tempdir().unwrap();
    let (id, first, second) = {
        let g = Gateway::open(state.path()).unwrap();
        let thread = thread(&g).await;
        let Reply::Job(created) = schedule(&g, thread, every(schedule::MIN_INTERVAL)).await else {
            panic!("job scheduled");
        };
        let at = arbitrary();
        // The earlier one was posting its reply.
        let first = JobRun {
            delivery: Some(Delivery::Posting),
            ..JobRun::start(at)
        };
        let second = JobRun::start(at);
        for run in [&first, &second] {
            let mut store = g.lock().unwrap();
            job_mut(&mut store.state, created.id).unwrap().last = Some(run.clone());
            store
                .persist(Some(EventKind::JobChanged { id: created.id }))
                .unwrap();
        }
        // Both are still going: the earlier one only in the history.
        let history = g.lock().unwrap().job_runs(created.id).unwrap();
        assert!(history.iter().all(|r| r.outcome == JobOutcome::Running));
        (created.id, first.id, second.id)
    };
    let g = Gateway::open(state.path()).unwrap();
    let history = runs(send(&g, Command::JobRuns { id }).await);
    assert_eq!(history[1].id, first);
    assert_eq!(history[1].outcome, JobOutcome::Unrecorded);
    assert_eq!(history[1].delivery, Some(Delivery::Unrecorded));
    // The latest is settled from its conversation, as `Job::last` is.
    assert_eq!(history[0].id, second);
    assert_eq!(Some(&history[0]), job(&g, id).last.as_ref());
    assert!(!matches!(
        history[0].outcome,
        JobOutcome::Running | JobOutcome::Unrecorded
    ));
}

#[test]
fn a_runs_reply_is_the_last_answer_after_its_own_message() {
    let say = |role, text: &str| Message::new(role, text.into(), 0, CodingBackend::default());
    let (sent, earlier, reply) = (nonce(), nonce(), nonce());
    let mut thread = Thread::new(nonce(), Mode::Chat);
    thread.messages = vec![
        // The same text, answered before this run's message was sent (in
        // the same second, which a time cannot tell apart).
        say(Role::User, &sent),
        say(Role::Assistant, &earlier),
        say(Role::User, &sent),
        say(Role::Assistant, &nonce()),
        // Steered into the run, which answered it.
        say(Role::User, &nonce()),
        say(Role::Assistant, &reply),
        say(Role::Assistant, " \n"),
        // Steered too late for the run, with the job's own text.
        say(Role::User, &sent),
    ];
    assert_eq!(super::run_reply(&thread, &sent, 2), Some(reply));
    // A run that answered with no text has no reply, not an earlier one.
    thread.messages.truncate(3);
    thread.messages.push(say(Role::Assistant, ""));
    assert_eq!(super::run_reply(&thread, &sent, 2), None);
    assert_eq!(super::run_reply(&thread, &nonce(), 0), None);
    assert_eq!(super::run_reply(&thread, &sent, 99), None);
}

#[test]
fn every_kind_that_posts_to_chats_has_a_chat_post_size() {
    for kind in MessengerKind::ALL {
        assert_eq!(
            kind.posts_to_chats(),
            crate::messengers::chat_part_limit(kind).is_some(),
            "{kind:?}"
        );
    }
}

#[tokio::test]
async fn only_configured_post_destinations_are_accepted() {
    let state = tempfile::tempdir().unwrap();
    let g = Gateway::open(state.path()).unwrap();
    let thread = thread(&g).await;
    let Reply::Job(created) = schedule(&g, thread, every(schedule::MIN_INTERVAL)).await else {
        panic!("job scheduled");
    };
    let (hook, bot, irc, chat) = (
        format!("h-{}", nonce()),
        format!("b-{}", nonce()),
        format!("i-{}", nonce()),
        nonce(),
    );
    let mut settings = g.snapshot().unwrap().settings;
    let with_chat = |name: &str, kind| MessengerConfig {
        allowed_chats: [chat.clone()].into(),
        ..MessengerConfig::new(name.to_owned(), kind)
    };
    settings.messengers = vec![
        MessengerConfig::new(hook.clone(), MessengerKind::Webhook),
        with_chat(&bot, MessengerKind::Telegram),
        with_chat(&irc, MessengerKind::Irc),
    ];
    let mut expected = vec![hook.clone(), format!("{bot}/{chat}")];
    expected.sort();
    let mut offered = messengers::post_destinations(&settings.messengers);
    offered.sort();
    assert_eq!(offered, expected);
    g.lock().unwrap().state.settings = settings;
    let deliver = |to: &str| Command::SetJobDelivery {
        id: created.id,
        to: Some(to.to_owned()),
    };
    for to in [hook.clone(), format!("{bot}/{chat}")] {
        let Reply::Job(set) = send(&g, deliver(&to)).await else {
            panic!("{to} accepted");
        };
        assert_eq!(set.deliver_to, Some(to));
    }
    // IRC posts only over its receiving session; a chat must be allowed;
    // a chat messenger is named with its chat.
    for to in [
        format!("{irc}/{chat}"),
        format!("{bot}/{}", nonce()),
        bot.clone(),
        nonce(),
    ] {
        assert_eq!(
            kind(send(&g, deliver(&to)).await),
            ErrorKind::InvalidRequest
        );
    }
    assert_eq!(
        job(&g, created.id).deliver_to,
        Some(format!("{bot}/{chat}"))
    );
    let missing = Command::SetJobDelivery {
        id: Id::new_v4(),
        to: None,
    };
    assert_eq!(kind(send(&g, missing).await), ErrorKind::NotFound);
}
