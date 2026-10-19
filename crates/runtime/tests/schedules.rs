//! Scheduled messages run as ordinary native runs against a scripted provider.
//! Every identifier, text and time is generated at runtime.
mod support;

use a206_core::*;
use std::time::Duration;
use support::*;

#[tokio::test]
async fn a_due_job_sends_its_text_to_the_conversation_and_records_the_run() {
    let state = tempfile::tempdir().unwrap();
    let gateway = a206_runtime::gateway::Gateway::open(state.path()).unwrap();
    let api = ProviderApi::OpenAi;
    let (model, mut requests) = connect(&gateway, api).await;
    let Reply::Thread(thread) = request(
        &gateway,
        Command::CreateThread {
            title: nonce(),
            mode: Mode::Chat,
        },
    )
    .await
    else {
        panic!("thread created");
    };
    let configured = request(
        &gateway,
        Command::ConfigureThread {
            id: thread.id,
            mode: Mode::Chat,
            model: Some(model),
            subagent_models: Default::default(),
        },
    )
    .await;
    assert!(matches!(configured, Reply::Thread(_)));
    let text = nonce();
    let Reply::Job(job) = request(
        &gateway,
        Command::ScheduleJob {
            kind: a206_core::JobKind::Message,
            name: nonce(),
            thread: thread.id,
            text: text.clone(),
            schedule: Schedule::Every {
                seconds: 3600,
                anchor: Some(Id::new_v4().as_u128() as u64 % 1_000_000_000),
            },
        },
    )
    .await
    else {
        panic!("job scheduled");
    };
    let due = job.next.unwrap();
    gateway.run_due_jobs(due).unwrap();

    let opened = next_request(&mut requests).await;
    assert!(opened.body.to_string().contains(&text));
    let answer = nonce();
    opened.respond(text_response(api, &answer)).await;
    let run = tokio::time::timeout(PATIENCE, async {
        loop {
            let state = gateway.snapshot().unwrap();
            let job = state.jobs.iter().find(|j| j.id == job.id).unwrap();
            match &job.last {
                Some(run) if run.outcome != JobOutcome::Running => return run.clone(),
                _ => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .unwrap_or_else(wait("the job's run to end"));
    assert_eq!(run.at, due);
    assert_eq!(run.outcome, JobOutcome::Complete);
    let state = gateway.snapshot().unwrap();
    let thread = state.threads.iter().find(|t| t.id == thread.id).unwrap();
    assert_eq!(thread.messages.last().unwrap().text, answer);
}

#[tokio::test]
async fn a_model_schedules_a_message_only_after_the_user_approves_that_call() {
    let state = tempfile::tempdir().unwrap();
    let gateway = a206_runtime::gateway::Gateway::open(state.path()).unwrap();
    let api = ProviderApi::OpenAi;
    let (model, mut requests) = connect(&gateway, api).await;
    let Reply::Thread(thread) = request(
        &gateway,
        Command::CreateThread {
            title: nonce(),
            mode: Mode::Chat,
        },
    )
    .await
    else {
        panic!("thread created");
    };
    let configured = request(
        &gateway,
        Command::ConfigureThread {
            id: thread.id,
            mode: Mode::Chat,
            model: Some(model),
            subagent_models: Default::default(),
        },
    )
    .await;
    assert!(matches!(configured, Reply::Thread(_)));
    // Allowing the tool on the conversation still leaves each call to the user.
    let tool = a206_runtime::schedule_tool::ADD;
    let permitted = request(
        &gateway,
        Command::SetToolPermission {
            thread: thread.id,
            tool: tool.into(),
            permission: Some(ToolPermission::Allow),
        },
    )
    .await;
    assert_eq!(permitted, Reply::Ok);

    let task = send(&gateway, thread.id);
    let first = next_request(&mut requests).await;
    assert!(offered(api, &first.body).contains(&tool.to_owned()));
    let (name, text) = (nonce(), nonce());
    let seconds = 60 + Id::new_v4().as_u128() as u64 % 100_000;
    let call = Call::new(
        tool,
        serde_json::json!({"name": name, "message": text, "every_seconds": seconds}),
    );
    first
        .respond(calls_response(api, std::slice::from_ref(&call), &nonce()))
        .await;
    let approval = tokio::time::timeout(PATIENCE, async {
        loop {
            let state = gateway.snapshot().unwrap();
            let thread = state.threads.iter().find(|t| t.id == thread.id).unwrap();
            match thread.approvals.first() {
                Some(approval) => return approval.clone(),
                None => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .unwrap_or_else(wait("the approval request"));
    assert_eq!(approval.tool, tool);
    assert!(approval.arguments.contains(&text));
    assert!(
        gateway.snapshot().unwrap().jobs.is_empty(),
        "scheduled before approval"
    );

    let approved = request(
        &gateway,
        Command::ResolveApproval {
            thread: thread.id,
            approval: approval.id,
            digest: approval.digest.clone(),
            decision: ApprovalDecision::Approve,
        },
    )
    .await;
    assert_eq!(approved, Reply::Ok);
    let second = next_request(&mut requests).await;
    let result = result_for(api, &second.body, &call.id).unwrap();
    second.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);

    let jobs = gateway.snapshot().unwrap().jobs;
    let [job] = jobs.as_slice() else {
        panic!("one job: {jobs:?}");
    };
    assert_eq!(
        (job.thread, &job.name, &job.text),
        (thread.id, &name, &text)
    );
    assert!(matches!(job.schedule, Schedule::Every { seconds: s, .. } if s == seconds));
    assert_eq!(job.author, JobAuthor::Agent { thread: thread.id });
    assert!(job.enabled);
    assert!(result.contains(&job.id.to_string()), "{result}");
}

/// Run `job` now and return its run once it has ended.
async fn run_now(gateway: &a206_runtime::gateway::Gateway, job: Id) -> JobRun {
    let Reply::Job(started) = request(gateway, Command::RunJob { id: job }).await else {
        panic!("job started");
    };
    let run = started.last.unwrap().id;
    tokio::time::timeout(PATIENCE, async {
        loop {
            let state = gateway.snapshot().unwrap();
            let job = state.jobs.iter().find(|j| j.id == job).unwrap();
            match &job.last {
                Some(last) if last.id == run && last.outcome != JobOutcome::Running => {
                    return last.clone();
                }
                _ => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .unwrap_or_else(wait("the heartbeat run to end"))
}

fn refusal(run: &JobRun) -> ErrorKind {
    match &run.outcome {
        JobOutcome::Refused(report) => report.kind,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn a_heartbeat_sends_heartbeat_md_and_a_heartbeat_ok_reply_is_quiet() {
    let state = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let gateway = a206_runtime::gateway::Gateway::open(state.path()).unwrap();
    let api = ProviderApi::OpenAi;
    let (model, mut requests) = connect(&gateway, api).await;
    let Reply::Project(created) = request(
        &gateway,
        Command::CreateProject {
            name: nonce(),
            root: project.path().to_string_lossy().into_owned(),
        },
    )
    .await
    else {
        panic!("project created");
    };
    let thread = created.manager;
    let mode = gateway
        .snapshot()
        .unwrap()
        .threads
        .into_iter()
        .find(|t| t.id == thread)
        .unwrap()
        .mode;
    let configured = request(
        &gateway,
        Command::ConfigureThread {
            id: thread,
            mode,
            model: Some(model),
            subagent_models: Default::default(),
        },
    )
    .await;
    assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    let grant = |capabilities: Vec<Capability>| {
        request(
            &gateway,
            Command::SetBackend {
                thread,
                backend: CodingBackend::Native,
                capabilities: capabilities.into_iter().collect(),
            },
        )
    };
    assert_eq!(grant(vec![Capability::ReadWorkspace]).await, Reply::Ok);
    let schedule = |thread: Id, text: String| {
        request(
            &gateway,
            Command::ScheduleJob {
                name: nonce(),
                thread,
                text,
                schedule: Schedule::Every {
                    seconds: 3600,
                    anchor: Some(Id::new_v4().as_u128() as u64 % 1_000_000_000),
                },
                kind: JobKind::Heartbeat,
            },
        )
    };
    let Reply::Error(report) = schedule(thread, nonce()).await else {
        panic!("a heartbeat with text was accepted");
    };
    assert_eq!(report.kind, ErrorKind::InvalidRequest);
    let Reply::Job(job) = schedule(thread, String::new()).await else {
        panic!("heartbeat scheduled");
    };
    assert_eq!(job.kind, JobKind::Heartbeat);
    let job = *job;
    let id = job.id;
    let messages = |gateway: &a206_runtime::gateway::Gateway| {
        gateway
            .snapshot()
            .unwrap()
            .threads
            .into_iter()
            .find(|t| t.id == thread)
            .unwrap()
            .messages
    };

    // No checklist: refused, and nothing is sent.
    assert_eq!(
        refusal(&run_now(&gateway, job.id).await),
        ErrorKind::NotFound
    );
    // An empty checklist has nothing to check, so nothing is sent either.
    let path = project.path().join("HEARTBEAT.md");
    let limit = a206_runtime::instructions::MAX_FILE;
    for blank in [" \n".to_owned(), " ".repeat(limit)] {
        std::fs::write(&path, blank).unwrap();
        assert_eq!(run_now(&gateway, job.id).await.outcome, JobOutcome::Quiet);
    }
    // A checklist after more blank space than is read is refused, not
    // taken for an empty one.
    std::fs::write(&path, format!("{}- {}", " ".repeat(limit), nonce())).unwrap();
    assert_eq!(
        refusal(&run_now(&gateway, job.id).await),
        ErrorKind::InvalidRequest
    );
    assert!(messages(&gateway).is_empty());
    assert!(requests.try_recv().is_err(), "a model was asked");

    // A checklist is sent; exactly HEARTBEAT_OK means nothing needs attention.
    let item = nonce();
    std::fs::write(&path, format!("- check {item}\n")).unwrap();
    let run = tokio::spawn({
        let gateway = gateway.clone();
        async move { run_now(&gateway, id).await }
    });
    let opened = next_request(&mut requests).await;
    let body = opened.body.to_string();
    assert!(body.contains(&item) && body.contains(schedule::HEARTBEAT_OK));
    opened
        .respond(text_response(
            api,
            &format!(" {} \n", schedule::HEARTBEAT_OK),
        ))
        .await;
    assert_eq!(run.await.unwrap().outcome, JobOutcome::Quiet);
    let sent = &messages(&gateway)[0].text;
    let Some(JobRun { id: run, .. }) = gateway
        .snapshot()
        .unwrap()
        .jobs
        .into_iter()
        .find(|j| j.id == id)
        .unwrap()
        .last
    else {
        panic!("the run was recorded");
    };
    assert_eq!(schedule::check_run(sent), Some(run), "{sent}");

    // Anything else is a reply that needs attention.
    let run = tokio::spawn({
        let gateway = gateway.clone();
        async move { run_now(&gateway, id).await }
    });
    let alert = format!("{} {}", schedule::HEARTBEAT_OK, nonce());
    next_request(&mut requests)
        .await
        .respond(text_response(api, &alert))
        .await;
    assert_eq!(run.await.unwrap().outcome, JobOutcome::Complete);

    // The checklist is read under the conversation's grants when it runs.
    assert_eq!(grant(vec![]).await, Reply::Ok);
    assert_eq!(
        refusal(&run_now(&gateway, job.id).await),
        ErrorKind::PermissionDenied
    );
    // A conversation without a project has no HEARTBEAT.md to read.
    let Reply::Thread(loose) = request(
        &gateway,
        Command::CreateThread {
            title: nonce(),
            mode: Mode::Chat,
        },
    )
    .await
    else {
        panic!("thread created");
    };
    let Reply::Job(loose) = schedule(loose.id, String::new()).await else {
        panic!("heartbeat scheduled");
    };
    assert_eq!(
        refusal(&run_now(&gateway, loose.id).await),
        ErrorKind::NotFound
    );
    assert!(requests.try_recv().is_err(), "a model was asked");
}
