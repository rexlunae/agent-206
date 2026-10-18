//! Retrying native model requests against a scripted provider: only a
//! request that failed before any response arrived is repeated, to the same
//! model, within the policy's attempts, and a retry wait can be cancelled.
//! Every identifier and text is generated at runtime.
mod support;

use a206_core::*;
use a206_runtime::{gateway::Gateway, retry::RetryPolicy};
use std::time::Duration;
use support::*;
use tokio::sync::mpsc;

struct Rig {
    gateway: Gateway,
    _state: tempfile::TempDir,
    thread: Id,
    model: ModelKey,
    requests: mpsc::Receiver<Opened>,
}
/// A policy of `attempts` with waits of `wait`.
fn policy(attempts: u32, wait: Duration) -> RetryPolicy {
    RetryPolicy {
        attempts,
        base: wait,
        max: wait,
        max_retry_after: wait,
    }
}
async fn rig(api: ProviderApi, retry: RetryPolicy) -> Rig {
    let state = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(state.path())
        .unwrap()
        .with_retry_policy(retry);
    let (model, requests) = connect(&gateway, api).await;
    let Reply::Thread(thread) = request(
        &gateway,
        Command::CreateThread {
            title: nonce(),
            mode: Mode::Chat,
        },
    )
    .await
    else {
        panic!("thread creation failed");
    };
    let configured = request(
        &gateway,
        Command::ConfigureThread {
            id: thread.id,
            mode: Mode::Chat,
            model: Some(model.clone()),
            subagent_models: [model.clone()].into(),
        },
    )
    .await;
    assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    Rig {
        gateway,
        _state: state,
        thread: thread.id,
        model,
        requests,
    }
}
impl Rig {
    fn thread(&self) -> Thread {
        self.gateway
            .snapshot()
            .unwrap()
            .threads
            .into_iter()
            .find(|t| t.id == self.thread)
            .unwrap()
    }
    fn send(&self) -> tokio::task::JoinHandle<Reply> {
        send(&self.gateway, self.thread)
    }
    async fn next(&mut self) -> Opened {
        next_request(&mut self.requests).await
    }
    /// No request beyond those already taken.
    fn no_more_requests(&mut self) {
        assert!(self.requests.try_recv().is_err(), "an unexpected request");
    }
}

#[tokio::test]
async fn a_refused_request_is_repeated_to_the_same_model_until_it_succeeds() {
    for api in PROTOCOLS {
        let mut rig = rig(api, policy(3, Duration::ZERO)).await;
        let run = rig.send();
        let first = rig.next().await;
        let body = first.body.clone();
        first.refuse(503, None);
        let second = rig.next().await;
        assert_eq!(second.body, body, "{api:?}");
        if api != ProviderApi::Gemini {
            assert_eq!(second.body["model"], rig.model.model.as_str(), "{api:?}");
        }
        second.refuse(429, Some(0));
        let answer = nonce();
        rig.next().await.respond(text_response(api, &answer)).await;
        let thread = finished(run).await;
        assert_eq!(thread.state, RunState::Complete, "{api:?}");
        let replies: Vec<_> = thread
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .collect();
        assert_eq!(replies.len(), 1, "{api:?}");
        assert_eq!(replies[0].text, answer, "{api:?}");
        // The retry notice does not outlive the round.
        assert_eq!(thread.backend_activity, None, "{api:?}");
        rig.no_more_requests();
    }
}

#[tokio::test]
async fn retries_stop_after_the_policys_attempts() {
    let attempts = 2 + (Id::new_v4().as_u128() % 3) as u32;
    let mut rig = rig(ProviderApi::OpenAi, policy(attempts, Duration::ZERO)).await;
    let run = rig.send();
    for _ in 0..attempts {
        rig.next().await.refuse(502, None);
    }
    let thread = finished(run).await;
    assert_eq!(thread.state, RunState::Failed);
    assert!(thread.error.is_some());
    rig.no_more_requests();
}

