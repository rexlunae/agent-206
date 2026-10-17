//! The native `plan_update` tool against scripted provider streams: it is
//! denied until the user permits it, a call replaces the conversation's plan
//! and returns it, a malformed call leaves the plan as it was, and the plan
//! survives a gateway restart. Every identifier and text is generated at
//! runtime.
mod support;

use a206_core::*;
use a206_runtime::gateway::Gateway;
use serde_json::{Value, json};
use support::*;
use tokio::sync::mpsc;

const TOOL: &str = "plan_update";

struct Rig {
    gateway: Gateway,
    state: tempfile::TempDir,
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
        state,
        thread: thread.id,
        requests,
    }
}
impl Rig {
    async fn permit(&self) {
        let reply = request(
            &self.gateway,
            Command::SetToolPermission {
                thread: self.thread,
                tool: TOOL.into(),
                permission: Some(ToolPermission::Allow),
            },
        )
        .await;
        assert_eq!(reply, Reply::Ok);
    }
    /// Run one turn whose model makes `call` and then answers.
    async fn turn(&mut self, api: ProviderApi, call: &Call) -> (Thread, Value) {
        let run = send(&self.gateway, self.thread);
        let first = next_request(&mut self.requests).await;
        assert!(
            offered(api, &first.body).contains(&TOOL.to_owned()),
            "{api:?}"
        );
        first
            .respond(calls_response(api, std::slice::from_ref(call), &nonce()))
            .await;
        let second = next_request(&mut self.requests).await;
        let result = result_for(api, &second.body, &call.id).expect("the call's result");
        // A plan the history shows is not repeated in the system prompt.
        assert!(
            !system_of(api, &second.body).contains("# Current plan"),
            "{api:?}"
        );
        second.respond(text_response(api, &nonce())).await;
        let thread = finished(run).await;
        assert_eq!(thread.state, RunState::Complete, "{api:?}");
        (
            thread,
            serde_json::from_str(&result).unwrap_or(Value::String(result)),
        )
    }
}
fn steps() -> Vec<(String, PlanStatus)> {
    PlanStatus::ALL
        .iter()
        .map(|status| (nonce(), *status))
        .collect()
}
fn update(steps: &[(String, PlanStatus)]) -> Call {
    let steps: Vec<_> = steps
        .iter()
        .map(|(text, status)| json!({"text": text, "status": status.name()}))
        .collect();
    Call::new(TOOL, json!({ "steps": steps }))
}
fn plan_of(thread: &Thread) -> Vec<(String, PlanStatus)> {
    thread
        .plan
        .iter()
        .map(|step| (step.text.clone(), step.status))
        .collect()
}

#[tokio::test]
async fn the_tool_is_denied_until_the_user_permits_it() {
    let mut rig = rig(ProviderApi::OpenAi).await;
    let Reply::Tools(list) = request(&rig.gateway, Command::ListTools { thread: rig.thread }).await
    else {
        panic!("tool listing failed");
    };
    let offer = list.tools.iter().find(|t| t.name == TOOL).unwrap();
    assert_eq!(offer.permission, None);
    assert_eq!(offer.effective, ToolPermission::Deny);
    // Not offered, and a call anyway is refused without changing the plan.
    let run = send(&rig.gateway, rig.thread);
    let first = next_request(&mut rig.requests).await;
    let api = ProviderApi::OpenAi;
    assert!(!offered(api, &first.body).contains(&TOOL.to_owned()));
    let call = update(&steps());
    first
        .respond(calls_response(api, std::slice::from_ref(&call), &nonce()))
        .await;
    next_request(&mut rig.requests)
        .await
        .respond(text_response(api, &nonce()))
        .await;
    let thread = finished(run).await;
    let calls = calls_of(&thread);
    assert_eq!(calls[0].state, ToolCallState::Denied);
    assert!(thread.plan.is_empty());
}

