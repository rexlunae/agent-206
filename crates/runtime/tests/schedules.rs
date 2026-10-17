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
