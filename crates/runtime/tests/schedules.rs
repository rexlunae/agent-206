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
                anchor: Id::new_v4().as_u128() as u64 % 1_000_000_000,
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
