//! The native `ask_user` tool against scripted provider streams: a question
//! reaches the conversation, only a matching answer from a client resumes the
//! run, and the model receives the answer text. Every identifier, question and
//! answer is generated at runtime.
mod support;

use a206_core::*;
use a206_runtime::gateway::Gateway;
use serde_json::{Value, json};
use std::time::Duration;
use support::*;
use tokio::sync::mpsc;

const TOOL: &str = "ask_user";

struct Rig {
    gateway: Gateway,
    _state: tempfile::TempDir,
    thread: Id,
    requests: mpsc::Receiver<Opened>,
}
async fn rig(api: ProviderApi) -> Rig {
    let state = tempfile::tempdir().unwrap();
    let gateway = Gateway::open(state.path()).unwrap();
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
            subagent_models: [model].into(),
        },
    )
    .await;
    assert!(matches!(configured, Reply::Thread(_)), "{configured:?}");
    Rig {
        gateway,
        _state: state,
        thread: thread.id,
        requests,
    }
}
impl Rig {
    async fn run(&self, body: Command) -> Reply {
        request(&self.gateway, body).await
    }
    async fn permit(&self) {
        let reply = self
            .run(Command::SetToolPermission {
                thread: self.thread,
                tool: TOOL.into(),
                permission: Some(ToolPermission::Allow),
            })
            .await;
        assert_eq!(reply, Reply::Ok);
    }
    fn thread(&self) -> Thread {
        self.gateway
            .snapshot()
            .unwrap()
            .threads
            .into_iter()
            .find(|t| t.id == self.thread)
            .unwrap()
    }
    async fn pending_prompt(&self) -> UserPrompt {
        tokio::time::timeout(PATIENCE, async {
            loop {
                if let Some(prompt) = self.thread().prompts.first() {
                    return prompt.clone();
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(wait("a question"))
    }
    async fn events(&self) -> Vec<EventKind> {
        let Reply::Events(events) = self.run(Command::Events { after: 0 }).await else {
            panic!("events unavailable");
        };
        events.into_iter().map(|e| e.kind).collect()
    }
    async fn answer(&self, thread: Id, prompt: &UserPrompt, answer: PromptAnswer) -> Reply {
        self.run(Command::AnswerPrompt {
            thread,
            prompt: prompt.id,
            answer,
        })
        .await
    }
}
fn refused(reply: &Reply, kind: ErrorKind) -> bool {
    matches!(reply, Reply::Error(report) if report.kind == kind)
}

#[tokio::test]
async fn the_tool_is_denied_until_the_user_permits_it() {
    let rig = rig(ProviderApi::OpenAi).await;
    let Reply::Tools(list) = rig.run(Command::ListTools { thread: rig.thread }).await else {
        panic!("tool listing failed");
    };
    let offer = list.tools.iter().find(|t| t.name == TOOL).unwrap();
    assert_eq!(offer.permission, None);
    assert_eq!(offer.effective, ToolPermission::Deny);
    assert!(offer.missing.is_empty());
}

#[tokio::test]
async fn only_a_fitting_answer_from_the_asking_conversation_reaches_the_model() {
    for api in PROTOCOLS {
        let mut rig = rig(api).await;
        rig.permit().await;
        let task = send(&rig.gateway, rig.thread);
        let first = next_request(&mut rig.requests).await;
        assert!(
            offered(api, &first.body).contains(&TOOL.to_string()),
            "{api:?}"
        );
        let (question, choices) = (nonce(), [nonce(), nonce()]);
        let call = Call::new(
            TOOL,
            json!({ "question": question, "choices": choices, "allow_other": false }),
        );
        first
            .respond(calls_response(api, std::slice::from_ref(&call), &nonce()))
            .await;

        let prompt = rig.pending_prompt().await;
        assert_eq!(prompt.question, question);
        assert_eq!(prompt.choices, choices);
        assert!(!prompt.other);
        assert_eq!(prompt.call, call.id);
        assert!(rig.events().await.contains(&EventKind::PromptRequested {
            thread: rig.thread,
            prompt: prompt.id,
        }));

        // Another conversation, an answer the question does not accept, or
        // a choice it does not offer leaves the question waiting.
        let elsewhere = rig
            .answer(Id::new_v4(), &prompt, PromptAnswer::Decline)
            .await;
        assert!(refused(&elsewhere, ErrorKind::NotFound), "{elsewhere:?}");
        let text = rig
            .answer(rig.thread, &prompt, PromptAnswer::Text { text: nonce() })
            .await;
        assert!(refused(&text, ErrorKind::InvalidRequest), "{text:?}");
        let missing = rig
            .answer(rig.thread, &prompt, PromptAnswer::Choice { index: 2 })
            .await;
        assert!(refused(&missing, ErrorKind::InvalidRequest), "{missing:?}");
        assert_eq!(rig.thread().prompts.len(), 1);

        assert_eq!(
            rig.answer(rig.thread, &prompt, PromptAnswer::Choice { index: 1 })
                .await,
            Reply::Ok
        );
        let second = next_request(&mut rig.requests).await;
        let output = result_for(api, &second.body, &call.id).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap(),
            json!({ "answer": choices[1] }),
            "{api:?}"
        );
        // The model never sees the question's identity.
        assert!(!second.body.to_string().contains(&prompt.id.to_string()));
        let again = rig.answer(rig.thread, &prompt, PromptAnswer::Decline).await;
        assert!(refused(&again, ErrorKind::NotFound), "{again:?}");
        second.respond(text_response(api, &nonce())).await;

        let thread = finished(task).await;
        assert_eq!(thread.state, RunState::Complete, "{:?}", thread.error);
        assert!(thread.prompts.is_empty());
        assert_eq!(calls_of(&thread)[0].state, ToolCallState::Succeeded);
        assert!(rig.events().await.contains(&EventKind::PromptResolved {
            thread: rig.thread,
            prompt: prompt.id,
            outcome: PromptOutcome::Answered,
        }));
    }
}

#[tokio::test]
async fn free_text_answers_and_declinations_reach_the_model() {
    let api = ProviderApi::Anthropic;
    let mut rig = rig(api).await;
    rig.permit().await;
    let task = send(&rig.gateway, rig.thread);
    let first = next_request(&mut rig.requests).await;
    let ask = Call::new(TOOL, json!({ "question": nonce() }));
    first
        .respond(calls_response(api, std::slice::from_ref(&ask), ""))
        .await;
    let prompt = rig.pending_prompt().await;
    let own = nonce();
    assert_eq!(
        rig.answer(
            rig.thread,
            &prompt,
            PromptAnswer::Text { text: own.clone() }
        )
        .await,
        Reply::Ok
    );
    let second = next_request(&mut rig.requests).await;
    let output = result_for(api, &second.body, &ask.id).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!({ "answer": own })
    );

    let again = Call::new(TOOL, json!({ "question": nonce() }));
    second
        .respond(calls_response(api, std::slice::from_ref(&again), ""))
        .await;
    let declined = rig.pending_prompt().await;
    assert_ne!(declined.id, prompt.id);
    assert_eq!(
        rig.answer(rig.thread, &declined, PromptAnswer::Decline)
            .await,
        Reply::Ok
    );
    let third = next_request(&mut rig.requests).await;
    let output = result_for(api, &third.body, &again.id).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!({ "declined": true })
    );
    third.respond(text_response(api, &nonce())).await;
    assert_eq!(finished(task).await.state, RunState::Complete);
    assert!(rig.events().await.contains(&EventKind::PromptResolved {
        thread: rig.thread,
        prompt: declined.id,
        outcome: PromptOutcome::Declined,
    }));
}

#[tokio::test]
async fn an_invalid_question_returns_an_error_to_the_model() {
    let api = ProviderApi::Gemini;
    let mut rig = rig(api).await;
    rig.permit().await;
    let task = send(&rig.gateway, rig.thread);
    let first = next_request(&mut rig.requests).await;
    let call = Call::new(TOOL, json!({ "question": nonce(), "allow_other": false }));
    first
        .respond(calls_response(api, std::slice::from_ref(&call), &nonce()))
        .await;
    let second = next_request(&mut rig.requests).await;
    assert!(result_for(api, &second.body, &call.id).is_some());
    assert!(rig.thread().prompts.is_empty());
    second.respond(text_response(api, &nonce())).await;
    let thread = finished(task).await;
    let calls = calls_of(&thread);
    assert_eq!(calls[0].state, ToolCallState::Failed);
    assert_eq!(error_kind(&calls[0]), ErrorKind::InvalidRequest);
}

#[tokio::test]
async fn cancelling_the_run_expires_its_question() {
    let api = ProviderApi::OpenAi;
    let mut rig = rig(api).await;
    rig.permit().await;
    let task = send(&rig.gateway, rig.thread);
    let first = next_request(&mut rig.requests).await;
    first
        .respond(calls_response(
            api,
            &[Call::new(TOOL, json!({ "question": nonce() }))],
            "",
        ))
        .await;
    let prompt = rig.pending_prompt().await;
    assert_eq!(
        rig.run(Command::CancelRun { thread: rig.thread }).await,
        Reply::Ok
    );
    let thread = finished(task).await;
    assert_eq!(thread.state, RunState::Cancelled);
    assert!(thread.prompts.is_empty());
    assert!(rig.events().await.contains(&EventKind::PromptResolved {
        thread: rig.thread,
        prompt: prompt.id,
        outcome: PromptOutcome::Expired,
    }));
    let late = rig.answer(rig.thread, &prompt, PromptAnswer::Decline).await;
    assert!(refused(&late, ErrorKind::NotFound), "{late:?}");
}