#[tokio::test]
async fn a_call_replaces_the_plan_and_returns_it() {
    for api in PROTOCOLS {
        let mut rig = rig(api).await;
        rig.permit().await;
        let first = steps();
        let (thread, result) = rig.turn(api, &update(&first)).await;
        assert_eq!(plan_of(&thread), first, "{api:?}");
        assert_eq!(result["total"], first.len(), "{api:?}");
        assert_eq!(result["done"], 1, "{api:?}");
        assert_eq!(result["plan"][0]["text"], first[0].0.as_str(), "{api:?}");
        // The next call replaces it whole.
        let second = vec![(nonce(), PlanStatus::Pending)];
        let (thread, _) = rig.turn(api, &update(&second)).await;
        assert_eq!(plan_of(&thread), second, "{api:?}");
        // An empty list clears it.
        let (thread, result) = rig.turn(api, &update(&[])).await;
        assert!(thread.plan.is_empty(), "{api:?}");
        assert_eq!(result["total"], 0, "{api:?}");
    }
}

#[tokio::test]
async fn a_malformed_call_fails_and_keeps_the_plan() {
    let api = ProviderApi::Anthropic;
    let mut rig = rig(api).await;
    rig.permit().await;
    let kept = steps();
    rig.turn(api, &update(&kept)).await;
    for bad in [
        json!({"steps": [{"text": nonce(), "status": nonce()}]}),
        json!({"steps": [{"text": "", "status": "done"}]}),
        json!({"steps": nonce()}),
    ] {
        let call = Call::new(TOOL, bad.clone());
        let (thread, _) = rig.turn(api, &call).await;
        let failed = calls_of(&thread).pop().unwrap();
        assert_eq!(failed.state, ToolCallState::Failed, "{bad}");
        assert_eq!(error_kind(&failed), ErrorKind::InvalidRequest, "{bad}");
        assert_eq!(plan_of(&thread), kept, "{bad}");
    }
}

#[tokio::test]
async fn the_plan_survives_a_restart() {
    let api = ProviderApi::Gemini;
    let mut rig = rig(api).await;
    rig.permit().await;
    let kept = steps();
    rig.turn(api, &update(&kept)).await;
    let Rig {
        gateway,
        state,
        thread,
        ..
    } = rig;
    drop(gateway);
    let reopened = Gateway::open(state.path()).unwrap();
    let thread = reopened
        .snapshot()
        .unwrap()
        .threads
        .into_iter()
        .find(|t| t.id == thread)
        .unwrap();
    assert_eq!(plan_of(&thread), kept);
}

#[tokio::test]
async fn a_plan_compaction_hid_returns_in_the_system_prompt() {
    for api in PROTOCOLS {
        let mut rig = rig(api).await;
        rig.permit().await;
        let kept = steps();
        rig.turn(api, &update(&kept)).await;
        // A later turn, so compaction summarizes the plan's turn away.
        let run = send(&rig.gateway, rig.thread);
        next_request(&mut rig.requests)
            .await
            .respond(text_response(api, &nonce()))
            .await;
        finished(run).await;
        let gateway = rig.gateway.clone();
        let thread = rig.thread;
        let compacting =
            tokio::spawn(async move { request(&gateway, Command::CompactThread { thread }).await });
        next_request(&mut rig.requests)
            .await
            .respond(text_response(api, &nonce()))
            .await;
        let compacted = compacting.await.unwrap();
        assert!(
            !matches!(compacted, Reply::Error(_)),
            "{api:?}: {compacted:?}"
        );

        let run = send(&rig.gateway, rig.thread);
        let asked = next_request(&mut rig.requests).await;
        let system = system_of(api, &asked.body);
        let section = system
            .find("# Current plan")
            .unwrap_or_else(|| panic!("{api:?}: {system}"));
        let mut from = section;
        for (text, status) in &kept {
            let at = system[from..]
                .find(&format!("[{}] {text}", status.name()))
                .unwrap_or_else(|| panic!("{api:?}: {system}"));
            from += at;
        }
        asked.respond(text_response(api, &nonce())).await;
        finished(run).await;
    }
}