#[tokio::test]
async fn other_refusals_are_final() {
    for status in [400, 401, 403, 404, 422] {
        let mut rig = rig(ProviderApi::OpenAi, policy(4, Duration::ZERO)).await;
        let run = rig.send();
        rig.next().await.refuse(status, None);
        let thread = finished(run).await;
        assert_eq!(thread.state, RunState::Failed, "{status}");
        rig.no_more_requests();
    }
}

#[tokio::test]
async fn a_response_that_already_streamed_is_not_repeated() {
    for api in PROTOCOLS {
        let mut rig = rig(api, policy(4, Duration::ZERO)).await;
        let run = rig.send();
        // Text, then the stream ends before the response is complete.
        let partial = nonce();
        let whole = text_response(api, &partial);
        let events: Vec<&str> = whole.split_inclusive("\n\n").collect();
        let at = events.iter().position(|e| e.contains(&partial)).unwrap();
        let cut = events[..=at]
            .concat()
            .replace("\"finishReason\":\"STOP\"", "\"index\":0");
        rig.next().await.respond(cut).await;
        let thread = finished(run).await;
        assert_eq!(thread.state, RunState::Failed, "{api:?}");
        assert!(
            thread.messages.iter().any(|m| m.text.contains(&partial)),
            "{api:?}"
        );
        rig.no_more_requests();
    }
}

#[tokio::test]
async fn a_connection_broken_mid_response_is_not_repeated() {
    let api = ProviderApi::OpenAi;
    let mut rig = rig(api, policy(4, Duration::ZERO)).await;
    let run = rig.send();
    let partial = nonce();
    let whole = text_response(api, &partial);
    let first = whole.split_inclusive("\n\n").next().unwrap().to_owned();
    // Not refused: the response starts streaming.
    let Opened { feed, refusal, .. } = rig.next().await;
    drop(refusal);
    feed.send(first.into_bytes()).await.unwrap();
    // The text reached the conversation before the connection broke.
    tokio::time::timeout(PATIENCE, async {
        while !rig
            .thread()
            .messages
            .iter()
            .any(|m| m.text.contains(&partial))
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the streamed text");
    feed.send(vec![]).await.unwrap();
    let thread = finished(run).await;
    assert_eq!(thread.state, RunState::Failed, "{:?}", thread.error);
    assert!(thread.messages.iter().any(|m| m.text.contains(&partial)));
    rig.no_more_requests();
}

#[tokio::test]
async fn a_connection_broken_before_any_response_is_repeated() {
    let api = ProviderApi::OpenAi;
    let mut rig = rig(api, policy(2, Duration::ZERO)).await;
    let run = rig.send();
    rig.next().await.abort_after(String::new()).await;
    let answer = nonce();
    rig.next().await.respond(text_response(api, &answer)).await;
    let thread = finished(run).await;
    assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
}

#[tokio::test]
async fn a_retry_wait_shows_its_reason_and_can_be_cancelled() {
    // A wait far longer than the test, so only cancellation ends it.
    let mut rig = rig(ProviderApi::OpenAi, policy(2, PATIENCE * 6)).await;
    let run = rig.send();
    rig.next().await.refuse(503, None);
    let activity = tokio::time::timeout(PATIENCE, async {
        loop {
            if let Some(activity) = rig.thread().backend_activity {
                return activity;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a retry notice");
    assert!(
        activity.contains("503") && activity.contains("2 of 2"),
        "{activity}"
    );
    let cancelled = request(&rig.gateway, Command::CancelRun { thread: rig.thread }).await;
    assert!(
        matches!(cancelled, Reply::Ok | Reply::Thread(_)),
        "{cancelled:?}"
    );
    let thread = finished(run).await;
    assert_eq!(thread.state, RunState::Cancelled);
    assert_eq!(thread.backend_activity, None);
    rig.no_more_requests();
}
