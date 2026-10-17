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
fn every(seconds: u64) -> Schedule {
    Schedule::Every {
        seconds,
        anchor: now(),
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
        at: now() - 1 - now() % 1000,
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
    let seconds = schedule::MIN_INTERVAL + now() % 1000;
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
    let missed = 2 + now() % 5;
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
    let at = now() + 1 + now() % 1000;
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
